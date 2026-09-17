use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(5);
pub const GRIM_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_CAPTURE_BYTES: usize = 48 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: u8,
    #[serde(default)]
    pub disabled: bool,
}

impl Monitor {
    /// Transformed logical extents in Hyprland layout coordinates.
    /// Transforms 1,3,5,7 are the 90/270-degree variants and swap axes.
    pub fn logical_size(&self) -> (u32, u32) {
        let w = (self.width as f64 / self.scale).round() as u32;
        let h = (self.height as f64 / self.scale).round() as u32;
        match self.transform {
            1 | 3 | 5 | 7 => (h, w),
            _ => (w, h),
        }
    }
}

#[derive(Debug)]
pub enum BackendError {
    Missing(&'static str),
    Failed(String),
    Timeout(&'static str),
    TooLarge(usize),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(what) => write!(f, "missing dependency or session: {what}"),
            Self::Failed(msg) => write!(f, "{msg}"),
            Self::Timeout(what) => write!(f, "{what} timed out"),
            Self::TooLarge(n) => write!(f, "capture exceeded {n} byte limit"),
        }
    }
}

/// XDG_RUNTIME_DIR, or the standard /run/user/<uid> when a client (Codex)
/// spawns us with a scrubbed environment.
fn runtime_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(dir.into());
    }
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let dir = std::path::PathBuf::from(format!("/run/user/{uid}"));
    dir.is_dir().then_some(dir)
}

/// MCP clients (Codex) may spawn this server with a scrubbed environment, so
/// fall back to discovering the live session under the runtime dir.
/// ponytail: single-user session; with multiple hypr instances we take the
/// first one that has an event socket.
fn hyprland_signature() -> Option<std::ffi::OsString> {
    if let Some(his) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        return Some(his);
    }
    let dir = runtime_dir()?.join("hypr");
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    entries
        .into_iter()
        .find(|e| e.path().join(".socket2.sock").exists())
        .map(|e| e.file_name())
}

fn wayland_display() -> Option<std::ffi::OsString> {
    if let Some(wd) = std::env::var_os("WAYLAND_DISPLAY") {
        return Some(wd);
    }
    let dir = runtime_dir()?;
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    entries
        .into_iter()
        .find(|e| e.file_name().to_string_lossy().starts_with("wayland-"))
        .map(|e| e.file_name())
}

fn session_env(cmd: &mut Command) -> &mut Command {
    if let Some(his) = hyprland_signature() {
        cmd.env("HYPRLAND_INSTANCE_SIGNATURE", his);
    }
    if let Some(wd) = wayland_display() {
        cmd.env("WAYLAND_DISPLAY", wd);
    }
    if let Some(dir) = runtime_dir() {
        cmd.env("XDG_RUNTIME_DIR", dir);
    }
    cmd
}

pub async fn monitors() -> Result<Vec<Monitor>, BackendError> {
    let out = run(
        session_env(Command::new("hyprctl").args(["-j", "monitors"])),
        HYPRCTL_TIMEOUT,
        "hyprctl monitors",
    )
    .await?;
    serde_json::from_slice(&out).map_err(|e| BackendError::Failed(format!("hyprctl monitors parse: {e}")))
}

/// Capture one monitor as PNG via `grim -o <name> -t png -`.
pub async fn capture(monitor: &str) -> Result<Vec<u8>, BackendError> {
    run(
        session_env(Command::new("grim").args(["-o", monitor, "-t", "png", "-"])),
        GRIM_TIMEOUT,
        "grim capture",
    )
    .await
}

async fn run(cmd: &mut Command, timeout: Duration, what: &'static str) -> Result<Vec<u8>, BackendError> {
    let child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BackendError::Missing(what)
            } else {
                BackendError::Failed(format!("{what} spawn: {e}"))
            }
        })?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(BackendError::Timeout(what)),
        Ok(Err(e)) => Err(BackendError::Failed(format!("{what}: {e}"))),
        Ok(Ok(out)) if !out.status.success() => Err(BackendError::Failed(format!(
            "{what} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
        Ok(Ok(out)) if out.stdout.len() > MAX_CAPTURE_BYTES => Err(BackendError::TooLarge(MAX_CAPTURE_BYTES)),
        Ok(Ok(out)) => Ok(out.stdout),
    }
}

/// Fingerprint over everything that affects targeting: the sorted set of
/// active monitors with position, logical extents, scale, and transform.
/// Not sufficient alone (change-and-restore); always paired with generation.
pub fn fingerprint(monitors: &[Monitor]) -> u64 {
    let mut parts: Vec<String> = monitors
        .iter()
        .filter(|m| !m.disabled)
        .map(|m| {
            let (w, h) = m.logical_size();
            format!("{}:{},{},{}x{},s{},t{}", m.name, m.x, m.y, w, h, m.scale, m.transform)
        })
        .collect();
    parts.sort();
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

/// hyprctl -j monitors reports disabled outputs too; keep only selectable ones.
pub fn selectable(monitors: Vec<Monitor>) -> Vec<Monitor> {
    monitors.into_iter().filter(|m| !m.disabled).collect()
}

/// Watches Hyprland's event socket; bumps `generation` on any display-relevant
/// event and on every disconnect/reconnect (the socket has no replay cursor,
/// so a gap means we may have missed a change-and-restore).
pub fn watch_events(generation: Arc<AtomicU64>, healthy: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match event_socket_path() {
                Some(path) => match tokio::net::UnixStream::connect(&path).await {
                    Ok(stream) => {
                        healthy.store(true, Ordering::SeqCst);
                        let mut lines = BufReader::new(stream).lines();
                        loop {
                            match lines.next_line().await {
                                Ok(Some(line)) => {
                                    let event = line.split('>').next().unwrap_or("");
                                    if matches!(event, "configreloaded")
                                        || event.starts_with("monitoradded")
                                        || event.starts_with("monitorremoved")
                                    {
                                        generation.fetch_add(1, Ordering::SeqCst);
                                    }
                                }
                                Ok(None) | Err(_) => break,
                            }
                        }
                    }
                    Err(_) => healthy.store(false, Ordering::SeqCst),
                },
                None => healthy.store(false, Ordering::SeqCst),
            }
            // Disconnect, error, or missing socket: invalidate everything and retry.
            healthy.store(false, Ordering::SeqCst);
            generation.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

fn event_socket_path() -> Option<std::path::PathBuf> {
    let his = hyprland_signature()?;
    let runtime = runtime_dir()?;
    Some(std::path::Path::new(&runtime).join("hypr").join(his).join(".socket2.sock"))
}

/// PNG IHDR dimensions, or None if not a PNG.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() >= 24 && png[..8] == [137, 80, 78, 71, 13, 10, 26, 10] && &png[12..16] == b"IHDR" {
        Some((
            u32::from_be_bytes(png[16..20].try_into().ok()?),
            u32::from_be_bytes(png[20..24].try_into().ok()?),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: i32, y: i32, w: u32, h: u32, scale: f64, t: u8) -> Monitor {
        Monitor {
            name: name.into(),
            description: String::new(),
            x,
            y,
            width: w,
            height: h,
            scale,
            transform: t,
            disabled: false,
        }
    }

    #[test]
    fn logical_size_normal_and_fractional() {
        assert_eq!(mon("a", 0, 0, 2560, 1440, 1.25, 0).logical_size(), (2048, 1152));
        assert_eq!(mon("a", 0, 0, 3840, 2160, 2.0, 0).logical_size(), (1920, 1080));
    }

    #[test]
    fn logical_size_swaps_axes_for_90_270_transforms() {
        for t in [1u8, 3, 5, 7] {
            assert_eq!(mon("a", 0, 0, 2560, 1440, 1.0, t).logical_size(), (1440, 2560));
        }
        for t in [0u8, 2, 4, 6] {
            assert_eq!(mon("a", 0, 0, 2560, 1440, 1.0, t).logical_size(), (2560, 1440));
        }
    }

    #[test]
    fn fingerprint_tracks_mapping_relevant_facts() {
        let base = vec![mon("eDP-1", 0, 0, 2560, 1440, 1.25, 0)];
        let same = vec![mon("eDP-1", 0, 0, 2560, 1440, 1.25, 0)];
        assert_eq!(fingerprint(&base), fingerprint(&same));
        for altered in [
            vec![mon("eDP-1", 0, 0, 2560, 1440, 1.5, 0)],   // scale
            vec![mon("eDP-1", -2048, 0, 2560, 1440, 1.25, 0)], // position
            vec![mon("eDP-1", 0, 0, 2560, 1440, 1.25, 1)], // transform
            vec![mon("DP-1", 0, 0, 2560, 1440, 1.25, 0)],  // identity
        ] {
            assert_ne!(fingerprint(&base), fingerprint(&altered));
        }
        let mut disabled_changed = base.clone();
        disabled_changed[0].disabled = true;
        assert_ne!(fingerprint(&base), fingerprint(&disabled_changed));
    }

    #[test]
    fn png_size_reads_ihdr() {
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((640, 480)));
        assert_eq!(png_size(b"not a png"), None);
    }
}
