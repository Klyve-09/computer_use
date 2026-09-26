//! Browser native-messaging bridge. It accepts only the extension's bounded
//! semantic projection to the MCP process over a private authenticated socket.

use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::thread;
use tokio::sync::{Mutex as AsyncMutex, Notify};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_EVIDENCE_AGE_MS: u64 = 3_000;
const BROWSER_POLL_MAX_AGE_MS: u64 = 1_000;
const DEFAULT_FIREFOX_EXTENSION_ID: &str = "jev-fast-path@computer-use.local";
const DEFAULT_CHROME_EXTENSION_ORIGIN: &str =
    "chrome-extension://finpjmpnpjlbfobfabnbphilkhbjanel/";
const BRIDGE_SUBDIRECTORY: &str = "computer-use-mcp";
const BRIDGE_SOCKET_NAME: &str = "browser-evidence.sock";

#[derive(Default)]
struct BrowserRefreshState {
    pending_request_id: Option<String>,
    response: Option<(String, Value)>,
}

static BROWSER_REFRESH: OnceLock<RwLock<BrowserRefreshState>> = OnceLock::new();
static BROWSER_REFRESH_NOTIFY: OnceLock<Notify> = OnceLock::new();
static BROWSER_REFRESH_SERIALIZER: OnceLock<AsyncMutex<()>> = OnceLock::new();
static NEXT_REFRESH_ID: AtomicU64 = AtomicU64::new(1);
static LAST_BROWSER_POLL_UNIX_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserRefreshError {
    Unavailable,
    Timeout,
}

/// Ask the opted-in extension for a new snapshot of the tab that is active
/// now. Cached browser evidence is never returned to an input revalidation.
pub(crate) async fn refresh_browser_evidence(
    timeout: std::time::Duration,
) -> Result<Value, BrowserRefreshError> {
    let started = std::time::Instant::now();
    let serializer = BROWSER_REFRESH_SERIALIZER.get_or_init(|| AsyncMutex::new(()));
    let _guard = tokio::time::timeout(timeout, serializer.lock())
        .await
        .map_err(|_| BrowserRefreshError::Timeout)?;
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(BrowserRefreshError::Timeout);
    }
    let now = unix_time_ms();
    let last_poll = LAST_BROWSER_POLL_UNIX_MS.load(Ordering::Acquire);
    if last_poll == 0 || now.saturating_sub(last_poll) > BROWSER_POLL_MAX_AGE_MS {
        return Err(BrowserRefreshError::Unavailable);
    }
    let request_id = format!(
        "{}-{}",
        now,
        NEXT_REFRESH_ID.fetch_add(1, Ordering::Relaxed)
    );
    {
        let mut state = BROWSER_REFRESH
            .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
            .write()
            .map_err(|_| BrowserRefreshError::Unavailable)?;
        state.pending_request_id = Some(request_id.clone());
        state.response = None;
    }
    let result = tokio::time::timeout(remaining, wait_for_browser_refresh(&request_id)).await;
    if let Ok(mut state) = BROWSER_REFRESH
        .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
        .write()
    {
        if state.pending_request_id.as_deref() == Some(&request_id) {
            state.pending_request_id = None;
        }
    }
    match result {
        Ok(Some(evidence)) => Ok(evidence),
        Ok(None) => Err(BrowserRefreshError::Unavailable),
        Err(_) => Err(BrowserRefreshError::Timeout),
    }
}

async fn wait_for_browser_refresh(request_id: &str) -> Option<Value> {
    loop {
        let notified = BROWSER_REFRESH_NOTIFY.get_or_init(Notify::new).notified();
        {
            let mut state = BROWSER_REFRESH
                .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
                .write()
                .ok()?;
            if let Some((response_id, evidence)) = state.response.as_ref() {
                if response_id == request_id {
                    let evidence = evidence.clone();
                    state.response = None;
                    return Some(evidence);
                }
            }
            if state.pending_request_id.as_deref() != Some(request_id) {
                return None;
            }
        }
        notified.await;
    }
}

/// Start the private local bridge that accepts only native-host subprocesses
/// launched by an installed, trusted browser executable.
pub(crate) fn start_browser_bridge() -> io::Result<()> {
    let path = bridge_socket_path()?;
    let runtime = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "runtime path unavailable"))?;
    if std::fs::symlink_metadata(runtime)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory is a symlink",
        ));
    }
    let runtime_metadata = std::fs::metadata(runtime)?;
    if runtime_metadata.uid() != unsafe { libc::geteuid() } || runtime_metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory is not private to the current user",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bridge path unavailable"))?;
    std::fs::create_dir_all(parent)?;
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || parent_metadata.uid() != runtime_metadata.uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "bridge directory is not trusted",
        ));
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;

    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bridge path is not a socket",
            ));
        }
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "browser evidence bridge is already active",
            ));
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    thread::Builder::new()
        .name("browser-evidence-bridge".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_bridge_client(stream);
            }
        })?;
    Ok(())
}

pub(crate) fn run() -> io::Result<()> {
    let parent = parent_process_executable()?;
    if !authorized_caller(std::env::args().skip(1), trusted_browser_parent(&parent)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "browser extension caller is not authorized",
        ));
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let Some(message) = read_message(&mut input)? else {
            return Ok(());
        };
        let response = match message.get("type").and_then(Value::as_str) {
            Some("invalidate") => forward_to_server(&json!({"type": "invalidate"}))
                .unwrap_or_else(|_| json!({"ok": false, "error": "invalid_or_unavailable"})),
            Some("poll") => forward_to_server(&json!({"type": "poll"}))
                .unwrap_or_else(|_| json!({"ok": false, "error": "invalid_or_unavailable"})),
            Some("evidence") => match sanitize_message(&message) {
                Ok(evidence) => {
                    let request_id = message
                        .get("request_id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty() && id.len() <= 64);
                    forward_to_server(&json!({
                        "type": "evidence",
                        "request_id": request_id,
                        "evidence": evidence
                    }))
                    .unwrap_or_else(|_| json!({"ok": false, "error": "invalid_or_unavailable"}))
                }
                Err(_) => json!({"ok": false, "error": "invalid_or_unavailable"}),
            },
            _ => json!({"ok": false, "error": "invalid_or_unavailable"}),
        };
        write_message(&mut output, &response)?;
    }
}

fn parent_process_executable() -> io::Result<PathBuf> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let parent_pid = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:")?.trim().parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| io::Error::other("native host parent process is unavailable"))?;
    std::fs::read_link(format!("/proc/{parent_pid}/exe"))
}

fn process_parent_executable(pid: u32) -> io::Result<PathBuf> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let parent_pid = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:")?.trim().parse::<u32>().ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| io::Error::other("browser process parent is unavailable"))?;
    std::fs::read_link(format!("/proc/{parent_pid}/exe"))
}

fn bridge_socket_path() -> io::Result<std::path::PathBuf> {
    let runtime = crate::backend::runtime_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "runtime directory is missing"))?;
    Ok(runtime.join(BRIDGE_SUBDIRECTORY).join(BRIDGE_SOCKET_NAME))
}

fn forward_to_server(message: &Value) -> io::Result<Value> {
    let mut stream = UnixStream::connect(bridge_socket_path()?)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    write_message(&mut stream, message)?;
    read_message(&mut stream)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "bridge server closed without a response",
        )
    })
}

fn serve_bridge_client(mut stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let response = match bridge_peer_identity(&stream) {
        Some(true) => read_message(&mut stream)
            .and_then(|message| {
                message.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "empty bridge message")
                })
            })
            .and_then(|message| apply_bridge_message(&message)),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "bridge client is not an authenticated browser host",
        )),
    }
    .unwrap_or_else(|_| json!({"ok": false, "error": "invalid_or_unavailable"}));
    let _ = write_message(&mut stream, &response);
}

fn bridge_peer_identity(stream: &UnixStream) -> Option<bool> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return None;
    }
    let credentials = unsafe { credentials.assume_init() };
    Some(authorized_bridge_peer(credentials.pid, credentials.uid))
}

fn authorized_bridge_peer(pid: libc::pid_t, uid: libc::uid_t) -> bool {
    if pid <= 0 || uid != unsafe { libc::geteuid() } {
        return false;
    }
    let peer = format!("/proc/{pid}");
    let Ok(peer_executable) = std::fs::read_link(format!("{peer}/exe")) else {
        return false;
    };
    let Ok(server_executable) = std::env::current_exe() else {
        return false;
    };
    let (Ok(peer_metadata), Ok(server_metadata)) = (
        std::fs::metadata(&peer_executable),
        std::fs::metadata(&server_executable),
    ) else {
        return false;
    };
    if peer_metadata.dev() != server_metadata.dev() || peer_metadata.ino() != server_metadata.ino()
    {
        return false;
    }
    let Ok(parent_executable) = process_parent_executable(pid as u32) else {
        return false;
    };
    let Some(parent_family) = trusted_browser_parent(&parent_executable) else {
        return false;
    };
    let Ok(command_line) = std::fs::read(format!("{peer}/cmdline")) else {
        return false;
    };
    let arguments = command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect::<Vec<_>>();
    arguments
        .get(1)
        .is_some_and(|arg| arg == "--native-browser-host")
        && authorized_caller(arguments.into_iter().skip(2), Some(parent_family))
}

fn apply_bridge_message(message: &Value) -> io::Result<Value> {
    match message.get("type").and_then(Value::as_str) {
        Some("invalidate") => {
            let mut state = BROWSER_REFRESH
                .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
                .write()
                .map_err(|_| io::Error::other("browser evidence lock is poisoned"))?;
            state.pending_request_id = None;
            state.response = None;
            LAST_BROWSER_POLL_UNIX_MS.store(0, Ordering::Release);
            BROWSER_REFRESH_NOTIFY.get_or_init(Notify::new).notify_one();
            Ok(json!({"ok": true}))
        }
        Some("poll") => {
            LAST_BROWSER_POLL_UNIX_MS.store(unix_time_ms(), Ordering::Release);
            let request_id = BROWSER_REFRESH
                .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
                .read()
                .map_err(|_| io::Error::other("browser evidence lock is poisoned"))?
                .pending_request_id
                .clone();
            Ok(match request_id {
                Some(request_id) => json!({"type": "refresh", "request_id": request_id}),
                None => json!({"ok": true}),
            })
        }
        Some("evidence") => {
            let request_id = message
                .get("request_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 64);
            let wrapper = json!({"type": "evidence", "evidence": message.get("evidence")});
            let evidence = sanitize_message(&wrapper)?;
            if let Some(request_id) = request_id {
                let mut state = BROWSER_REFRESH
                    .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
                    .write()
                    .map_err(|_| io::Error::other("browser evidence lock is poisoned"))?;
                if state.pending_request_id.as_deref() == Some(request_id) {
                    state.response = Some((request_id.to_owned(), evidence));
                    state.pending_request_id = None;
                    drop(state);
                    BROWSER_REFRESH_NOTIFY.get_or_init(Notify::new).notify_one();
                }
            }
            Ok(json!({"ok": true}))
        }
        _ => invalid_data("unsupported bridge message"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserFamily {
    Firefox,
    Chrome,
}

fn trusted_browser_parent(executable: &Path) -> Option<BrowserFamily> {
    let executable = executable.canonicalize().ok()?;
    for path in executable.ancestors() {
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return None;
        }
    }
    match executable
        .file_name()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "firefox" | "firefox-bin" | "firefox-esr" => Some(BrowserFamily::Firefox),
        "chrome" | "chromium" | "google-chrome" | "google-chrome-stable" => {
            Some(BrowserFamily::Chrome)
        }
        _ => None,
    }
}

fn authorized_caller(
    arguments: impl Iterator<Item = String>,
    parent: Option<BrowserFamily>,
) -> bool {
    let configured_id = std::env::var("COMPUTER_USE_BROWSER_EXTENSION_ID").ok();
    authorized_caller_with_id(arguments, parent, configured_id.as_deref())
}

fn authorized_caller_with_id(
    arguments: impl Iterator<Item = String>,
    parent: Option<BrowserFamily>,
    configured_id: Option<&str>,
) -> bool {
    let arguments = arguments.collect::<Vec<_>>();
    let expected_firefox_id = configured_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.len() <= 128 && !id.starts_with("chrome-extension://"))
        .unwrap_or(DEFAULT_FIREFOX_EXTENSION_ID);
    let expected_chrome_origin = configured_id
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .map(|id| {
            if id.starts_with("chrome-extension://") {
                id.to_owned()
            } else {
                format!("chrome-extension://{id}/")
            }
        })
        .unwrap_or_else(|| DEFAULT_CHROME_EXTENSION_ORIGIN.to_owned());
    (parent == Some(BrowserFamily::Firefox)
        && arguments.iter().any(|arg| arg == expected_firefox_id))
        || (parent == Some(BrowserFamily::Chrome)
            && arguments.iter().any(|arg| arg == &expected_chrome_origin))
}

fn read_message(reader: &mut impl Read) -> io::Result<Option<Value>> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(prefix) as usize;
    if length == 0 || length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native message exceeds limit",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "malformed native message"))
}

fn write_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "response serialization failed"))?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds limit",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

fn sanitize_message(message: &Value) -> io::Result<Value> {
    if message.get("type").and_then(Value::as_str) != Some("evidence") {
        return invalid_data("unsupported browser message");
    }
    let evidence = message
        .get("evidence")
        .filter(|value| value.is_object())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing evidence"))?;
    let source = evidence
        .get("source")
        .filter(|value| value.is_object())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing source"))?;
    let application = bounded_string(source, "application", 64)?;
    if !matches!(application, "Firefox" | "Chrome")
        || bounded_string(source, "source_kind", 32)? != "browser_extension"
        || !boolean(source, "active_tab")?
        || !boolean(source, "visible")?
        || !boolean(source, "focused")?
        || boolean(source, "occluded")?
        || (!boolean(source, "geometry_verified")? && application != "Chrome")
        || evidence.get("coordinate_space").and_then(Value::as_str) != Some("browser_viewport_css")
        || evidence.get("truncated").and_then(Value::as_bool) != Some(false)
    {
        return invalid_data("browser source is not actionable");
    }

    let window = bounded_string(source, "window", 256)?;
    let window_id = bounded_string(source, "window_id", 256)?;
    let browser_window_id = bounded_string(source, "browser_window_id", 128)?;
    let browser_tab_id = bounded_string(source, "browser_tab_id", 128)?;
    let document_id = bounded_string(source, "document_id", 128)?;
    let revision = bounded_string(source, "revision", 128)?;
    let browser_origin = source
        .get("browser_origin")
        .and_then(Value::as_str)
        .unwrap_or("");
    if browser_origin.len() > 512 || browser_origin.chars().any(char::is_control) {
        return invalid_data("invalid browser origin");
    }
    if window_id != format!("{browser_window_id}:{browser_tab_id}:{document_id}")
        || revision != document_id
    {
        return invalid_data("browser tab provenance is inconsistent");
    }

    let viewport = evidence
        .get("browser_viewport")
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing viewport"))?;
    let mut clean_viewport = serde_json::Map::new();
    for key in [
        "screen_x",
        "screen_y",
        "width",
        "height",
        "device_pixel_ratio",
        "visual_scale",
    ] {
        clean_viewport.insert(key.into(), json!(finite_number(viewport.get(key))?));
    }
    if clean_viewport["width"].as_f64().unwrap_or(0.0) <= 0.0
        || clean_viewport["height"].as_f64().unwrap_or(0.0) <= 0.0
        || clean_viewport["device_pixel_ratio"].as_f64().unwrap_or(0.0) <= 0.0
        || clean_viewport["visual_scale"].as_f64().unwrap_or(0.0) != 1.0
    {
        return invalid_data("browser viewport is invalid");
    }

    let elements = evidence
        .get("elements")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing elements"))?;
    if elements.len() > 512 {
        return invalid_data("browser element limit exceeded");
    }
    let mut clean_elements = Vec::with_capacity(elements.len());
    for element in elements {
        if !element.is_object() {
            return invalid_data("malformed browser element");
        }
        let role = bounded_string(element, "role", 32)?;
        let media_role = browser_origin == "https://www.netflix.com"
            && matches!(role, "media title" | "media play");
        if !(matches!(role, "tab" | "label" | "status" | "text field") || media_role)
            || !boolean(element, "visible")?
            || !boolean(element, "showing")?
        {
            return invalid_data("browser element is not visible or supported");
        }
        let editable = if role == "text field" {
            if !boolean(element, "editable")? || boolean(element, "protected")? {
                return invalid_data("browser text field is not safely editable");
            }
            true
        } else {
            false
        };
        let mut clean = serde_json::Map::new();
        for key in ["id", "role", "name"] {
            let limit = if key == "name" { 120 } else { 128 };
            clean.insert(key.into(), json!(bounded_string(element, key, limit)?));
        }
        for key in ["x", "y", "width", "height"] {
            clean.insert(key.into(), json!(finite_number(element.get(key))?));
        }
        for key in ["visible", "enabled", "showing", "focused", "selected"] {
            clean.insert(key.into(), json!(boolean(element, key)?));
        }
        clean.insert("editable".into(), json!(editable));
        clean.insert("protected".into(), json!(false));
        clean_elements.push(Value::Object(clean));
    }

    let timestamp = evidence
        .get("captured_at_unix_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing capture time"))?;
    let now = unix_time_ms();
    if timestamp == 0
        || timestamp > now.saturating_add(1_000)
        || now.saturating_sub(timestamp) > MAX_EVIDENCE_AGE_MS
    {
        return invalid_data("browser snapshot is stale");
    }
    Ok(json!({
        "source": {
            "application": application,
            "window": window,
            "window_id": window_id,
            "revision": revision,
            "visible": true,
            "focused": true,
            "occluded": false,
            "source_kind": "browser_extension",
            "browser_origin": browser_origin,
            "active_tab": true,
            "browser_window_id": browser_window_id,
            "browser_tab_id": browser_tab_id,
            "document_id": document_id,
            "geometry_verified": boolean(source, "geometry_verified")?
        },
        "coordinate_space": "browser_viewport_css",
        "browser_viewport": clean_viewport,
        "captured_at_unix_ms": timestamp,
        "truncated": false,
        "elements": clean_elements
    }))
}

fn bounded_string<'a>(object: &'a Value, key: &str, max: usize) -> io::Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid browser string"))
}

fn boolean(object: &Value, key: &str) -> io::Result<bool> {
    object
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid browser state"))
}

fn finite_number(value: Option<&Value>) -> io::Result<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && n.abs() <= 100_000.0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid browser geometry"))
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn invalid_data<T>(message: &'static str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    static REFRESH_TEST_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

    fn evidence() -> Value {
        json!({
            "type": "evidence",
            "unrelated": "PRIVATE_CANARY",
            "evidence": {
                "source": {
                    "application": "Firefox",
                    "window": "Fixture tab",
                    "window_id": "1:2:document-1",
                    "revision": "document-1",
                    "visible": true,
                    "focused": true,
                    "occluded": false,
                    "source_kind": "browser_extension",
                    "active_tab": true,
                    "browser_window_id": "1",
                    "browser_tab_id": "2",
                    "document_id": "document-1",
            "geometry_verified": true,
                    "url": "https://private.example"
                },
                "coordinate_space": "browser_viewport_css",
                "browser_viewport": {
                    "screen_x": 20.0,
                    "screen_y": 30.0,
                    "width": 500.0,
                    "height": 300.0,
                    "device_pixel_ratio": 1.25,
                    "visual_scale": 1.0,
                    "untrusted_extra": "PRIVATE_CANARY"
                },
                "captured_at_unix_ms": unix_time_ms(),
                "truncated": false,
                "elements": [
                    {
                        "id": "tab-1", "role": "tab", "name": "Open details",
                        "x": 12.0, "y": 20.0, "width": 80.0, "height": 30.0,
                        "visible": true, "enabled": true, "showing": true,
                        "focused": false, "selected": false,
                        "document_body": "PRIVATE_CANARY"
                    },
                    {
                        "id": "field-1", "role": "text field", "name": "Search",
                        "x": 10.0, "y": 100.0, "width": 160.0, "height": 32.0,
                        "visible": true, "enabled": true, "showing": true,
                        "focused": true, "selected": false,
                        "editable": true, "protected": false,
                        "value": "PRIVATE_FORM_VALUE"
                    }
                ]
            }
        })
    }

    #[test]
    fn native_host_media_roles_require_netflix_origin() {
        let mut message = evidence();
        message["evidence"]["elements"][0]["role"] = json!("media play");
        assert!(sanitize_message(&message).is_err());
        message["evidence"]["source"]["browser_origin"] = json!("https://www.netflix.com");
        assert!(sanitize_message(&message).is_ok());
        message["evidence"]["elements"][0]["role"] = json!("media title");
        assert!(sanitize_message(&message).is_ok());
        message["evidence"]["elements"][0]["role"] = json!("button");
        assert!(sanitize_message(&message).is_err());
    }

    #[test]
    fn native_host_keeps_only_bounded_visible_browser_semantics() {
        let cleaned = sanitize_message(&evidence()).unwrap();
        let serialized = cleaned.to_string();
        assert!(serialized.contains("Open details"));
        assert!(!serialized.contains("PRIVATE_CANARY"));
        assert!(!serialized.contains("PRIVATE_FORM_VALUE"));
        assert!(!serialized.contains("private.example"));
        assert!(cleaned["elements"][0]["editable"] == false);
        assert_eq!(cleaned["elements"][1]["name"], "Search");
        assert_eq!(cleaned["elements"][1]["editable"], true);
        assert_eq!(cleaned["elements"][0].as_object().unwrap().len(), 14);
    }

    #[test]
    fn native_host_preserves_disabled_visible_controls_for_local_filtering() {
        let mut message = evidence();
        message["evidence"]["elements"][0]["enabled"] = json!(false);
        let cleaned = sanitize_message(&message).unwrap();
        assert_eq!(cleaned["elements"][0]["enabled"], false);
        assert_eq!(cleaned["elements"][1]["enabled"], true);
    }

    #[test]
    fn native_host_rejects_inconsistent_browser_tab_provenance() {
        let mut wrong_tab = evidence();
        wrong_tab["evidence"]["source"]["browser_tab_id"] = json!("3");
        assert!(sanitize_message(&wrong_tab).is_err());

        let mut wrong_document = evidence();
        wrong_document["evidence"]["source"]["document_id"] = json!("document-2");
        assert!(sanitize_message(&wrong_document).is_err());

        let mut wrong_revision = evidence();
        wrong_revision["evidence"]["source"]["revision"] = json!("revision-2");
        assert!(sanitize_message(&wrong_revision).is_err());
    }

    #[tokio::test]
    async fn bridge_refresh_requires_the_matching_live_tab_request_id() {
        let _test_guard = REFRESH_TEST_LOCK
            .get_or_init(|| AsyncMutex::new(()))
            .lock()
            .await;
        let state_lock =
            BROWSER_REFRESH.get_or_init(|| RwLock::new(BrowserRefreshState::default()));
        {
            let mut state = state_lock.write().unwrap();
            state.pending_request_id = Some("refresh-B".into());
            state.response = None;
        }
        let command = apply_bridge_message(&json!({"type": "poll"})).unwrap();
        assert_eq!(
            command,
            json!({"type": "refresh", "request_id": "refresh-B"})
        );

        let mut stale = evidence();
        stale["request_id"] = json!("refresh-A");
        apply_bridge_message(&stale).unwrap();
        {
            let state = state_lock.read().unwrap();
            assert_eq!(state.pending_request_id.as_deref(), Some("refresh-B"));
            assert!(state.response.is_none());
        }

        let mut current = evidence();
        current["request_id"] = json!("refresh-B");
        current["evidence"]["source"]["browser_tab_id"] = json!("tab-B");
        let window_id = current["evidence"]["source"]["window_id"]
            .as_str()
            .unwrap()
            .replace(":2:", ":tab-B:");
        current["evidence"]["source"]["window_id"] = json!(window_id);
        apply_bridge_message(&current).unwrap();
        {
            let mut state = state_lock.write().unwrap();
            let (request_id, evidence) = state.response.take().unwrap();
            assert_eq!(request_id, "refresh-B");
            assert_eq!(evidence["source"]["browser_tab_id"], "tab-B");
            assert!(state.pending_request_id.is_none());
            state.response = None;
        }
        LAST_BROWSER_POLL_UNIX_MS.store(0, Ordering::Release);
    }

    #[tokio::test]
    async fn concurrent_browser_refreshes_do_not_replace_each_others_request() {
        let _test_guard = REFRESH_TEST_LOCK
            .get_or_init(|| AsyncMutex::new(()))
            .lock()
            .await;
        {
            let mut state = BROWSER_REFRESH
                .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
                .write()
                .unwrap();
            state.pending_request_id = None;
            state.response = None;
        }
        apply_bridge_message(&json!({"type": "poll"})).unwrap();
        let first = tokio::spawn(refresh_browser_evidence(std::time::Duration::from_secs(2)));

        async fn next_request(except: Option<&str>) -> String {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let response = apply_bridge_message(&json!({"type": "poll"})).unwrap();
                if response["type"] == "refresh" {
                    let request_id = response["request_id"].as_str().unwrap().to_owned();
                    if except != Some(request_id.as_str()) {
                        return request_id;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "refresh request did not arrive"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }

        let first_id = next_request(None).await;
        let second = tokio::spawn(refresh_browser_evidence(std::time::Duration::from_secs(2)));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let still_first = apply_bridge_message(&json!({"type": "poll"})).unwrap();
        assert_eq!(still_first["request_id"], first_id);

        let mut first_evidence = evidence();
        first_evidence["request_id"] = json!(first_id);
        apply_bridge_message(&first_evidence).unwrap();
        let second_id = next_request(Some(&first_id)).await;
        let mut second_evidence = evidence();
        second_evidence["request_id"] = json!(second_id);
        second_evidence["evidence"]["source"]["browser_tab_id"] = json!("tab-B");
        let window_id = second_evidence["evidence"]["source"]["window_id"]
            .as_str()
            .unwrap()
            .replace(":2:", ":tab-B:");
        second_evidence["evidence"]["source"]["window_id"] = json!(window_id);
        apply_bridge_message(&second_evidence).unwrap();

        let first_result = first.await.unwrap().unwrap();
        let second_result = second.await.unwrap().unwrap();
        assert_eq!(first_result["source"]["browser_tab_id"], "2");
        assert_eq!(second_result["source"]["browser_tab_id"], "tab-B");
        {
            let mut state = BROWSER_REFRESH
                .get_or_init(|| RwLock::new(BrowserRefreshState::default()))
                .write()
                .unwrap();
            state.pending_request_id = None;
            state.response = None;
        }
        LAST_BROWSER_POLL_UNIX_MS.store(0, Ordering::Release);
    }

    #[test]
    fn native_host_rejects_hidden_elements_and_unverified_geometry() {
        let mut hidden = evidence();
        hidden["evidence"]["elements"][0]["visible"] = json!(false);
        assert!(sanitize_message(&hidden).is_err());

        let mut unverified = evidence();
        unverified["evidence"]["source"]["geometry_verified"] = json!(false);
        assert!(sanitize_message(&unverified).is_err());
    }

    #[test]
    fn native_host_requires_allowlisted_extension_and_browser_parent() {
        assert!(authorized_caller_with_id(
            [DEFAULT_FIREFOX_EXTENSION_ID.to_owned()].into_iter(),
            Some(BrowserFamily::Firefox),
            None
        ));
        assert!(!authorized_caller_with_id(
            ["https://example.com".to_owned()].into_iter(),
            Some(BrowserFamily::Firefox),
            None
        ));
        assert!(!authorized_caller_with_id(
            [DEFAULT_FIREFOX_EXTENSION_ID.to_owned()].into_iter(),
            None,
            None
        ));
        assert!(authorized_caller_with_id(
            [DEFAULT_CHROME_EXTENSION_ORIGIN.to_owned()].into_iter(),
            Some(BrowserFamily::Chrome),
            None
        ));
        assert!(authorized_caller_with_id(
            ["custom-firefox-extension".to_owned()].into_iter(),
            Some(BrowserFamily::Firefox),
            Some("custom-firefox-extension")
        ));
        assert!(authorized_caller_with_id(
            ["chrome-extension://custom-id/".to_owned()].into_iter(),
            Some(BrowserFamily::Chrome),
            Some("chrome-extension://custom-id/")
        ));
        assert!(authorized_caller_with_id(
            [DEFAULT_FIREFOX_EXTENSION_ID.to_owned()].into_iter(),
            Some(BrowserFamily::Firefox),
            Some("chrome-extension://custom-id/")
        ));
        assert!(authorized_caller_with_id(
            [DEFAULT_FIREFOX_EXTENSION_ID.to_owned()].into_iter(),
            Some(BrowserFamily::Firefox),
            Some(&"x".repeat(129))
        ));
        assert!(!authorized_caller_with_id(
            ["chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".to_owned()].into_iter(),
            Some(BrowserFamily::Chrome),
            None
        ));
        assert!(!authorized_caller_with_id(
            [DEFAULT_CHROME_EXTENSION_ORIGIN.to_owned()].into_iter(),
            None,
            None
        ));
    }

    #[test]
    fn native_host_rejects_browser_named_executables_outside_trusted_paths() {
        let directory = std::env::temp_dir().join(format!(
            "jev-browser-parent-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let fake = directory.join("firefox");
        std::fs::write(&fake, b"not a browser").unwrap();
        assert_eq!(trusted_browser_parent(&fake), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn local_process_cannot_publish_to_the_browser_bridge() {
        assert!(!authorized_bridge_peer(
            std::process::id() as libc::pid_t,
            unsafe { libc::geteuid() }
        ));

        let path = std::env::temp_dir().join(format!(
            "jev-bridge-peer-test-{}-{}.sock",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_bridge_client(stream);
        });
        let mut client = UnixStream::connect(&path).unwrap();
        let response = read_message(&mut client).unwrap().unwrap();
        assert_eq!(response["ok"], false);
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
