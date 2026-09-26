use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(3);
const LOCAL_OCR_TIMEOUT: Duration = Duration::from_secs(8);
const BROWSER_EVIDENCE_AGE_MS: u64 = 3_000;
const MAX_EVIDENCE_BYTES: usize = 4 * 1024 * 1024;

/// Evidence returned by the optional native accessibility source. Only an
/// explicitly verified `desktop_logical` coordinate source is accepted;
/// converting those coordinates to an observation's image pixels is
/// deliberately done by the Rust server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Evidence {
    pub source: EvidenceSource,
    /// Computed only by the server from its exact-origin policy. Never accepted
    /// from a page, extension message, evidence file, or model response.
    #[serde(skip)]
    pub browser_actions_authorized: bool,
    pub coordinate_space: String,
    /// Raw AT-SPI frame extents, used only for verified native coordinate mapping.
    #[serde(default)]
    pub native_frame: Option<[f64; 4]>,
    /// Compositor bounds bind OCR-derived desktop coordinates to the same
    /// window geometry that was present in the screenshot.
    #[serde(default)]
    pub window_geometry: Option<WindowGeometry>,
    /// Browser viewport geometry is normalized to desktop logical coordinates
    /// by the Rust server only after it matches the active compositor window.
    #[serde(default)]
    pub browser_viewport: Option<BrowserViewport>,
    /// Wall-clock freshness for extension/OCR sources. It is excluded from the
    /// semantic revision so a fresh heartbeat does not change candidate IDs.
    #[serde(default)]
    pub captured_at_unix_ms: u64,
    #[serde(default)]
    pub elements: Vec<AccessibleElement>,
    /// A bounded native traversal must report truncation instead of allowing a
    /// partial tree to be treated as a complete candidate set.
    #[serde(default = "default_truncated")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowGeometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvidenceSource {
    pub application: String,
    #[serde(default)]
    pub window: String,
    #[serde(default)]
    pub window_id: String,
    /// Stable process identity when the evidence source can provide it.
    #[serde(default)]
    pub process_id: Option<i64>,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub focused: bool,
    /// AT-SPI has no compositor occlusion query. The native helper marks an
    /// active, showing window as clear; fixtures must make that assumption
    /// explicit too. Anything else is rejected before input.
    #[serde(default = "default_occluded")]
    pub occluded: bool,
    /// `native_accessibility` or `browser_extension`; other sources are kept
    /// explicit so a fixture cannot masquerade as browser provenance.
    #[serde(default)]
    pub source_kind: String,
    #[serde(default)]
    pub active_tab: bool,
    #[serde(default)]
    pub browser_window_id: String,
    #[serde(default)]
    pub browser_tab_id: String,
    #[serde(default)]
    pub document_id: String,
    #[serde(default)]
    pub geometry_verified: bool,
    /// Local-only origin from the extension's active-tab API, not page DOM.
    #[serde(default)]
    pub browser_origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BrowserViewport {
    /// Main-frame viewport origin in CSS screen coordinates.
    pub screen_x: f64,
    pub screen_y: f64,
    pub width: f64,
    pub height: f64,
    pub device_pixel_ratio: f64,
    pub visual_scale: f64,
}

fn default_occluded() -> bool {
    true
}

fn default_truncated() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AccessibleElement {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub showing: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub checked: bool,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default)]
    pub editable: bool,
    #[serde(default)]
    pub protected: bool,
    /// Local Tesseract confidence for OCR-derived text; absent for native and
    /// browser semantic controls.
    #[serde(default)]
    pub ocr_confidence: Option<f64>,
}

#[derive(Debug)]
pub(crate) enum EvidenceError {
    Missing,
    Timeout,
    Failed(&'static str),
    Malformed,
    LocalOcrUnavailable,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OcrText {
    pub id: String,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub confidence: f64,
}

#[derive(Debug, Deserialize)]
struct OcrHelperResult {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    elements: Vec<OcrText>,
    #[serde(default = "default_truncated")]
    truncated: bool,
    #[serde(default)]
    region_hash: String,
}

impl Evidence {
    pub(crate) fn revision(&self) -> String {
        // Do not trust a fixture- or toolkit-provided revision by itself:
        // binding includes the complete bounded snapshot so a changed label,
        // state, or bound cannot reuse an old candidate ID. OCR's normalized
        // crop fingerprint is retained so meaningful visual changes invalidate
        // the candidate even when OCR returns the same text and bounds. OCR
        // confidence is omitted because small score changes do not change the
        // grounded target.
        let mut semantic = self.clone();
        semantic.captured_at_unix_ms = 0;
        if semantic.source.source_kind == "local_ocr" {
            for element in &mut semantic.elements {
                element.ocr_confidence = None;
            }
        }
        let bytes = serde_json::to_vec(&semantic).unwrap_or_default();
        format!("{:016x}", stable_hash(&bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ocr_evidence() -> Evidence {
        Evidence {
            source: EvidenceSource {
                application: "fixture".into(),
                window: "fixture window".into(),
                window_id: "window-1".into(),
                process_id: None,
                revision: "pixel-hash-1".into(),
                visible: true,
                focused: true,
                occluded: false,
                source_kind: "local_ocr".into(),
                active_tab: false,
                browser_window_id: String::new(),
                browser_tab_id: String::new(),
                document_id: String::new(),
                geometry_verified: true,
                browser_origin: String::new(),
            },
            browser_actions_authorized: false,
            coordinate_space: "desktop_logical".into(),
            native_frame: None,
            window_geometry: Some(WindowGeometry {
                x: 50.0,
                y: 40.0,
                width: 800.0,
                height: 600.0,
            }),
            browser_viewport: None,
            captured_at_unix_ms: 1,
            elements: vec![AccessibleElement {
                id: "ocr-line-0".into(),
                role: "ocr navigation label".into(),
                name: "Show completion".into(),
                x: 100.0,
                y: 80.0,
                width: 170.0,
                height: 20.0,
                visible: true,
                enabled: true,
                showing: true,
                focused: false,
                selected: false,
                checked: false,
                expanded: false,
                editable: false,
                protected: false,
                ocr_confidence: Some(91.5),
            }],
            truncated: false,
        }
    }

    #[test]
    fn ocr_revision_tracks_crop_pixels_even_when_text_and_bounds_match() {
        let original = ocr_evidence();
        let mut refreshed = original.clone();
        refreshed.captured_at_unix_ms += 500;
        refreshed.source.revision = "different-pixel-hash".into();
        assert_ne!(original.revision(), refreshed.revision());

        refreshed = original.clone();
        refreshed.captured_at_unix_ms += 500;
        refreshed.elements[0].ocr_confidence = Some(91.7);
        assert_eq!(original.revision(), refreshed.revision());

        refreshed.elements[0].ocr_confidence = original.elements[0].ocr_confidence;
        refreshed.elements[0].name = "Delete account".into();
        assert_ne!(original.revision(), refreshed.revision());
        refreshed.elements[0].name = original.elements[0].name.clone();
        refreshed.elements[0].x += 1.0;
        assert_ne!(original.revision(), refreshed.revision());

        refreshed = original.clone();
        refreshed.window_geometry.as_mut().unwrap().x += 1.0;
        assert_ne!(original.revision(), refreshed.revision());

        refreshed = original.clone();
        refreshed.source.process_id = Some(456);
        assert_ne!(original.revision(), refreshed.revision());
    }
}

fn stable_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Collect one bounded native AT-SPI snapshot. Tests and desktop fixtures can
/// provide a JSON snapshot through `COMPUTER_USE_ATSPI_EVIDENCE`; ordinary
/// builds do not need AT-SPI or Python installed until this optional path is
/// actually used.
pub(crate) async fn collect(
    target_application: &str,
    target_window: Option<&str>,
    timeout: Duration,
) -> Result<Evidence, EvidenceError> {
    collect_mode(target_application, target_window, timeout, None).await
}

pub(crate) async fn collect_completion(
    target_application: &str,
    target_window: Option<&str>,
    completion_name: &str,
    completion_role: Option<&str>,
    timeout: Duration,
) -> Result<Evidence, EvidenceError> {
    collect_mode(
        target_application,
        target_window,
        timeout,
        Some((completion_name, completion_role)),
    )
    .await
}

async fn collect_mode(
    target_application: &str,
    target_window: Option<&str>,
    timeout: Duration,
    completion: Option<(&str, Option<&str>)>,
) -> Result<Evidence, EvidenceError> {
    if let Some(path) = std::env::var_os("COMPUTER_USE_ATSPI_EVIDENCE") {
        let path = std::path::PathBuf::from(path);
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| EvidenceError::Failed("accessibility fixture unavailable"))?;
        let bytes = tokio::time::timeout(timeout.min(EVIDENCE_TIMEOUT), read_bounded(file))
            .await
            .map_err(|_| EvidenceError::Timeout)??;
        let mut evidence = parse(&bytes)?;
        if let Some((name, role)) = completion {
            retain_completion_elements(&mut evidence, name, role);
        }
        return Ok(evidence);
    }

    let window = target_window.unwrap_or("");
    let mut command = Command::new("python3");
    command
        .args(["-c", PYTHON_HELPER, target_application, window])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some((name, role)) = completion {
        command.arg("completion").arg(name).arg(role.unwrap_or(""));
    }
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EvidenceError::Missing
        } else {
            EvidenceError::Failed("native accessibility helper could not start")
        }
    })?;

    let stdout = child.stdout.take().ok_or(EvidenceError::Failed(
        "native accessibility helper had no output",
    ))?;
    let output = async move {
        let bytes = read_bounded(stdout).await?;
        let status = child
            .wait()
            .await
            .map_err(|_| EvidenceError::Failed("native accessibility helper failed"))?;
        if !status.success() {
            return Err(EvidenceError::Failed(
                "native accessibility helper returned an error",
            ));
        }
        Ok(bytes)
    };
    let bytes = match tokio::time::timeout(timeout.min(EVIDENCE_TIMEOUT), output).await {
        Ok(result) => result?,
        Err(_) => return Err(EvidenceError::Timeout),
    };
    parse(&bytes)
}

/// Read current-tab evidence delivered by the authenticated browser native
/// messaging host over the MCP process's private runtime socket.
pub(crate) async fn collect_browser(
    target_application: &str,
    target_window: Option<&str>,
    timeout: Duration,
) -> Result<Evidence, EvidenceError> {
    collect_browser_mode(target_application, target_window, None, timeout).await
}

pub(crate) async fn collect_browser_completion(
    target_application: &str,
    target_window: Option<&str>,
    completion_name: &str,
    completion_role: Option<&str>,
    timeout: Duration,
) -> Result<Evidence, EvidenceError> {
    collect_browser_mode(
        target_application,
        target_window,
        Some((completion_name, completion_role)),
        timeout,
    )
    .await
}

/// Run optional local Tesseract against only the caller-approved crop. The
/// screenshot bytes are piped to the helper and are never written to disk or
/// sent to TypeSafe.
pub(crate) async fn collect_local_ocr(
    png: Arc<Vec<u8>>,
    reference_png: Option<Arc<Vec<u8>>>,
    region: &crate::fast_path::OcrRegion,
    languages: &[crate::fast_path::OcrLanguage],
    timeout: Duration,
) -> Result<(Vec<OcrText>, bool, String), EvidenceError> {
    if png.is_empty()
        || png.len() > 20 * 1024 * 1024
        || reference_png
            .as_ref()
            .is_some_and(|png| png.is_empty() || png.len() > 20 * 1024 * 1024)
        || languages.is_empty()
        || languages.len() > 2
    {
        return Err(EvidenceError::Malformed);
    }
    let language = languages
        .iter()
        .map(|language| match language {
            crate::fast_path::OcrLanguage::Eng => "eng",
            crate::fast_path::OcrLanguage::Kor => "kor",
        })
        .collect::<Vec<_>>()
        .join("+");
    let helper_timeout = timeout.min(Duration::from_secs(5)).as_secs_f64().max(0.1);
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg(OCR_HELPER)
        .args([
            region.x.to_string(),
            region.y.to_string(),
            region.width.to_string(),
            region.height.to_string(),
            language,
            helper_timeout.to_string(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if reference_png.is_some() {
        command.arg(png.len().to_string());
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EvidenceError::Missing
        } else {
            EvidenceError::Failed("local OCR helper could not start")
        }
    })?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(EvidenceError::Failed("local OCR helper has no input"));
    };
    let Some(stdout) = child.stdout.take() else {
        return Err(EvidenceError::Failed("local OCR helper has no output"));
    };
    let input = png.clone();
    let output = async move {
        let write = async move {
            let result = async {
                stdin.write_all(input.as_slice()).await?;
                if let Some(reference) = reference_png {
                    stdin.write_all(reference.as_slice()).await?;
                }
                Ok::<(), std::io::Error>(())
            }
            .await;
            // Closing the pipe is what lets the helper's bounded stdin read
            // finish; AsyncWrite::shutdown alone does not reliably deliver
            // EOF for a child process pipe on every runtime platform.
            drop(stdin);
            result
        };
        let read = read_bounded(stdout);
        let (write_result, output) = tokio::join!(write, read);
        write_result.map_err(|_| EvidenceError::Failed("local OCR input failed"))?;
        output
    };
    let bytes = match tokio::time::timeout(timeout.min(LOCAL_OCR_TIMEOUT), output).await {
        Ok(result) => result?,
        Err(_) => return Err(EvidenceError::Timeout),
    };
    let status = child
        .wait()
        .await
        .map_err(|_| EvidenceError::Failed("local OCR helper did not exit"))?;
    if !status.success() {
        return Err(EvidenceError::Failed("local OCR helper failed"));
    }
    let result: OcrHelperResult =
        serde_json::from_slice(&bytes).map_err(|_| EvidenceError::Malformed)?;
    match result.error.as_deref() {
        Some("ocr_unavailable") => Err(EvidenceError::LocalOcrUnavailable),
        Some("ocr_timeout") => Err(EvidenceError::Timeout),
        Some(_) => Err(EvidenceError::Failed("local OCR extraction failed")),
        None if result.truncated => Err(EvidenceError::Failed("local OCR output was truncated")),
        None if result.region_hash.len() == 64
            && result
                .region_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Ok((result.elements, result.truncated, result.region_hash))
        }
        None => Err(EvidenceError::Malformed),
    }
}

async fn collect_browser_mode(
    target_application: &str,
    target_window: Option<&str>,
    completion: Option<(&str, Option<&str>)>,
    timeout: Duration,
) -> Result<Evidence, EvidenceError> {
    let value = crate::native_messaging::refresh_browser_evidence(timeout.min(EVIDENCE_TIMEOUT))
        .await
        .map_err(|error| match error {
            crate::native_messaging::BrowserRefreshError::Unavailable => EvidenceError::Missing,
            crate::native_messaging::BrowserRefreshError::Timeout => EvidenceError::Timeout,
        })?;
    let bytes = serde_json::to_vec(&value).map_err(|_| EvidenceError::Malformed)?;
    let mut evidence = parse(&bytes)?;
    let now = unix_time_ms();
    let fresh = evidence.captured_at_unix_ms > 0
        && evidence.captured_at_unix_ms <= now.saturating_add(1_000)
        && now.saturating_sub(evidence.captured_at_unix_ms) <= BROWSER_EVIDENCE_AGE_MS;
    if evidence.source.source_kind != "browser_extension"
        || !evidence.source.active_tab
        || !evidence.source.visible
        || !evidence.source.focused
        || evidence.source.occluded
        || (!evidence.source.geometry_verified && evidence.source.application != "Chrome")
        || evidence.source.browser_window_id.is_empty()
        || evidence.source.browser_tab_id.is_empty()
        || evidence.source.document_id.is_empty()
        || !fresh
        || evidence.coordinate_space != "browser_viewport_css"
        || evidence.browser_viewport.is_none()
        || evidence.truncated
    {
        return Err(EvidenceError::Failed(
            "browser source provenance, freshness, or geometry is not established",
        ));
    }
    if evidence.source.application != target_application
        || target_window.is_some_and(|window| evidence.source.window != window)
    {
        return Err(EvidenceError::Failed(
            "browser target does not match the active extension tab",
        ));
    }
    if let Some((name, role)) = completion {
        retain_completion_elements(&mut evidence, name, role);
    }
    Ok(evidence)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn normalized_role(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['_', '-'], " ")
}

fn retain_completion_elements(evidence: &mut Evidence, name: &str, role: Option<&str>) {
    evidence.elements.retain(|element| {
        !matches!(
            element.role.as_str(),
            "entry" | "text" | "text field" | "password"
        ) && element.name == name
            && role
                .is_none_or(|expected| normalized_role(expected) == normalized_role(&element.role))
    });
}

async fn read_bounded<R>(mut reader: R) -> Result<Vec<u8>, EvidenceError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|_| EvidenceError::Failed("native accessibility source read failed"))?;
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(read) > MAX_EVIDENCE_BYTES {
            return Err(EvidenceError::Malformed);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn parse(bytes: &[u8]) -> Result<Evidence, EvidenceError> {
    if bytes.len() > MAX_EVIDENCE_BYTES {
        return Err(EvidenceError::Malformed);
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| EvidenceError::Malformed)?;
    if value.get("error").is_some() {
        return Err(EvidenceError::Failed(
            "native accessibility source returned no evidence",
        ));
    }
    serde_json::from_value(value).map_err(|_| EvidenceError::Malformed)
}

// Keep the native dependency optional at runtime. This helper is fixed server
// code, not a command supplied by an MCP caller. It reads only bounded names,
// roles, states, and bounds for controls/status nodes; it never reads editable
// text, clipboard data, paragraphs, or a document body.
const PYTHON_HELPER: &str = include_str!("atspi_helper.py");
const OCR_HELPER: &str = include_str!("ocr_helper.py");

#[derive(Debug, Deserialize)]
pub(crate) struct BrowserGeometry {
    pub process_id: i64,
    pub frame: [f64; 4],
    pub document: [f64; 4],
}

pub(crate) async fn browser_geometry(
    pid: i64,
    title: &str,
    timeout: Duration,
) -> Result<BrowserGeometry, EvidenceError> {
    if pid <= 0 || title.is_empty() {
        return Err(EvidenceError::Malformed);
    }
    let mut child = Command::new("python3")
        .args([
            "-c",
            PYTHON_HELPER,
            "",
            title,
            "browser_viewport",
            &pid.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| EvidenceError::Missing)?;
    let stdout = child.stdout.take().ok_or(EvidenceError::Malformed)?;
    let read = async {
        let bytes = read_bounded(stdout).await?;
        let status = child.wait().await.map_err(|_| EvidenceError::Malformed)?;
        if !status.success() {
            return Err(EvidenceError::Malformed);
        }
        serde_json::from_slice(&bytes).map_err(|_| EvidenceError::Malformed)
    };
    tokio::time::timeout(timeout.min(EVIDENCE_TIMEOUT), read)
        .await
        .map_err(|_| EvidenceError::Timeout)?
}
