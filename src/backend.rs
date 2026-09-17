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
    #[serde(default)]
    pub id: i64,
}

impl Monitor {
    fn id_or_name_matches(&self, want: i64) -> bool {
        self.id == want
    }
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
    serde_json::from_slice(&out)
        .map_err(|e| BackendError::Failed(format!("hyprctl monitors parse: {e}")))
}

/// Name of the monitor that owns the currently focused window, if any.
pub async fn focused_monitor() -> Result<Option<String>, BackendError> {
    let out = run(
        session_env(Command::new("hyprctl").args(["-j", "activewindow"])),
        HYPRCTL_TIMEOUT,
        "hyprctl activewindow",
    )
    .await?;
    if out.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None); // empty output: nothing focused
    }
    let v: serde_json::Value = serde_json::from_slice(&out)
        .map_err(|e| BackendError::Failed(format!("activewindow parse: {e}")))?;
    // activewindow reports a monitor id; map it via the monitor list.
    let Some(want) = v.get("monitor").and_then(|m| m.as_i64()) else {
        return Ok(None);
    };
    let monitors = monitors().await?;
    Ok(monitors
        .into_iter()
        .find(|m| m.id_or_name_matches(want))
        .map(|m| m.name))
}

/// Replace the clipboard with the exact UTF-8 text (stdin, never shell).
pub async fn clipboard_set(text: &str) -> Result<(), BackendError> {
    let mut child =
        session_env(Command::new("wl-copy").args(["--type", "text/plain;charset=utf-8"]))
            .stdin(std::process::Stdio::piped())
            // wl-copy forks a daemon to serve the selection; it must not
            // inherit our stdio or it keeps the MCP pipes open forever.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    BackendError::Missing("wl-copy")
                } else {
                    BackendError::Failed(format!("wl-copy spawn: {e}"))
                }
            })?;
    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().unwrap();
    let write = async move {
        stdin.write_all(text.as_bytes()).await?;
        stdin.shutdown().await
    };
    let (wres, cres) = tokio::join!(write, tokio::time::timeout(HYPRCTL_TIMEOUT, child.wait()));
    wres.map_err(|e| BackendError::Failed(format!("wl-copy stdin: {e}")))?;
    match cres {
        Err(_) => Err(BackendError::Timeout("wl-copy")),
        Ok(Err(e)) => Err(BackendError::Failed(format!("wl-copy: {e}"))),
        Ok(Ok(s)) if !s.success() => Err(BackendError::Failed(format!("wl-copy exited {s}"))),
        Ok(Ok(_)) => Ok(()),
    }
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

async fn run(
    cmd: &mut Command,
    timeout: Duration,
    what: &'static str,
) -> Result<Vec<u8>, BackendError> {
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
        Ok(Ok(out)) if out.stdout.len() > MAX_CAPTURE_BYTES => {
            Err(BackendError::TooLarge(MAX_CAPTURE_BYTES))
        }
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
            format!(
                "{}:{},{},{}x{},s{},t{}",
                m.name, m.x, m.y, w, h, m.scale, m.transform
            )
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
pub fn watch_events(
    generation: Arc<AtomicU64>,
    healthy: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
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
    // Diagnostic/test seam: point the watcher at a controlled socket to
    // exercise disconnect/reconnect without touching the real compositor.
    if let Some(p) = std::env::var_os("COMPUTER_USE_EVENT_SOCKET") {
        return Some(std::path::PathBuf::from(p));
    }
    let his = hyprland_signature()?;
    let runtime = runtime_dir()?;
    Some(runtime.join("hypr").join(his).join(".socket2.sock"))
}

/// Hyprland's IPC socket does NOT emit an event for every display change:
/// verified on 0.56.2 that `hl.monitor({scale=...})` changes the fingerprint
/// with no monitoradded/removed/configreloaded event. The spec therefore
/// requires a compositor-level output notification channel: a wl_output
/// listener bumps the generation on every output event, which catches
/// scale/mode/geometry/transform reconfigures the IPC socket misses
/// (including change-and-restore, since the listener is continuously
/// connected and sees both halves).
pub fn watch_wayland_outputs(
    generation: Arc<AtomicU64>,
    healthy: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use wayland_client::protocol::{wl_output, wl_registry};
    use wayland_client::{Connection, Dispatch, QueueHandle};

    struct Watcher {
        generation: Arc<AtomicU64>,
    }

    impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for Watcher {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &wayland_client::globals::GlobalListContents,
            _: &Connection,
            qhandle: &QueueHandle<Self>,
        ) {
            // Only post-initial-roundtrip events reach us; initial globals are
            // bound from GlobalList::contents() below.
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == "wl_output" {
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qhandle, ());
                }
            }
            // Any global appearing/disappearing may be a display change.
            state.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Dispatch<wl_output::WlOutput, ()> for Watcher {
        fn event(
            state: &mut Self,
            _: &wl_output::WlOutput,
            _: wl_output::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            state.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    std::thread::spawn(move || {
        loop {
            let Some(path) = wayland_socket_path() else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            let run = || -> Result<(), Box<dyn std::error::Error>> {
                let stream = std::os::unix::net::UnixStream::connect(path)?;
                let backend = wayland_client::backend::Backend::connect(stream)?;
                let conn = Connection::from_backend(backend);
                let (globals, mut queue) =
                    wayland_client::globals::registry_queue_init::<Watcher>(&conn)?;
                let mut watcher = Watcher {
                    generation: generation.clone(),
                };
                // Bind every currently advertised output; new ones are bound via
                // the forwarded registry events above.
                for global in globals.contents().clone_list() {
                    if global.interface == "wl_output" {
                        globals.registry().bind::<wl_output::WlOutput, _, _>(
                            global.name,
                            global.version.min(4),
                            &queue.handle(),
                            (),
                        );
                    }
                }
                queue.roundtrip(&mut watcher)?;
                healthy.store(true, Ordering::SeqCst);
                loop {
                    queue.blocking_dispatch(&mut watcher)?;
                }
            };
            if let Err(e) = run() {
                eprintln!("wayland output watcher disconnected: {e}");
            }
            // Connection lost: we may have missed a change-and-restore.
            healthy.store(false, Ordering::SeqCst);
            generation.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(1));
        }
    })
}

fn wayland_socket_path() -> Option<std::path::PathBuf> {
    Some(runtime_dir()?.join(wayland_display()?))
}

// ---------------------------------------------------------------------------
// Persistent virtual pointer (zwlr_virtual_pointer_v1) + coordinate mapping.
// ---------------------------------------------------------------------------

/// evdev button codes used by Hyprland dispatchers and the pointer protocol.
pub const BTN_LEFT: u32 = 272;
pub const BTN_RIGHT: u32 = 273;

/// Bounding rectangle of the complete logical monitor layout — the absolute
/// frame an output-unmapped virtual pointer maps onto (Hyprland 0.56.2
/// `warpAbsolute`: normalized position inside this box).
pub struct LayoutBox {
    pub min_x: i32,
    pub min_y: i32,
    pub x_extent: u32,
    pub y_extent: u32,
}

pub fn layout_box(monitors: &[Monitor]) -> Option<LayoutBox> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for m in monitors.iter().filter(|m| !m.disabled) {
        let (w, h) = m.logical_size();
        min_x = min_x.min(m.x);
        min_y = min_y.min(m.y);
        max_x = max_x.max(m.x + w as i32);
        max_y = max_y.max(m.y + h as i32);
    }
    (max_x > min_x && max_y > min_y).then(|| LayoutBox {
        min_x,
        min_y,
        x_extent: (max_x - min_x) as u32,
        y_extent: (max_y - min_y) as u32,
    })
}

/// Map a point in an observation image into the pointer's absolute frame.
/// The image is already upright; it maps linearly onto the monitor's oriented
/// logical rectangle, which is then offset into the layout bounding box.
/// Returns (x, y) in the box's unsigned coordinate space, or None for
/// out-of-bounds/non-finite input. Never clamps a wrong point.
pub fn map_point(
    px: f64,
    py: f64,
    image_w: u32,
    image_h: u32,
    monitor: &Monitor,
    layout: &LayoutBox,
) -> Option<(u32, u32)> {
    if !px.is_finite() || !py.is_finite() || px < 0.0 || py < 0.0 {
        return None;
    }
    if px >= image_w as f64 || py >= image_h as f64 || image_w == 0 || image_h == 0 {
        return None;
    }
    let (lw, lh) = monitor.logical_size();
    let gx = monitor.x as f64 + px * lw as f64 / image_w as f64;
    let gy = monitor.y as f64 + py * lh as f64 / image_h as f64;
    let ax = (gx - layout.min_x as f64).round();
    let ay = (gy - layout.min_y as f64).round();
    // abs coords are in [0, extent); rounding an edge pixel can reach extent.
    let x = ax.clamp(0.0, (layout.x_extent - 1) as f64) as u32;
    let y = ay.clamp(0.0, (layout.y_extent - 1) as f64) as u32;
    Some((x, y))
}

#[derive(Debug)]
pub enum PointerOp {
    /// Absolute position + frame extents for this dispatch.
    Move {
        x: u32,
        y: u32,
        x_extent: u32,
        y_extent: u32,
    },
    Button {
        code: u32,
        pressed: bool,
    },
    /// Signed wheel steps; axis 0 = vertical, 1 = horizontal.
    Scroll {
        axis: u8,
        steps: i32,
    },
    /// Pause between events so clients see a drag gesture, not a jump.
    Wait {
        ms: u32,
    },
    Frame,
}

/// How far the compositor got, for the caller-visible `effect` field.
#[derive(Debug, PartialEq)]
pub enum Delivery {
    None,
    Completed,
    /// Some ops dispatched, then transport failed.
    Partial,
    /// All ops dispatched but the sync roundtrip failed.
    Unknown,
}

/// Mid-action abort signal: (live generation counter, value the action was
/// validated against). Sessions check it between ops; a mismatch stops the
/// remaining intended input, releases held state, and reports Partial.
pub type Guard = Option<(Arc<AtomicU64>, u64)>;

pub struct Pointer {
    tx: std::sync::mpsc::Sender<
        (
            Vec<PointerOp>,
            Guard,
            std::sync::mpsc::Sender<Delivery>,
        ),
    >,
}

impl Pointer {
    /// Spawns the pointer thread. Connection/protocol init happens lazily per
    /// request and is retried after failures; the pointer object persists so
    /// button state survives across calls (required for drags later).
    pub fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(
            Vec<PointerOp>,
            Guard,
            std::sync::mpsc::Sender<Delivery>,
        )>();
        std::thread::spawn(move || {
            let mut session: Option<pointer_session::Session> = None;
            for (ops, guard, reply) in rx {
                if session.is_none() {
                    session = pointer_session::Session::connect().ok();
                }
                let result = match session.as_mut() {
                    None => Delivery::None,
                    Some(s) => s.apply(&ops, &guard),
                };
                let _ = reply.send(result);
                if session.as_ref().is_some_and(|s| s.broken) {
                    session = None;
                }
            }
        });
        Self { tx }
    }

    /// Blocks until the op list is delivered or fails. Call from spawn_blocking.
    /// `guard` is (generation counter, expected value): when the generation
    /// has moved on (display change mid-action), remaining ops are skipped and
    /// held inputs released before returning Partial.
    pub fn apply(&self, ops: Vec<PointerOp>, guard: Guard) -> Delivery {
        let (reply, rx) = std::sync::mpsc::channel();
        if self.tx.send((ops, guard, reply)).is_err() {
            return Delivery::None;
        }
        // Bound the wait: a wedged compositor must not hang every later action.
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or(Delivery::Unknown)
    }
}

mod pointer_session {
    use super::{Delivery, Guard, PointerOp};
    use std::sync::atomic::Ordering;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_output, wl_pointer, wl_registry};
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

    pub struct Session {
        conn: Connection,
        pointer: ZwlrVirtualPointerV1,
        held: std::collections::HashSet<u32>,
        time_ms: u32,
        pub broken: bool,
    }

    impl Drop for Session {
        /// A dropped session (replaced after failure or thread exit) attempts
        /// one final release of held buttons before teardown.
        fn drop(&mut self) {
            if !self.held.is_empty() {
                self.release_all();
            }
        }
    }

    struct State;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZwlrVirtualPointerManagerV1,
            _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZwlrVirtualPointerV1,
            _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<wl_output::WlOutput, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_output::WlOutput,
            _: wl_output::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Session {
        pub fn connect() -> Result<Self, Box<dyn std::error::Error>> {
            let path = super::wayland_socket_path().ok_or("no wayland session")?;
            let stream = std::os::unix::net::UnixStream::connect(path)?;
            let backend = wayland_client::backend::Backend::connect(stream)?;
            let conn = Connection::from_backend(backend);
            let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
            let mut state = State;
            queue.roundtrip(&mut state)?;
            let mut manager = None;
            for g in globals.contents().clone_list() {
                if g.interface == "zwlr_virtual_pointer_manager_v1" {
                    manager = Some(
                        globals
                            .registry()
                            .bind::<ZwlrVirtualPointerManagerV1, _, _>(
                                g.name,
                                g.version.min(2),
                                &queue.handle(),
                                (),
                            ),
                    );
                }
            }
            let manager = manager.ok_or("zwlr_virtual_pointer_manager_v1 not advertised")?;
            // v2: no seat/output mapping -> pointer covers the whole layout.
            let pointer =
                manager.create_virtual_pointer_with_output(None, None, &queue.handle(), ());
            queue.roundtrip(&mut state)?;
            Ok(Self {
                conn,
                pointer,
                held: Default::default(),
                time_ms: 0,
                broken: false,
            })
        }

        fn tick(&mut self) -> u32 {
            self.time_ms = self.time_ms.wrapping_add(1);
            self.time_ms
        }

        /// Best-effort release of every held button, ignoring further errors.
        fn release_all(&mut self) {
            for code in self.held.drain().collect::<Vec<_>>() {
                let t = self.tick();
                self.pointer
                    .button(t, code, wl_pointer::ButtonState::Released);
            }
            self.pointer.frame();
            let _ = self.conn.flush();
        }

        pub fn apply(
            &mut self,
            ops: &[PointerOp],
            guard: &Guard,
        ) -> Delivery {
            let mut sent = false;
            for op in ops {
                // A display change mid-sequence invalidates every further
                // intended coordinate: stop here and release what is held.
                if let Some((g, expected)) = guard {
                    if g.load(Ordering::SeqCst) != *expected {
                        self.release_all();
                        return if sent {
                            Delivery::Partial
                        } else {
                            Delivery::None
                        };
                    }
                }
                let t = self.tick();
                match *op {
                    PointerOp::Move {
                        x,
                        y,
                        x_extent,
                        y_extent,
                    } => {
                        self.pointer.motion_absolute(t, x, y, x_extent, y_extent);
                    }
                    PointerOp::Button { code, pressed } => {
                        self.pointer.button(
                            t,
                            code,
                            if pressed {
                                wl_pointer::ButtonState::Pressed
                            } else {
                                wl_pointer::ButtonState::Released
                            },
                        );
                        if pressed {
                            self.held.insert(code);
                        } else {
                            self.held.remove(&code);
                        }
                    }
                    PointerOp::Scroll { axis, steps } => {
                        let axis_e = match axis {
                            1 => wl_pointer::Axis::HorizontalScroll,
                            _ => wl_pointer::Axis::VerticalScroll,
                        };
                        // Wheel semantics: discrete steps are authoritative;
                        // the smooth axis carries the equivalent 120/step
                        // value. Clients honoring both pick discrete.
                        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                        self.pointer
                            .axis_discrete(t, axis_e, steps as f64 * 120.0, steps);
                        self.pointer.axis(t, axis_e, steps as f64 * 120.0);
                    }
                    PointerOp::Wait { ms } => {
                        // Flush before sleeping so earlier events are already
                        // on the wire; caps keep one action bounded. Advance
                        // the event clock by the slept time so toolkits see
                        // real gesture timing, not a jump.
                        let _ = self.conn.flush();
                        let ms = ms.min(2000);
                        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
                        self.time_ms = self.time_ms.wrapping_add(ms);
                    }
                    PointerOp::Frame => self.pointer.frame(),
                }
                sent = true;
            }
            if self.conn.flush().is_err() {
                // Best-effort release of held buttons before dropping the
                // pointer; a held button without release can damage
                // compositor pointer state.
                self.release_all();
                self.broken = true;
                return if sent {
                    Delivery::Partial
                } else {
                    Delivery::None
                };
            }
            // Sync the connection: surfaces transport errors (e.g. protocol
            // error, disconnect) instead of assuming the queue was accepted.
            match self.conn.roundtrip() {
                Ok(_) => Delivery::Completed,
                Err(e) => {
                    eprintln!("pointer roundtrip failed: {e}");
                    self.release_all();
                    self.broken = true;
                    Delivery::Unknown
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent virtual keyboard (zwp_virtual_keyboard_v1).
//
// Hyprland 0.56.2's Lua dispatchers (hl.dsp.send_key_state / send_shortcut)
// return "ok" but deliver no key events to Wayland-native clients — verified
// live against gnome-text-editor. The compositor DOES expose
// zwlr_virtual_keyboard_manager_v1, the same mechanism wtype uses, so keys go
// through a real virtual input device instead.
// ---------------------------------------------------------------------------

/// One keyboard operation. `Key` presses/releases a keysym name resolved
/// against the uploaded keymap ("Return", "a"). `Mods` sets the depressed
/// modifier mask from xkb modifier names ("Control", "Shift", "Mod1", "Mod4") —
/// the client only sees modifiers via the dedicated `modifiers` event, so
/// pressing a modifier *key* alone does nothing.
#[derive(Debug)]
pub enum KeyOp {
    Key { name: String, pressed: bool },
    Mods { names: Vec<String> },
}

/// apply() result: Err(name) means `name` resolved to no keycode — nothing was
/// sent, treat as a caller error rather than a backend failure.
pub struct Keyboard {
    tx: std::sync::mpsc::Sender<(
        Vec<KeyOp>,
        Guard,
        std::sync::mpsc::Sender<Result<Delivery, String>>,
    )>,
}

impl Keyboard {
    pub fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(
            Vec<KeyOp>,
            Guard,
            std::sync::mpsc::Sender<Result<Delivery, String>>,
        )>();
        std::thread::spawn(move || {
            let mut session: Option<keyboard_session::Session> = None;
            for (ops, guard, reply) in rx {
                if session.is_none() {
                    session = keyboard_session::Session::connect().ok();
                }
                let result = match session.as_mut() {
                    None => Ok(Delivery::None),
                    Some(s) => s.apply(&ops, &guard),
                };
                let _ = reply.send(result);
                if session.as_ref().is_some_and(|s| s.broken) {
                    session = None;
                }
            }
        });
        Self { tx }
    }

    /// Blocks until the key list is delivered or fails. Call from
    /// spawn_blocking. `guard` aborts remaining ops (with releases) when the
    /// display generation has moved on mid-sequence.
    pub fn apply(
        &self,
        ops: Vec<KeyOp>,
        guard: Guard,
    ) -> Result<Delivery, String> {
        let (reply, rx) = std::sync::mpsc::channel();
        if self.tx.send((ops, guard, reply)).is_err() {
            return Ok(Delivery::None);
        }
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or(Ok(Delivery::Unknown))
    }
}

mod keyboard_session {
    use super::{Delivery, Guard, KeyOp};
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;
    use std::io::Write;
    use std::os::fd::AsFd;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
    use xkbcommon::xkb;

    pub struct Session {
        conn: Connection,
        keyboard: ZwpVirtualKeyboardV1,
        keymap: xkb::Keymap,
        held: HashSet<u32>,
        mods_depressed: u32,
        time_ms: u32,
        pub broken: bool,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if !self.held.is_empty() || self.mods_depressed != 0 {
                self.release_all();
            }
        }
    }

    struct State;

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<wl_seat::WlSeat, ()> for State {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZwpVirtualKeyboardManagerV1,
            _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &ZwpVirtualKeyboardV1,
            _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Session {
        pub fn connect() -> Result<Self, Box<dyn std::error::Error>> {
            let path = super::wayland_socket_path().ok_or("no wayland session")?;
            let stream = std::os::unix::net::UnixStream::connect(path)?;
            let backend = wayland_client::backend::Backend::connect(stream)?;
            let conn = Connection::from_backend(backend);
            let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
            let mut state = State;
            queue.roundtrip(&mut state)?;
            let mut manager = None;
            let mut seat = None;
            for g in globals.contents().clone_list() {
                if g.interface == "zwp_virtual_keyboard_manager_v1" {
                    manager = Some(
                        globals
                            .registry()
                            .bind::<ZwpVirtualKeyboardManagerV1, _, _>(
                                g.name,
                                g.version.min(1),
                                &queue.handle(),
                                (),
                            ),
                    );
                } else if g.interface == "wl_seat" && seat.is_none() {
                    seat = Some(globals.registry().bind::<wl_seat::WlSeat, _, _>(
                        g.name,
                        g.version.min(1),
                        &queue.handle(),
                        (),
                    ));
                }
            }
            let manager = manager.ok_or("zwp_virtual_keyboard_manager_v1 not advertised")?;
            let seat = seat.ok_or("no wl_seat advertised")?;
            let keyboard = manager.create_virtual_keyboard(&seat, &queue.handle(), ());

            // Upload a keymap before any key event (protocol requirement).
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let keymap = xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                "",
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .ok_or("xkb keymap compile failed")?;
            let text = keymap.get_as_string(xkb::FORMAT_TEXT_V1);
            let memfd = memfd::MemfdOptions::default()
                .close_on_exec(true)
                .create("computer-use-keymap")?;
            memfd.as_file().write_all(text.as_bytes())?;
            let fd = memfd.into_file();
            keyboard.keymap(
                wl_keyboard::KeymapFormat::XkbV1.into(),
                fd.as_fd(),
                text.len() as u32,
            );
            queue.roundtrip(&mut state)?;
            Ok(Self {
                conn,
                keyboard,
                keymap,
                held: Default::default(),
                mods_depressed: 0,
                time_ms: 0,
                broken: false,
            })
        }

        fn tick(&mut self) -> u32 {
            self.time_ms = self.time_ms.wrapping_add(1);
            self.time_ms
        }

        /// Keysym name -> evdev keycode. xkb keycodes are evdev+8; the
        /// virtual-keyboard `key` request takes evdev codes, so subtract 8.
        /// Accepts the usual keysym spellings plus lowercase/capitalized
        /// fallbacks.
        fn keycode_for(&self, name: &str) -> Option<u32> {
            let sym = [name, &name.to_lowercase(), &capitalize(name)]
                .into_iter()
                .map(|n| xkb::keysym_from_name(n, xkb::KEYSYM_NO_FLAGS))
                .find(|s| s.raw() != xkb::keysyms::KEY_NoSymbol)?;
            let min: u32 = self.keymap.min_keycode().into();
            let max: u32 = self.keymap.max_keycode().into();
            for raw in min..=max {
                if self
                    .keymap
                    .key_get_syms_by_level(xkb::Keycode::new(raw), 0, 0)
                    .contains(&sym)
                {
                    return raw.checked_sub(8);
                }
            }
            None
        }

        /// xkb modifier name -> depressed-mask bit on the uploaded keymap.
        fn mod_bit(&self, name: &str) -> Option<u32> {
            let idx = self.keymap.mod_get_index(name);
            (idx != xkb::MOD_INVALID).then(|| 1 << idx)
        }

        /// Best-effort release of every held key and depressed modifier,
        /// ignoring further errors.
        fn release_all(&mut self) {
            for code in self.held.drain().collect::<Vec<_>>() {
                let t = self.tick();
                self.keyboard
                    .key(t, code, wl_keyboard::KeyState::Released.into());
            }
            self.mods_depressed = 0;
            self.keyboard.modifiers(0, 0, 0, 0);
            let _ = self.conn.flush();
        }

        pub fn apply(
            &mut self,
            ops: &[KeyOp],
            guard: &Guard,
        ) -> Result<Delivery, String> {
            // Resolve every name first so an unknown key/modifier rejects the
            // whole batch before any event is emitted.
            enum ResolvedOp {
                Key { code: u32, pressed: bool },
                Mods { mask: u32 },
            }
            let mut events = Vec::with_capacity(ops.len());
            for op in ops {
                match op {
                    KeyOp::Key { name, pressed } => {
                        let Some(code) = self.keycode_for(name) else {
                            return Err(format!("unknown key name {name:?}"));
                        };
                        events.push(ResolvedOp::Key {
                            code,
                            pressed: *pressed,
                        });
                    }
                    KeyOp::Mods { names } => {
                        let mut mask = 0u32;
                        for n in names {
                            let Some(bit) = self.mod_bit(n) else {
                                return Err(format!("unknown modifier {n:?}"));
                            };
                            mask |= bit;
                        }
                        events.push(ResolvedOp::Mods { mask });
                    }
                }
            }
            let mut sent = false;
            for ev in events {
                // Display change mid-sequence: stop and release what is held.
                if let Some((g, expected)) = guard {
                    if g.load(Ordering::SeqCst) != *expected {
                        self.release_all();
                        return Ok(if sent {
                            Delivery::Partial
                        } else {
                            Delivery::None
                        });
                    }
                }
                match ev {
                    ResolvedOp::Key { code, pressed } => {
                        let t = self.tick();
                        self.keyboard.key(
                            t,
                            code,
                            if pressed {
                                wl_keyboard::KeyState::Pressed
                            } else {
                                wl_keyboard::KeyState::Released
                            }
                            .into(),
                        );
                        if pressed {
                            self.held.insert(code);
                        } else {
                            self.held.remove(&code);
                        }
                    }
                    ResolvedOp::Mods { mask } => {
                        self.mods_depressed = mask;
                        self.keyboard.modifiers(mask, 0, 0, 0);
                    }
                }
                sent = true;
            }
            if self.conn.flush().is_err() {
                self.release_all();
                self.broken = true;
                return Ok(if sent {
                    Delivery::Partial
                } else {
                    Delivery::None
                });
            }
            match self.conn.roundtrip() {
                Ok(_) => Ok(Delivery::Completed),
                Err(e) => {
                    eprintln!("keyboard roundtrip failed: {e}");
                    self.release_all();
                    self.broken = true;
                    Ok(Delivery::Unknown)
                }
            }
        }
    }

    fn capitalize(s: &str) -> String {
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => s.into(),
        }
    }
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
            id: 0,
        }
    }

    #[test]
    fn logical_size_normal_and_fractional() {
        assert_eq!(
            mon("a", 0, 0, 2560, 1440, 1.25, 0).logical_size(),
            (2048, 1152)
        );
        assert_eq!(
            mon("a", 0, 0, 3840, 2160, 2.0, 0).logical_size(),
            (1920, 1080)
        );
    }

    #[test]
    fn logical_size_swaps_axes_for_90_270_transforms() {
        for t in [1u8, 3, 5, 7] {
            assert_eq!(
                mon("a", 0, 0, 2560, 1440, 1.0, t).logical_size(),
                (1440, 2560)
            );
        }
        for t in [0u8, 2, 4, 6] {
            assert_eq!(
                mon("a", 0, 0, 2560, 1440, 1.0, t).logical_size(),
                (2560, 1440)
            );
        }
    }

    #[test]
    fn fingerprint_tracks_mapping_relevant_facts() {
        let base = vec![mon("eDP-1", 0, 0, 2560, 1440, 1.25, 0)];
        let same = vec![mon("eDP-1", 0, 0, 2560, 1440, 1.25, 0)];
        assert_eq!(fingerprint(&base), fingerprint(&same));
        for altered in [
            vec![mon("eDP-1", 0, 0, 2560, 1440, 1.5, 0)], // scale
            vec![mon("eDP-1", -2048, 0, 2560, 1440, 1.25, 0)], // position
            vec![mon("eDP-1", 0, 0, 2560, 1440, 1.25, 1)], // transform
            vec![mon("DP-1", 0, 0, 2560, 1440, 1.25, 0)], // identity
        ] {
            assert_ne!(fingerprint(&base), fingerprint(&altered));
        }
        let mut disabled_changed = base.clone();
        disabled_changed[0].disabled = true;
        assert_ne!(fingerprint(&base), fingerprint(&disabled_changed));
    }

    fn layout(ms: &[Monitor]) -> LayoutBox {
        layout_box(ms).unwrap()
    }

    #[test]
    fn map_point_single_scaled_monitor() {
        // 2560x1440 @ 1.25 -> logical 2048x1152; image is physical size.
        let m = mon("eDP-1", 0, 0, 2560, 1440, 1.25, 0);
        let b = layout(&[m.clone()]);
        assert_eq!(map_point(0.0, 0.0, 2560, 1440, &m, &b), Some((0, 0)));
        // Center of the image -> center of the logical box.
        assert_eq!(
            map_point(1280.0, 720.0, 2560, 1440, &m, &b),
            Some((1024, 576))
        );
        // Last valid pixel stays inside.
        let (x, y) = map_point(2559.0, 1439.0, 2560, 1440, &m, &b).unwrap();
        assert!(x < b.x_extent && y < b.y_extent);
        assert_eq!((x, y), (2047, 1151));
    }

    #[test]
    fn map_point_negative_origin_second_monitor() {
        // Left monitor at negative x, scale 1; right at 0, scale 2.
        let left = mon("DP-1", -1920, 0, 1920, 1080, 1.0, 0);
        let right = mon("eDP-1", 0, 0, 3840, 2160, 2.0, 0); // logical 1920x1080
        let b = layout(&[left.clone(), right.clone()]);
        assert_eq!(
            (b.min_x, b.min_y, b.x_extent, b.y_extent),
            (-1920, 0, 3840, 1080)
        );
        // Center of left monitor's image -> (-960, 540) global -> (960, 540) abs.
        assert_eq!(
            map_point(960.0, 540.0, 1920, 1080, &left, &b),
            Some((960, 540))
        );
        // Center of right monitor's image -> (960, 540) global -> (2880, 540).
        assert_eq!(
            map_point(1920.0, 1080.0, 3840, 2160, &right, &b),
            Some((2880, 540))
        );
    }

    #[test]
    fn map_point_rotated_monitor() {
        // Portrait: transform 1 swaps logical axes -> logical 1440x2560.
        let m = mon("DP-1", 0, 0, 2560, 1440, 1.0, 1);
        let b = layout(&[m.clone()]);
        assert_eq!((b.x_extent, b.y_extent), (1440, 2560));
        // Image is already upright; bottom-right image pixel -> bottom-right of box.
        assert_eq!(
            map_point(1439.0, 2559.0, 1440, 2560, &m, &b),
            Some((1439, 2559))
        );
    }

    #[test]
    fn map_point_rejects_bad_input() {
        let m = mon("eDP-1", 0, 0, 2560, 1440, 1.25, 0);
        let b = layout(&[m.clone()]);
        for (px, py) in [
            (2560.0, 0.0),
            (-1.0, 0.0),
            (0.0, 1440.0),
            (f64::NAN, 0.0),
            (f64::INFINITY, 0.0),
            (0.0, f64::NEG_INFINITY),
        ] {
            assert_eq!(map_point(px, py, 2560, 1440, &m, &b), None);
        }
    }

    #[test]
    fn layout_box_empty_without_monitors() {
        assert!(layout_box(&[]).is_none());
        let mut disabled = mon("a", 0, 0, 100, 100, 1.0, 0);
        disabled.disabled = true;
        assert!(layout_box(&[disabled]).is_none());
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
