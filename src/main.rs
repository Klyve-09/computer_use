mod accessibility;
mod backend;
mod fast_path;
mod native_messaging;
mod typesafe;

use backend::{BackendError, KeyOp, Keyboard, Monitor, Pointer, PointerOp};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock as Content, ErrorData as McpError};
use rmcp::service::ServiceExt;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ObserveParams {
    /// Monitor identifier from `computer_monitors` (the Hyprland output name).
    pub monitor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClickButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionKind {
    /// Click at image-pixel coordinates from the referenced observation.
    Click {
        x: f64,
        y: f64,
        button: ClickButton,
        #[serde(default)]
        double: bool,
    },
    /// Scroll at a point: signed wheel steps, dy vertical, dx horizontal.
    Scroll {
        x: f64,
        y: f64,
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
    },
    /// Key press with optional modifiers (e.g. key "return", mods ["ctrl"]).
    /// Acts on the focused application on the observed monitor.
    Key {
        key: String,
        #[serde(default)]
        mods: Vec<String>,
    },
    /// Drag from a point in this observation's image to a point in the
    /// destination observation's image (may be a different monitor). One
    /// move/press/move/release sequence; endpoints are mapped independently.
    Drag {
        x: f64,
        y: f64,
        /// observation_id of the destination observation (same or another
        /// monitor); must be current under the same Display Configuration.
        dst_observation_id: String,
        dst_x: f64,
        dst_y: f64,
    },
    /// Enter exact UTF-8 text via clipboard + paste shortcut.
    /// Replaces the clipboard. `paste`: "ctrl_v" (default) or
    /// "ctrl_shift_v" (terminals).
    TypeText {
        text: String,
        #[serde(default)]
        paste: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ActionParams {
    /// Opaque observation_id from `computer_observe`.
    pub observation_id: String,
    pub action: ActionKind,
}

#[derive(Debug, Clone)]
struct Observation {
    monitor: String,
    image_width: u32,
    image_height: u32,
    generation: u64,
    fingerprint: u64,
    image_png: Option<Arc<Vec<u8>>>,
}

const MAX_OCR_OBSERVATION_BYTES: usize = 20 * 1024 * 1024;

struct State {
    epoch: Uuid,
    generation: Arc<AtomicU64>,
    events_healthy: Arc<AtomicBool>,
    wayland_healthy: Arc<AtomicBool>,
    observations: Mutex<HashMap<String, Observation>>,
}

impl State {
    /// Opaque Display Configuration revision: session epoch + event-driven
    /// generation + snapshot fingerprint. A change-and-restore still bumps the
    /// generation, and a server restart changes the epoch.
    /// An observation is actionable only while both notification channels are
    /// connected: the Hyprland IPC socket and the wl_output listener (which
    /// covers reconfigures the IPC socket never reports).
    fn actionable(&self) -> bool {
        self.events_healthy.load(Ordering::SeqCst) && self.wayland_healthy.load(Ordering::SeqCst)
    }

    fn revision(&self, fingerprint: u64) -> String {
        format!(
            "{}-{}-{:016x}",
            self.epoch,
            self.generation.load(Ordering::SeqCst),
            fingerprint
        )
    }

    /// Used by `computer_action` (issue #3); tested now because stale-
    /// observation rejection is part of this ticket's contract.
    /// An observation authorizes later input only while its generation is
    /// still current AND a fresh snapshot reproduces its fingerprint.
    /// Session epoch is implicit: observations from a previous process are
    /// simply absent. Generation bumps cover display events, silent
    /// reconfigures (wl_output), change-and-restore, and socket loss.
    fn validate_observation(
        &self,
        id: &str,
        current_fingerprint: u64,
    ) -> Result<Observation, &'static str> {
        let observations = self.observations.lock().unwrap();
        let obs = observations
            .get(id)
            .ok_or("unknown or expired observation")?;
        if obs.generation != self.generation.load(Ordering::SeqCst)
            || obs.fingerprint != current_fingerprint
        {
            return Err("stale observation: display configuration changed");
        }
        Ok(obs.clone())
    }

    /// An action that reaches input invalidates its source observation even if
    /// no replacement observation can be captured afterward. This prevents a
    /// waiting Fast Path from reusing state after partial or unknown delivery.
    fn invalidate_observation(&self, id: &str) {
        self.observations.lock().unwrap().remove(id);
    }
}

#[derive(Clone)]
struct ComputerUse {
    state: Arc<State>,
    pointer: Arc<Pointer>,
    keyboard: Arc<Keyboard>,
    /// Input is serialized: no two actions may interleave pointer events.
    action_lock: Arc<tokio::sync::Mutex<()>>,
    #[allow(dead_code)] // read by the #[tool_handler]-generated call_tool
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl ComputerUse {
    fn new() -> Self {
        let state = Arc::new(State {
            epoch: Uuid::new_v4(),
            generation: Arc::new(AtomicU64::new(0)),
            events_healthy: Arc::new(AtomicBool::new(false)),
            wayland_healthy: Arc::new(AtomicBool::new(false)),
            observations: Mutex::new(HashMap::new()),
        });
        backend::watch_events(state.generation.clone(), state.events_healthy.clone());
        backend::watch_wayland_outputs(state.generation.clone(), state.wayland_healthy.clone());
        Self {
            state,
            pointer: Arc::new(Pointer::start()),
            keyboard: Arc::new(Keyboard::start()),
            action_lock: Arc::new(tokio::sync::Mutex::new(())),
            tool_router: Self::tool_router(),
        }
    }

    /// Store the latest observation for this monitor (superseding earlier IDs)
    /// and return its JSON metadata block.
    fn record_observation(
        &self,
        monitor: &Monitor,
        w: u32,
        h: u32,
        generation: u64,
        fingerprint: u64,
        image_png: &[u8],
    ) -> serde_json::Value {
        let observation_id = Uuid::new_v4().to_string();
        let mut observations = self.state.observations.lock().unwrap();
        observations.retain(|_, o| o.monitor != monitor.name);
        observations.insert(
            observation_id.clone(),
            Observation {
                monitor: monitor.name.clone(),
                image_width: w,
                image_height: h,
                generation,
                fingerprint,
                image_png: (image_png.len() <= MAX_OCR_OBSERVATION_BYTES)
                    .then(|| Arc::new(image_png.to_vec())),
            },
        );
        serde_json::json!({
            "observation_id": observation_id,
            "monitor": Self::monitor_summary(monitor),
            "image": { "width_px": w, "height_px": h, "mime_type": "image/png" },
            "revision": self.state.revision(fingerprint),
            "actionable": self.state.actionable(),
        })
    }

    fn monitor_summary(m: &Monitor) -> serde_json::Value {
        let (w, h) = m.logical_size();
        serde_json::json!({
            "id": m.name,
            "description": m.description,
            "logical_bounds": { "x": m.x, "y": m.y, "width": w, "height": h },
            "scale": m.scale,
            "transform": m.transform,
        })
    }

    async fn acquire_action_lock<'a>(
        &'a self,
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<tokio::sync::MutexGuard<'a, ()>, &'static str> {
        self.acquire_action_lock_with_deadline(context, None).await
    }

    fn invalidate_after_input_attempt(&self, p: &ActionParams) {
        self.state.invalidate_observation(&p.observation_id);
        if let ActionKind::Drag {
            dst_observation_id, ..
        } = &p.action
        {
            self.state.invalidate_observation(dst_observation_id);
        }
    }

    async fn acquire_action_lock_with_deadline<'a>(
        &'a self,
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
        deadline: Option<Instant>,
    ) -> Result<tokio::sync::MutexGuard<'a, ()>, &'static str> {
        const QUEUED_CANCELLED: &str =
            "request cancelled while queued behind another action; no input was attempted";
        const QUEUED_TIMEOUT: &str =
            "goal timeout while queued behind another action; no input was attempted";
        let guard = if let Some(deadline) = deadline {
            let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
                return Err(QUEUED_TIMEOUT);
            };
            tokio::select! {
                guard = self.action_lock.lock() => guard,
                _ = context.ct.cancelled() => return Err(QUEUED_CANCELLED),
                _ = tokio::time::sleep(timeout) => return Err(QUEUED_TIMEOUT),
            }
        } else {
            tokio::select! {
                guard = self.action_lock.lock() => guard,
                _ = context.ct.cancelled() => return Err(QUEUED_CANCELLED),
            }
        };
        if context.ct.is_cancelled() {
            return Err(QUEUED_CANCELLED);
        }
        Ok(guard)
    }
}

fn ok_result(
    value: serde_json::Value,
    image_png: Option<Vec<u8>>,
) -> Result<CallToolResult, McpError> {
    let mut content = vec![Content::text(value.to_string())];
    if let Some(png) = image_png {
        content.push(Content::image(
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png),
            "image/png",
        ));
    }
    let mut result = CallToolResult::success(content);
    result.structured_content = Some(value);
    Ok(result)
}

fn err_result(code: &str, message: impl Into<String>) -> Result<CallToolResult, McpError> {
    let value = serde_json::json!({ "error": { "code": code, "message": message.into() } });
    let mut result = CallToolResult::error(vec![Content::text(value.to_string())]);
    result.structured_content = Some(value);
    Ok(result)
}

/// Rejection on the action path: no input was sent, so effect is `none`.
fn action_err(code: &str, message: impl Into<String>) -> Result<CallToolResult, McpError> {
    let value = serde_json::json!({
        "outcome": "rejected",
        "effect": "none",
        "error": { "code": code, "message": message.into() },
    });
    let mut result = CallToolResult::error(vec![Content::text(value.to_string())]);
    result.structured_content = Some(value);
    Ok(result)
}

fn backend_error_code(e: &BackendError) -> &'static str {
    match e {
        BackendError::Missing(_) => "MISSING_DEPENDENCY",
        BackendError::Timeout(_) => "BACKEND_TIMEOUT",
        BackendError::TooLarge(_) => "CAPTURE_TOO_LARGE",
        BackendError::Cancelled => "CANCELLED",
        BackendError::Failed(_) => "BACKEND_FAILED",
    }
}

fn backend_error(e: BackendError) -> Result<CallToolResult, McpError> {
    err_result(backend_error_code(&e), e.to_string())
}

/// Backend setup failures during action admission happen before input. Keep
/// the action contract explicit so Fast Path cannot mistake them for delivery.
fn action_backend_error(e: BackendError) -> Result<CallToolResult, McpError> {
    action_err(backend_error_code(&e), e.to_string())
}

#[derive(Debug)]
struct FastPathTimings {
    started: Instant,
    extraction_ms: u64,
    inference_ms: u64,
    revalidation_ms: u64,
    input_ms: u64,
    capture_ms: u64,
}

impl FastPathTimings {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            extraction_ms: 0,
            inference_ms: 0,
            revalidation_ms: 0,
            input_ms: 0,
            capture_ms: 0,
        }
    }

    fn value(&self) -> serde_json::Value {
        serde_json::json!({
            "extraction_ms": self.extraction_ms,
            "inference_ms": self.inference_ms,
            "revalidation_ms": self.revalidation_ms,
            "input_ms": self.input_ms,
            "capture_ms": self.capture_ms,
            "total_ms": self.started.elapsed().as_millis() as u64,
        })
    }
}

struct ElapsedTiming<'a> {
    started: Instant,
    target: &'a mut u64,
}

impl<'a> ElapsedTiming<'a> {
    fn new(target: &'a mut u64) -> Self {
        Self {
            started: Instant::now(),
            target,
        }
    }
}

impl Drop for ElapsedTiming<'_> {
    fn drop(&mut self) {
        *self.target = self.started.elapsed().as_millis() as u64;
    }
}

#[derive(Clone, Copy, Default)]
struct ActionTimings {
    input_ms: u64,
    capture_ms: u64,
}

/// One bounded Fast Path step moves through these phases in order; any failure
/// becomes a centralized handback before input, or a no-replay verification
/// result after dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FastPathStage {
    Validate,
    Extract,
    Infer,
    Revalidate,
    Dispatch,
    Verify,
    Handback,
}

impl FastPathStage {
    fn can_enter(self, next: Self) -> bool {
        self == next
            || next == Self::Handback
            || matches!(
                (self, next),
                (Self::Validate, Self::Extract)
                    | (Self::Extract, Self::Infer)
                    | (Self::Extract, Self::Verify)
                    | (Self::Infer, Self::Revalidate)
                    | (Self::Revalidate, Self::Dispatch)
                    | (Self::Dispatch, Self::Verify)
            )
    }
}

struct FastPathRun {
    observation_id: String,
    timings: FastPathTimings,
    stage: FastPathStage,
}

impl FastPathRun {
    fn new(observation_id: &str) -> Self {
        Self {
            observation_id: observation_id.into(),
            timings: FastPathTimings::new(),
            stage: FastPathStage::Validate,
        }
    }

    fn enter(&mut self, next: FastPathStage) {
        debug_assert!(self.stage.can_enter(next));
        self.stage = next;
    }

    fn deadline(&self, timeout_ms: u32) -> Instant {
        self.timings.started + Duration::from_millis(timeout_ms as u64)
    }

    fn handback(
        &mut self,
        reason: &str,
        extra: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        self.enter(FastPathStage::Handback);
        let mut value = serde_json::json!({
            "outcome": "handback",
            "status": "handback",
            "reason": reason,
            "effect": "none",
            "do_not_replay": false,
            "actions_executed": 0,
            "action_history": [],
            "progress": {
                "goal_complete": false,
            },
            "observation_id": self.observation_id,
            "timings_ms": self.timings.value(),
        });
        if let Some(serde_json::Value::Object(fields)) = extra
            && let serde_json::Value::Object(target) = &mut value
        {
            target.extend(fields);
        }
        let mut result = CallToolResult::error(vec![Content::text(value.to_string())]);
        result.structured_content = Some(value);
        Ok(result)
    }
}

struct FastPathGoalProgress {
    started: Instant,
    actions_executed: u8,
    action_history: Vec<serde_json::Value>,
    selections: Vec<serde_json::Value>,
    steps: Vec<serde_json::Value>,
    extraction_ms: u64,
    inference_ms: u64,
    revalidation_ms: u64,
    input_ms: u64,
    capture_ms: u64,
    latest_effect: &'static str,
    do_not_replay: bool,
}

impl FastPathGoalProgress {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            actions_executed: 0,
            action_history: Vec::new(),
            selections: Vec::new(),
            steps: Vec::new(),
            extraction_ms: 0,
            inference_ms: 0,
            revalidation_ms: 0,
            input_ms: 0,
            capture_ms: 0,
            latest_effect: "none",
            do_not_replay: false,
        }
    }

    fn absorb(&mut self, value: &serde_json::Value) {
        let action_count = value["actions_executed"].as_u64().unwrap_or(0) as u8;
        self.actions_executed = self.actions_executed.saturating_add(action_count);
        if let Some(actions) = value["action_history"].as_array() {
            self.action_history.extend(actions.iter().cloned());
        }
        let selection = value.get("selection").filter(|value| value.is_object());
        if let Some(selection) = selection {
            self.selections.push(selection.clone());
        }
        let effect = value["effect"].as_str().unwrap_or("none");
        if effect != "none" {
            self.latest_effect = match effect {
                "completed" => "completed",
                "partial" => "partial",
                _ => "unknown",
            };
        }
        self.do_not_replay |= value["do_not_replay"].as_bool().unwrap_or(false);

        let timings = value.get("timings_ms").cloned().unwrap_or_default();
        self.extraction_ms = self
            .extraction_ms
            .saturating_add(timings["extraction_ms"].as_u64().unwrap_or(0));
        self.inference_ms = self
            .inference_ms
            .saturating_add(timings["inference_ms"].as_u64().unwrap_or(0));
        self.revalidation_ms = self
            .revalidation_ms
            .saturating_add(timings["revalidation_ms"].as_u64().unwrap_or(0));
        self.input_ms = self
            .input_ms
            .saturating_add(timings["input_ms"].as_u64().unwrap_or(0));
        self.capture_ms = self
            .capture_ms
            .saturating_add(timings["capture_ms"].as_u64().unwrap_or(0));
        self.steps.push(serde_json::json!({
            "step": self.steps.len() + 1,
            "reason": value.get("reason"),
            "effect": effect,
            "actions_executed": action_count,
            "selection": selection,
            "timings_ms": timings,
        }));
    }

    fn add_refresh(&mut self, elapsed: Duration) {
        let milliseconds = elapsed.as_millis() as u64;
        self.capture_ms = self.capture_ms.saturating_add(milliseconds);
        self.steps.push(serde_json::json!({
            "step": self.steps.len() + 1,
            "kind": "reobserve",
            "effect": "none",
            "timings_ms": {"capture_ms": milliseconds},
        }));
    }

    fn result(
        &self,
        mut value: serde_json::Value,
        reason: &str,
        completed: bool,
        max_actions: u8,
        refreshes: u8,
        observation_id: &str,
        observation: Option<&serde_json::Value>,
    ) -> serde_json::Value {
        value["outcome"] = serde_json::json!(if completed { "completed" } else { "handback" });
        value["status"] = serde_json::json!(if completed { "completed" } else { "handback" });
        value["reason"] = serde_json::json!(reason);
        value["effect"] = serde_json::json!(self.latest_effect);
        // If a later step hands back, rerunning the whole Goal may repeat an
        // earlier delivered Action. Preserve that signal at Goal level.
        value["do_not_replay"] =
            serde_json::json!(!completed && (self.do_not_replay || self.actions_executed > 0));
        value["actions_executed"] = serde_json::json!(self.actions_executed);
        value["action_history"] = serde_json::json!(self.action_history);
        value["selections"] = serde_json::json!(self.selections);
        value["steps"] = serde_json::json!(self.steps);
        value["refreshes"] = serde_json::json!(refreshes);
        value["observation_id"] = serde_json::json!(observation_id);
        if let Some(last_selection) = self.selections.last() {
            value["selection"] = last_selection.clone();
        }
        if let Some(observation) = observation {
            value["observation"] = observation.clone();
            value["final_observation"] = observation.clone();
        }
        value["progress"] = serde_json::json!({
            "goal_complete": completed,
            "completion_observed": completed,
            "actions_executed": self.actions_executed,
            "max_actions": max_actions,
            "remaining_actions": max_actions.saturating_sub(self.actions_executed),
            "refreshes": refreshes,
        });
        value["timings_ms"] = serde_json::json!({
            "extraction_ms": self.extraction_ms,
            "inference_ms": self.inference_ms,
            "revalidation_ms": self.revalidation_ms,
            "input_ms": self.input_ms,
            "capture_ms": self.capture_ms,
            "total_ms": self.started.elapsed().as_millis() as u64,
        });
        value
    }

    fn response(
        &self,
        value: serde_json::Value,
        reason: &str,
        completed: bool,
        max_actions: u8,
        refreshes: u8,
        observation_id: &str,
        observation: Option<&serde_json::Value>,
        images: Vec<Content>,
    ) -> CallToolResult {
        wrap_goal_result(
            self.result(
                value,
                reason,
                completed,
                max_actions,
                refreshes,
                observation_id,
                observation,
            ),
            images,
            !completed,
        )
    }
}

fn wrap_goal_result(
    value: serde_json::Value,
    images: Vec<Content>,
    is_error: bool,
) -> CallToolResult {
    let mut content = vec![Content::text(value.to_string())];
    content.extend(images);
    let mut result = if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    result.structured_content = Some(value);
    result
}

struct FastPathHandback {
    reason: &'static str,
    extra: Option<serde_json::Value>,
}

impl FastPathHandback {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            extra: None,
        }
    }

    fn with_selection(reason: &'static str, selection: &serde_json::Value) -> Self {
        Self {
            reason,
            extra: Some(serde_json::json!({ "selection": selection })),
        }
    }

    fn respond(self, run: &mut FastPathRun) -> Result<CallToolResult, McpError> {
        run.handback(self.reason, self.extra)
    }
}

struct FastPathPrepared {
    evidence: accessibility::Evidence,
    plans: Vec<fast_path::CandidatePlan>,
}

enum FastPathExtraction {
    AlreadyComplete,
    Ready(FastPathPrepared),
}

struct FastPathSelection {
    selection: typesafe::Selection,
    selection_json: serde_json::Value,
    candidate: fast_path::CandidatePlan,
}

#[derive(Clone)]
struct DispatchWindowBinding {
    class: String,
    title: String,
    address: String,
    pid: i64,
    monitor_id: i64,
    geometry: accessibility::WindowGeometry,
}

struct BrowserCandidateBinding {
    // The compositor window can remain unchanged across a same-title tab switch.
    target: fast_path::TargetScope,
    evidence_revision: String,
    browser_window_id: String,
    browser_tab_id: String,
    document_id: String,
    window_id: String,
}

impl DispatchWindowBinding {
    fn matches(&self, window: &serde_json::Value) -> bool {
        window["address"].as_str() == Some(self.address.as_str())
            && window["title"].as_str() == Some(self.title.as_str())
            && window["class"].as_str() == Some(self.class.as_str())
            && window["monitor"].as_i64() == Some(self.monitor_id)
            && window["pid"].as_i64() == Some(self.pid)
            && active_window_geometry(window) == Some(self.geometry)
    }
}

struct FastPathAdmission<'a> {
    serial: tokio::sync::MutexGuard<'a, ()>,
    candidate: fast_path::CandidatePlan,
    action: ActionKind,
    dispatch_window_id: String,
    dispatch_pid: i64,
    expected_active_window: Option<DispatchWindowBinding>,
    browser_candidate: Option<BrowserCandidateBinding>,
    action_budget: Duration,
}

fn fast_action_history(
    candidate: &fast_path::CandidatePlan,
    effect: fast_path::ActionEffect,
    do_not_replay: bool,
) -> serde_json::Value {
    if effect == fast_path::ActionEffect::None {
        return serde_json::json!([]);
    }
    serde_json::json!([{
        "candidate_id": candidate.id,
        "description": candidate.description,
        "target_element_id": candidate.element_id,
        "action": {
            "kind": candidate.action.kind(),
            "literal": if matches!(&candidate.action, fast_path::ActionSpec::TypeText { .. }) {
                "redacted"
            } else {
                "not_applicable"
            },
        },
        "effect": effect.as_str(),
        "do_not_replay": do_not_replay,
    }])
}

fn wrap_fast_action_result(
    value: serde_json::Value,
    action_result: CallToolResult,
    is_error: bool,
) -> Result<CallToolResult, McpError> {
    // Preserve only the fresh screenshot image. The existing action result's
    // text can contain type_text input, so it is intentionally not copied into
    // the Fast Path response.
    let images = action_result
        .content
        .iter()
        .filter_map(|block| block.as_image().map(|_| block.clone()))
        .collect::<Vec<_>>();
    let mut content = vec![Content::text(value.to_string())];
    content.extend(images);
    let mut result = if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    result.structured_content = Some(value);
    Ok(result)
}

fn validation_reason(error: fast_path::ValidationError) -> &'static str {
    match error {
        fast_path::ValidationError::Invalid(message) => {
            let _ = message;
            "invalid_goal_schema"
        }
        fast_path::ValidationError::Unsupported(message) => {
            let _ = message;
            "unsupported_goal"
        }
    }
}

fn evidence_reason(error: accessibility::EvidenceError) -> &'static str {
    match error {
        accessibility::EvidenceError::Missing => "accessibility_dependency_missing",
        accessibility::EvidenceError::Timeout => "accessibility_timeout",
        accessibility::EvidenceError::Failed(message) => {
            let _ = message;
            "accessibility_unavailable"
        }
        accessibility::EvidenceError::Malformed => "malformed_accessibility_evidence",
        accessibility::EvidenceError::LocalOcrUnavailable => "local_ocr_unavailable",
    }
}

async fn run_with_goal_budget<F, T>(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    deadline: Instant,
    future: F,
) -> Option<T>
where
    F: Future<Output = T>,
{
    let timeout = deadline.checked_duration_since(Instant::now())?;
    tokio::select! {
        result = future => Some(result),
        _ = tokio::time::sleep(timeout) => None,
        _ = context.ct.cancelled() => None,
    }
}

async fn ocr_completion_window_is_current(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    target: &fast_path::TargetScope,
    dispatch_window_id: &str,
    dispatch_pid: Option<i64>,
    native_window_id: &str,
    deadline: Instant,
) -> bool {
    let Some(dispatch_pid) = dispatch_pid.filter(|pid| *pid > 0) else {
        return false;
    };
    if atspi_process_id(native_window_id) != Some(dispatch_pid) {
        return false;
    }
    let Some(Ok(window)) = run_with_goal_budget(context, deadline, backend::active_window()).await
    else {
        return false;
    };
    ocr_completion_window_matches(&window, target, dispatch_window_id, dispatch_pid)
}

async fn read_active_window_for_dispatch(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    deadline: Instant,
) -> Result<serde_json::Value, &'static str> {
    let Some(window) = run_with_goal_budget(context, deadline, backend::active_window()).await
    else {
        return Err(if context.ct.is_cancelled() {
            "cancelled"
        } else {
            "goal_timeout_before_window_check"
        });
    };
    window.map_err(|_| "active_window_unavailable_at_dispatch")
}

async fn validate_input_window(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    expected: Option<&DispatchWindowBinding>,
    browser_candidate: Option<&BrowserCandidateBinding>,
    monitor: &Monitor,
) -> Result<(), &'static str> {
    let Some(expected) = expected else {
        return if browser_candidate.is_some() {
            Err("target_window_unavailable_before_input")
        } else {
            Ok(())
        };
    };
    let window = tokio::select! {
        result = backend::active_window() => result.map_err(|_| "active_window_unavailable_before_input")?,
        _ = context.ct.cancelled() => return Err("cancelled"),
    };
    if expected.matches(&window) {
        if let Some(binding) = browser_candidate {
            let timeout = Duration::from_secs(2);
            let deadline = Instant::now() + timeout;
            let evidence = run_with_goal_budget(
                context,
                deadline,
                collect_fast_evidence(
                    context,
                    fast_path::EvidenceSourceKind::BrowserExtension,
                    &binding.target,
                    timeout,
                ),
            )
            .await
            .and_then(Result::ok)
            .ok_or("browser_target_unavailable_before_input")?;
            let evidence = normalize_fast_evidence(
                context,
                fast_path::EvidenceSourceKind::BrowserExtension,
                &binding.target,
                monitor,
                deadline,
                evidence,
            )
            .await
            .map_err(|_| "browser_target_changed_before_input")?;
            if !browser_candidate_matches(binding, &evidence) {
                return Err("browser_target_changed_before_input");
            }
            let latest = tokio::select! {
                result = backend::active_window() => result.map_err(|_| "active_window_unavailable_before_input")?,
                _ = context.ct.cancelled() => return Err("cancelled"),
            };
            if !expected.matches(&latest) {
                return Err("target_window_changed_before_input");
            }
        }
        Ok(())
    } else {
        Err("target_window_changed_before_input")
    }
}

fn browser_candidate_matches(
    expected: &BrowserCandidateBinding,
    evidence: &accessibility::Evidence,
) -> bool {
    evidence.source.source_kind == "browser_extension"
        && evidence.browser_actions_authorized
        && evidence.source.active_tab
        && evidence.source.visible
        && evidence.source.focused
        && !evidence.source.occluded
        && evidence.source.geometry_verified
        && evidence.source.browser_window_id == expected.browser_window_id
        && evidence.source.browser_tab_id == expected.browser_tab_id
        && evidence.source.document_id == expected.document_id
        && evidence.source.window_id == expected.window_id
        && evidence.revision() == expected.evidence_revision
}

fn ocr_completion_window_matches(
    window: &serde_json::Value,
    target: &fast_path::TargetScope,
    dispatch_window_id: &str,
    dispatch_pid: i64,
) -> bool {
    window["address"].as_str() == Some(dispatch_window_id)
        && window["pid"].as_i64() == Some(dispatch_pid)
        && window["title"].as_str() == target.window.as_deref()
        && window["class"]
            .as_str()
            .is_some_and(|class| class.eq_ignore_ascii_case(&target.application))
}

fn active_window_geometry(window: &serde_json::Value) -> Option<accessibility::WindowGeometry> {
    let at = window["at"].as_array()?;
    let size = window["size"].as_array()?;
    if at.len() != 2 || size.len() != 2 {
        return None;
    }
    let geometry = accessibility::WindowGeometry {
        x: at[0].as_f64()?,
        y: at[1].as_f64()?,
        width: size[0].as_f64()?,
        height: size[1].as_f64()?,
    };
    [geometry.x, geometry.y, geometry.width, geometry.height]
        .iter()
        .all(|value| value.is_finite())
        .then_some(geometry)
        .filter(|geometry| geometry.width > 0.0 && geometry.height > 0.0)
}

fn dispatch_window_binding(
    source: fast_path::EvidenceSourceKind,
    target: &fast_path::TargetScope,
    evidence: &accessibility::Evidence,
    monitor_id: i64,
    window: &serde_json::Value,
) -> Option<DispatchWindowBinding> {
    let class = window["class"].as_str()?;
    let title = window["title"].as_str()?;
    let address = window["address"].as_str()?;
    let active_pid = window["pid"].as_i64().filter(|pid| *pid > 0)?;
    let active_monitor_id = window["monitor"].as_i64()?;
    let geometry = active_window_geometry(window)?;
    if address.is_empty()
        || active_monitor_id != monitor_id
        || target
            .window
            .as_deref()
            .is_some_and(|expected| expected != evidence.source.window)
    {
        return None;
    }

    let class_matches = match source {
        fast_path::EvidenceSourceKind::NativeAccessibility => {
            normalized_app_identity(class) == normalized_app_identity(&target.application)
                && title == evidence.source.window
                && atspi_process_id(&evidence.source.window_id).is_some_and(|pid| pid == active_pid)
        }
        fast_path::EvidenceSourceKind::BrowserExtension => {
            let expected_family = match target.application.as_str() {
                "Firefox" => "firefox",
                "Chrome" => "chrome",
                _ => return None,
            };
            class.to_ascii_lowercase().contains(expected_family)
                && title.contains(&evidence.source.window)
        }
        fast_path::EvidenceSourceKind::LocalOcr => {
            normalized_app_identity(class) == normalized_app_identity(&target.application)
                && title == evidence.source.window
                && address == evidence.source.window_id
                && evidence.source.process_id == Some(active_pid)
                && evidence.window_geometry == Some(geometry)
        }
    };
    if !class_matches {
        return None;
    }

    if source == fast_path::EvidenceSourceKind::BrowserExtension
        && evidence.window_geometry != Some(geometry)
    {
        return None;
    }

    Some(DispatchWindowBinding {
        class: class.to_owned(),
        title: title.to_owned(),
        address: address.to_owned(),
        pid: active_pid,
        monitor_id: active_monitor_id,
        geometry,
    })
}

fn normalized_app_identity(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn atspi_process_id(window_id: &str) -> Option<i64> {
    let pid = window_id.split(':').next()?.parse::<i64>().ok()?;
    (pid > 0).then_some(pid)
}

async fn run_until_cancelled<F, T>(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    future: F,
) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::select! {
        result = future => Some(result),
        _ = context.ct.cancelled() => None,
    }
}

async fn collect_fast_evidence(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    source: fast_path::EvidenceSourceKind,
    target: &fast_path::TargetScope,
    timeout: Duration,
) -> Result<accessibility::Evidence, accessibility::EvidenceError> {
    let collect = async {
        match source {
            fast_path::EvidenceSourceKind::NativeAccessibility => {
                accessibility::collect(
                    target.application.as_str(),
                    target.window.as_deref(),
                    timeout,
                )
                .await
            }
            fast_path::EvidenceSourceKind::BrowserExtension => {
                accessibility::collect_browser(
                    target.application.as_str(),
                    target.window.as_deref(),
                    timeout,
                )
                .await
            }
            fast_path::EvidenceSourceKind::LocalOcr => Err(accessibility::EvidenceError::Missing),
        }
    };
    tokio::pin!(collect);
    tokio::select! {
        result = &mut collect => result,
        _ = context.ct.cancelled() => Err(accessibility::EvidenceError::Failed("request cancelled")),
    }
}

async fn collect_fast_completion_evidence(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    source: fast_path::EvidenceSourceKind,
    target: &fast_path::TargetScope,
    completion: &fast_path::CompletionCondition,
    timeout: Duration,
) -> Result<accessibility::Evidence, accessibility::EvidenceError> {
    let collect = async {
        match source {
            fast_path::EvidenceSourceKind::NativeAccessibility => {
                accessibility::collect_completion(
                    &target.application,
                    target.window.as_deref(),
                    &completion.name,
                    completion.role.as_deref(),
                    timeout,
                )
                .await
            }
            fast_path::EvidenceSourceKind::BrowserExtension => {
                accessibility::collect_browser_completion(
                    &target.application,
                    target.window.as_deref(),
                    &completion.name,
                    completion.role.as_deref(),
                    timeout,
                )
                .await
            }
            // OCR can select a caller-attested navigation tab, but its text
            // cannot prove task completion. Completion remains native AX.
            fast_path::EvidenceSourceKind::LocalOcr => {
                accessibility::collect_completion(
                    &target.application,
                    target.window.as_deref(),
                    &completion.name,
                    completion.role.as_deref(),
                    timeout,
                )
                .await
            }
        }
    };
    tokio::pin!(collect);
    tokio::select! {
        result = &mut collect => result,
        _ = context.ct.cancelled() => Err(accessibility::EvidenceError::Failed("request cancelled")),
    }
}

async fn normalize_fast_evidence(
    context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
    source: fast_path::EvidenceSourceKind,
    target: &fast_path::TargetScope,
    monitor: &Monitor,
    deadline: Instant,
    mut evidence: accessibility::Evidence,
) -> Result<accessibility::Evidence, &'static str> {
    if source == fast_path::EvidenceSourceKind::NativeAccessibility
        && evidence.coordinate_space == "unknown"
    {
        let window = run_with_goal_budget(context, deadline, backend::active_window())
            .await
            .ok_or("native_window_identity_unavailable")?
            .map_err(|_| "native_window_identity_unavailable")?;
        map_native_coordinates(&mut evidence, target, monitor, &window)?;
        return Ok(evidence);
    }
    if source != fast_path::EvidenceSourceKind::BrowserExtension {
        return Ok(evidence);
    }
    let (window, clients) = tokio::join!(
        run_with_goal_budget(context, deadline, backend::active_window()),
        run_with_goal_budget(context, deadline, backend::clients()),
    );
    let Some(window) = window else {
        return Err("browser_window_identity_unavailable");
    };
    let window = window.map_err(|_| "browser_window_identity_unavailable")?;
    let Some(clients) = clients else {
        return Err("browser_window_identity_unavailable");
    };
    let clients = clients.map_err(|_| "browser_window_identity_unavailable")?;
    let geometry = if evidence.source.application == "Chrome" {
        let pid = window["pid"]
            .as_i64()
            .ok_or("browser_process_unavailable")?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("browser_geometry_timeout")?;
        Some(
            run_with_goal_budget(
                context,
                deadline,
                accessibility::browser_geometry(pid, &evidence.source.window, remaining),
            )
            .await
            .ok_or("browser_geometry_timeout")?
            .map_err(|_| "browser_accessibility_geometry_unavailable")?,
        )
    } else {
        None
    };
    map_browser_coordinates(
        &mut evidence,
        target,
        monitor,
        &window,
        &clients,
        geometry.as_ref(),
    )?;
    evidence.browser_actions_authorized = browser_origin_allowed(
        &evidence.source.browser_origin,
        &std::env::var("COMPUTER_USE_BROWSER_ALLOWED_ORIGINS").unwrap_or_default(),
    );
    Ok(evidence)
}

/// AT-SPI SCREEN coordinates from XWayland are physical. Admit the mapping
/// only when the exact process/title and all four frame extents match Hyprland.
/// Other toolkit coordinate contracts remain explicit, never guessed.
fn map_native_coordinates(
    evidence: &mut accessibility::Evidence,
    target: &fast_path::TargetScope,
    monitor: &Monitor,
    window: &serde_json::Value,
) -> Result<(), &'static str> {
    if monitor.transform != 0
        || !monitor.scale.is_finite()
        || monitor.scale <= 0.0
        || window["xwayland"].as_bool() != Some(true)
        || dispatch_window_binding(
            fast_path::EvidenceSourceKind::NativeAccessibility,
            target,
            evidence,
            monitor.id,
            window,
        )
        .is_none()
    {
        return Err("native_coordinate_mapping_unverified");
    }
    let geometry = active_window_geometry(window).ok_or("native_window_geometry_unavailable")?;
    let frame = evidence
        .native_frame
        .ok_or("native_frame_geometry_unavailable")?;
    let expected = [geometry.x, geometry.y, geometry.width, geometry.height];
    if frame.iter().zip(expected).any(|(actual, logical)| {
        !actual.is_finite() || (actual - logical * monitor.scale).abs() > 1.01
    }) {
        return Err("native_frame_geometry_mismatch");
    }
    for element in &mut evidence.elements {
        element.x /= monitor.scale;
        element.y /= monitor.scale;
        element.width /= monitor.scale;
        element.height /= monitor.scale;
    }
    evidence.coordinate_space = "desktop_logical".into();
    evidence.window_geometry = Some(geometry);
    Ok(())
}

fn browser_origin_allowed(origin: &str, policy: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.origin().ascii_serialization() != origin
        || origin.len() > 512
    {
        return false;
    }
    let Ok(origins) = serde_json::from_str::<Vec<String>>(policy) else {
        return false;
    };
    origins.len() <= 64 && origins.iter().any(|allowed| allowed == origin)
}

fn chrome_viewport_geometry(
    geometry: &accessibility::BrowserGeometry,
    viewport: &accessibility::BrowserViewport,
    window: &serde_json::Value,
    scale: f64,
) -> Result<(f64, f64, f64, f64), &'static str> {
    let fail = "browser_accessibility_geometry_mismatch";
    let [fx, fy, fw, fh] = geometry.frame;
    let [dx, dy, dw, dh] = geometry.document;
    let wx = window["at"][0].as_f64().ok_or(fail)?;
    let wy = window["at"][1].as_f64().ok_or(fail)?;
    let ww = window["size"][0].as_f64().ok_or(fail)?;
    let wh = window["size"][1].as_f64().ok_or(fail)?;
    if geometry.process_id <= 0
        || window["pid"].as_i64() != Some(geometry.process_id)
        || ![fx, fy, fw, fh, dx, dy, dw, dh, wx, wy, ww, wh, scale]
            .iter()
            .all(|n| n.is_finite())
        || [
            fw,
            fh,
            dw,
            dh,
            ww,
            wh,
            scale,
            viewport.width,
            viewport.height,
            viewport.device_pixel_ratio,
        ]
        .iter()
        .any(|n| !n.is_finite() || *n <= 0.0)
        || (dw - viewport.width * viewport.device_pixel_ratio).abs() > 1.01
        || (dh - viewport.height * viewport.device_pixel_ratio).abs() > 1.01
    {
        return Err(fail);
    }
    let (x, y) = match window["xwayland"].as_bool() {
        Some(true) => {
            if (fx - wx * scale).abs() > 1.01
                || (fy - wy * scale).abs() > 1.01
                || (fw - ww * scale).abs() > 1.01
                || (fh - wh * scale).abs() > 1.01
            {
                return Err(fail);
            }
            (wx + (dx - fx) / scale, wy + (dy - fy) / scale)
        }
        Some(false) => {
            // Chrome's Wayland frame uses logical window-local coordinates;
            // its web document uses physical window-local coordinates.
            if fx.abs() > 0.01
                || fy.abs() > 0.01
                || (fw - ww).abs() > 0.01
                || (fh - wh).abs() > 0.01
            {
                return Err(fail);
            }
            (wx + dx / scale, wy + dy / scale)
        }
        None => return Err(fail),
    };
    Ok((
        x,
        y,
        dw / viewport.width / scale,
        dh / viewport.height / scale,
    ))
}

fn map_browser_coordinates(
    evidence: &mut accessibility::Evidence,
    target: &fast_path::TargetScope,
    monitor: &Monitor,
    active_window: &serde_json::Value,
    clients: &[serde_json::Value],
    chrome_geometry: Option<&accessibility::BrowserGeometry>,
) -> Result<(), &'static str> {
    if evidence.source.source_kind != "browser_extension"
        || evidence.coordinate_space != "browser_viewport_css"
        || !evidence.source.active_tab
        || !evidence.source.focused
        || !evidence.source.visible
        || evidence.source.occluded
        || (!evidence.source.geometry_verified && chrome_geometry.is_none())
        || evidence.truncated
        || target
            .window_id
            .as_ref()
            .is_some_and(|id| id != &evidence.source.window_id)
    {
        return Err("browser_source_provenance_or_target_mismatch");
    }
    let browser_class = active_window["class"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let expected_class = match evidence.source.application.as_str() {
        "Firefox" => "firefox",
        "Chrome" => "chrome",
        _ => return Err("unsupported_browser_application"),
    };
    let title = active_window["title"].as_str().unwrap_or_default();
    let Some(xwayland) = active_window["xwayland"].as_bool() else {
        return Err("browser_window_is_not_the_focused_supported_surface");
    };
    if !browser_class.contains(expected_class)
        || !title.contains(&evidence.source.window)
        || active_window["monitor"].as_i64() != Some(monitor.id)
        || monitor.transform != 0
    {
        return Err("browser_window_is_not_the_focused_supported_surface");
    }
    let Some(active_address) = active_window["address"].as_str() else {
        return Err("browser_window_identity_unavailable");
    };
    let matching_clients = clients
        .iter()
        .filter(|client| {
            let class = client["class"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let title = client["title"].as_str().unwrap_or_default();
            class.contains(expected_class) && title.contains(&evidence.source.window)
        })
        .collect::<Vec<_>>();
    if matching_clients.len() != 1
        || matching_clients[0]["address"].as_str() != Some(active_address)
    {
        return Err("browser_window_identity_is_ambiguous");
    }
    let Some(viewport) = evidence.browser_viewport.as_ref() else {
        return Err("browser_viewport_geometry_missing");
    };
    let values = [
        viewport.screen_x,
        viewport.screen_y,
        viewport.width,
        viewport.height,
        viewport.device_pixel_ratio,
        viewport.visual_scale,
    ];
    if !values.iter().all(|value| value.is_finite())
        || viewport.width <= 0.0
        || viewport.height <= 0.0
        || viewport.visual_scale != 1.0
        || viewport.device_pixel_ratio <= 0.0
        || (expected_class != "chrome"
            && (viewport.device_pixel_ratio - monitor.scale).abs() > 0.02)
    {
        return Err("browser_zoom_or_display_scale_is_unverified");
    }
    let Some(at) = active_window["at"].as_array() else {
        return Err("browser_window_bounds_unavailable");
    };
    let Some(size) = active_window["size"].as_array() else {
        return Err("browser_window_bounds_unavailable");
    };
    if at.len() != 2 || size.len() != 2 {
        return Err("browser_window_bounds_unavailable");
    }
    let (Some(window_x), Some(window_y), Some(window_width), Some(window_height)) = (
        at[0].as_f64(),
        at[1].as_f64(),
        size[0].as_f64(),
        size[1].as_f64(),
    ) else {
        return Err("browser_window_bounds_unavailable");
    };
    // Firefox on native Wayland reports the viewport origin relative to its
    // browser window. XWayland Firefox reports desktop screen coordinates.
    let (viewport_x, viewport_y, css_scale_x, css_scale_y) = if expected_class == "chrome" {
        let geometry = chrome_geometry.ok_or("browser_accessibility_geometry_unavailable")?;
        chrome_viewport_geometry(geometry, viewport, active_window, monitor.scale)?
    } else if xwayland {
        (viewport.screen_x, viewport.screen_y, 1.0, 1.0)
    } else {
        (
            window_x + viewport.screen_x,
            window_y + viewport.screen_y,
            1.0,
            1.0,
        )
    };
    let viewport_right = viewport_x + viewport.width * css_scale_x;
    let viewport_bottom = viewport_y + viewport.height * css_scale_y;
    if viewport_x < window_x
        || viewport_y < window_y
        || viewport_right > window_x + window_width
        || viewport_bottom > window_y + window_height
    {
        return Err("browser_viewport_is_outside_the_focused_window");
    }
    evidence.window_geometry = Some(accessibility::WindowGeometry {
        x: window_x,
        y: window_y,
        width: window_width,
        height: window_height,
    });
    for element in &mut evidence.elements {
        if !matches!(
            element.role.as_str(),
            "tab" | "label" | "status" | "text field" | "media title" | "media play"
        ) || !element.visible
            || !element.showing
            || element.x < 0.0
            || element.y < 0.0
            || element.width <= 0.0
            || element.height <= 0.0
            || element.x + element.width > viewport.width
            || element.y + element.height > viewport.height
        {
            return Err("browser_element_geometry_is_unverified");
        }
        element.x = viewport_x + element.x * css_scale_x;
        element.y = viewport_y + element.y * css_scale_y;
        element.width *= css_scale_x;
        element.height *= css_scale_y;
    }
    evidence.source.geometry_verified = true;
    evidence.coordinate_space = "desktop_logical".into();
    Ok(())
}

fn validate_ocr_window(
    target: &fast_path::TargetScope,
    monitor: &Monitor,
    observation: &Observation,
    region: &fast_path::OcrRegion,
    active_window: &serde_json::Value,
) -> Result<(String, String, accessibility::WindowGeometry, i64), accessibility::EvidenceError> {
    let class = active_window["class"].as_str().unwrap_or_default();
    let title = active_window["title"].as_str().unwrap_or_default();
    let address = active_window["address"].as_str().unwrap_or_default();
    let process_id = active_window["pid"]
        .as_i64()
        .filter(|pid| *pid > 0)
        .ok_or(accessibility::EvidenceError::Malformed)?;
    if !class.eq_ignore_ascii_case(&target.application)
        || title != target.window.as_deref().unwrap_or_default()
        || title.is_empty()
        || address.is_empty()
        || target
            .window_id
            .as_deref()
            .is_some_and(|expected| expected != address)
        || active_window["monitor"].as_i64() != Some(monitor.id)
    {
        return Err(accessibility::EvidenceError::Failed(
            "OCR target window identity changed",
        ));
    }
    let geometry = active_window_geometry(active_window).ok_or(
        accessibility::EvidenceError::Failed("OCR target window bounds unavailable or invalid"),
    )?;
    let (logical_width, logical_height) = monitor.logical_size();
    let scale_x = logical_width as f64 / observation.image_width as f64;
    let scale_y = logical_height as f64 / observation.image_height as f64;
    let region_left = monitor.x as f64 + region.x as f64 * scale_x;
    let region_top = monitor.y as f64 + region.y as f64 * scale_y;
    let region_right = monitor.x as f64 + (region.x + region.width) as f64 * scale_x;
    let region_bottom = monitor.y as f64 + (region.y + region.height) as f64 * scale_y;
    if region_left < geometry.x
        || region_top < geometry.y
        || region_right > geometry.x + geometry.width
        || region_bottom > geometry.y + geometry.height
    {
        return Err(accessibility::EvidenceError::Failed(
            "OCR region is outside the focused target window",
        ));
    }
    Ok((address.to_owned(), title.to_owned(), geometry, process_id))
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tool_router]
impl ComputerUse {
    /// List connected, selectable Monitors with logical geometry, scale,
    /// orientation, and the current opaque Display Configuration revision.
    /// Read-only.
    #[tool(name = "computer_monitors")]
    async fn computer_monitors(&self) -> Result<CallToolResult, McpError> {
        let monitors = match backend::monitors().await.map(backend::selectable) {
            Ok(m) => m,
            Err(e) => return backend_error(e),
        };
        let fingerprint = backend::fingerprint(&monitors);
        ok_result(
            serde_json::json!({
                "revision": self.state.revision(fingerprint),
                "events_healthy": self.state.events_healthy.load(Ordering::SeqCst),
                "wayland_events_healthy": self.state.wayland_healthy.load(Ordering::SeqCst),
                "monitors": monitors.iter().map(Self::monitor_summary).collect::<Vec<_>>(),
            }),
            None,
        )
    }

    /// Capture a screenshot of one Monitor. Returns a PNG image plus an opaque
    /// Observation ID, the actual image dimensions, and the Display
    /// Configuration revision the image belongs to. Read-only.
    #[tool(name = "computer_observe")]
    async fn computer_observe(
        &self,
        Parameters(p): Parameters<ObserveParams>,
    ) -> Result<CallToolResult, McpError> {
        // Capture bracketed by configuration checks; retry once if the display
        // configuration races the capture, then refuse rather than issue an
        // observation whose coordinate space may already be wrong.
        for _ in 0..2 {
            let pre = match backend::monitors().await.map(backend::selectable) {
                Ok(m) => m,
                Err(e) => return backend_error(e),
            };
            let Some(monitor) = pre.iter().find(|m| m.name == p.monitor) else {
                return err_result(
                    "MONITOR_NOT_FOUND",
                    format!(
                        "no selectable monitor {:?}; call computer_monitors",
                        p.monitor
                    ),
                );
            };
            let gen_pre = self.state.generation.load(Ordering::SeqCst);
            let fp_pre = backend::fingerprint(&pre);
            let png = match backend::capture(&monitor.name).await {
                Ok(p) => p,
                Err(e) => return backend_error(e),
            };
            let post = match backend::monitors().await.map(backend::selectable) {
                Ok(m) => m,
                Err(e) => return backend_error(e),
            };
            let gen_post = self.state.generation.load(Ordering::SeqCst);
            let fp_post = backend::fingerprint(&post);
            if gen_pre != gen_post || fp_pre != fp_post {
                continue;
            }
            let Some((w, h)) = backend::png_size(&png) else {
                return err_result("CAPTURE_FAILED", "grim output was not a PNG");
            };
            let observation_json = self.record_observation(monitor, w, h, gen_post, fp_post, &png);
            return ok_result(observation_json, Some(png));
        }
        err_result(
            "DISPLAY_CONFIGURATION_CHANGED",
            "display configuration changed during capture; call computer_observe again",
        )
    }

    async fn collect_ocr_evidence_for_observation(
        &self,
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
        observation_id: &str,
        target: &fast_path::TargetScope,
        region: &fast_path::OcrRegion,
        languages: &[fast_path::OcrLanguage],
        fresh_capture: bool,
        timeout: Duration,
    ) -> Result<accessibility::Evidence, accessibility::EvidenceError> {
        let deadline = Instant::now() + timeout;
        let Some(snapshot) = run_with_goal_budget(context, deadline, async {
            backend::monitors().await.map(backend::selectable)
        })
        .await
        else {
            return Err(accessibility::EvidenceError::Timeout);
        };
        let snapshot = snapshot.map_err(|_| {
            accessibility::EvidenceError::Failed("display snapshot unavailable for OCR")
        })?;
        let fingerprint = backend::fingerprint(&snapshot);
        let observation = self
            .state
            .validate_observation(observation_id, fingerprint)
            .map_err(|_| accessibility::EvidenceError::Failed("stale OCR observation"))?;
        let monitor = snapshot
            .iter()
            .find(|monitor| monitor.name == observation.monitor)
            .cloned()
            .ok_or(accessibility::EvidenceError::Failed(
                "OCR observation monitor is unavailable",
            ))?;
        if monitor.transform != 0
            || region
                .x
                .checked_add(region.width)
                .is_none_or(|right| right > observation.image_width)
            || region
                .y
                .checked_add(region.height)
                .is_none_or(|bottom| bottom > observation.image_height)
        {
            return Err(accessibility::EvidenceError::Failed(
                "OCR region bounds or monitor transform are unsupported",
            ));
        }
        let (logical_width, logical_height) = monitor.logical_size();
        if logical_width == 0
            || logical_height == 0
            || (observation.image_width as f64 / logical_width as f64 - monitor.scale).abs() > 0.02
            || (observation.image_height as f64 / logical_height as f64 - monitor.scale).abs()
                > 0.02
        {
            return Err(accessibility::EvidenceError::Failed(
                "OCR screenshot scale is not verified",
            ));
        }
        let Some(before_window) =
            run_with_goal_budget(context, deadline, backend::active_window()).await
        else {
            return Err(accessibility::EvidenceError::Timeout);
        };
        let before_window = before_window.map_err(|_| {
            accessibility::EvidenceError::Failed("focused window is unavailable for OCR")
        })?;
        let (window_address, window_title, window_geometry, window_process_id) =
            validate_ocr_window(target, &monitor, &observation, region, &before_window)?;

        let png = if fresh_capture {
            let Some(png) =
                run_with_goal_budget(context, deadline, backend::capture(&monitor.name)).await
            else {
                return Err(accessibility::EvidenceError::Timeout);
            };
            Arc::new(png.map_err(|_| {
                accessibility::EvidenceError::Failed("OCR screenshot capture failed")
            })?)
        } else {
            observation
                .image_png
                .clone()
                .ok_or(accessibility::EvidenceError::Failed(
                    "approved Observation image is unavailable for OCR",
                ))?
        };
        if backend::png_size(png.as_slice())
            != Some((observation.image_width, observation.image_height))
        {
            return Err(accessibility::EvidenceError::Failed(
                "OCR screenshot dimensions changed from the Observation",
            ));
        }
        if fresh_capture {
            let Some(after_snapshot) = run_with_goal_budget(context, deadline, async {
                backend::monitors().await.map(backend::selectable)
            })
            .await
            else {
                return Err(accessibility::EvidenceError::Timeout);
            };
            let after_snapshot = after_snapshot.map_err(|_| {
                accessibility::EvidenceError::Failed(
                    "display snapshot unavailable after OCR capture",
                )
            })?;
            if self.state.generation.load(Ordering::SeqCst) != observation.generation
                || backend::fingerprint(&after_snapshot) != fingerprint
            {
                return Err(accessibility::EvidenceError::Failed(
                    "display configuration changed during OCR capture",
                ));
            }
            let Some(after_window) =
                run_with_goal_budget(context, deadline, backend::active_window()).await
            else {
                return Err(accessibility::EvidenceError::Timeout);
            };
            let after_window = after_window.map_err(|_| {
                accessibility::EvidenceError::Failed("focused window changed during OCR capture")
            })?;
            if after_window["address"].as_str() != Some(window_address.as_str())
                || after_window["pid"].as_i64() != Some(window_process_id)
                || after_window["title"].as_str() != Some(window_title.as_str())
                || after_window["monitor"].as_i64() != Some(monitor.id)
                || active_window_geometry(&after_window) != Some(window_geometry)
            {
                return Err(accessibility::EvidenceError::Failed(
                    "OCR target window identity or geometry changed during capture",
                ));
            }
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(accessibility::EvidenceError::Timeout)?;
        let (texts, truncated, region_hash) = accessibility::collect_local_ocr(
            png,
            if fresh_capture {
                observation.image_png.clone()
            } else {
                None
            },
            region,
            languages,
            remaining,
        )
        .await?;
        let scale_x = logical_width as f64 / observation.image_width as f64;
        let scale_y = logical_height as f64 / observation.image_height as f64;
        let elements = texts
            .into_iter()
            .filter(|text| {
                !text.text.trim().is_empty()
                    && text.confidence.is_finite()
                    && text.confidence >= 70.0
            })
            .map(|text| accessibility::AccessibleElement {
                id: text.id,
                role: "ocr navigation label".into(),
                name: text.text,
                x: monitor.x as f64 + (region.x as f64 + text.x) * scale_x,
                y: monitor.y as f64 + (region.y as f64 + text.y) * scale_y,
                width: text.width * scale_x,
                height: text.height * scale_y,
                visible: true,
                enabled: true,
                showing: true,
                focused: false,
                selected: false,
                checked: false,
                expanded: false,
                editable: false,
                protected: false,
                ocr_confidence: Some(text.confidence),
            })
            .collect();
        Ok(accessibility::Evidence {
            source: accessibility::EvidenceSource {
                application: target.application.clone(),
                window: window_title,
                window_id: window_address,
                process_id: Some(window_process_id),
                revision: region_hash,
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
            window_geometry: Some(window_geometry),
            browser_viewport: None,
            captured_at_unix_ms: unix_time_ms(),
            elements,
            truncated,
        })
    }

    async fn extract_fast_path(
        &self,
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
        p: &fast_path::FastPathParams,
        run: &mut FastPathRun,
        deadline: Instant,
    ) -> Result<FastPathExtraction, FastPathHandback> {
        run.enter(FastPathStage::Extract);
        let _extraction_timing = ElapsedTiming::new(&mut run.timings.extraction_ms);
        let remaining = || deadline.checked_duration_since(Instant::now());
        let snapshot_result = run_with_goal_budget(context, deadline, async {
            backend::monitors().await.map(backend::selectable)
        })
        .await;
        let snapshot = match snapshot_result {
            Some(Ok(snapshot)) => snapshot,
            Some(Err(_)) => return Err(FastPathHandback::new("display_snapshot_unavailable")),
            None => {
                return Err(FastPathHandback::new(if context.ct.is_cancelled() {
                    "cancelled"
                } else {
                    "goal_timeout_before_display_snapshot"
                }));
            }
        };
        if !self.state.actionable() {
            return Err(FastPathHandback::new("display_event_channel_unhealthy"));
        }
        let fingerprint = backend::fingerprint(&snapshot);
        let observation = self
            .state
            .validate_observation(&p.observation_id, fingerprint)
            .map_err(|_| FastPathHandback::new("stale_observation"))?;
        let monitor = snapshot
            .iter()
            .find(|monitor| monitor.name == observation.monitor)
            .cloned()
            .ok_or_else(|| FastPathHandback::new("observation_monitor_unavailable"))?;
        if !typesafe::credentials_available() {
            return Err(FastPathHandback::new("missing_typesafe_credentials"));
        }
        let Some(evidence_timeout) = remaining() else {
            return Err(FastPathHandback::new("goal_timeout_before_extraction"));
        };
        let evidence_timeout = match p.source {
            fast_path::EvidenceSourceKind::LocalOcr => evidence_timeout,
            _ => evidence_timeout.min(Duration::from_secs(3)),
        };
        let evidence_result = match p.source {
            fast_path::EvidenceSourceKind::LocalOcr => {
                let Some(region) = p.ocr_region.as_ref() else {
                    return Err(FastPathHandback::new("invalid_ocr_scope"));
                };
                let languages = p.ocr_languages.as_slice();
                match run_with_goal_budget(
                    context,
                    deadline,
                    self.collect_ocr_evidence_for_observation(
                        context,
                        &p.observation_id,
                        &p.target,
                        region,
                        languages,
                        false,
                        evidence_timeout,
                    ),
                )
                .await
                {
                    Some(result) => result,
                    None => Err(accessibility::EvidenceError::Timeout),
                }
            }
            source => collect_fast_evidence(context, source, &p.target, evidence_timeout).await,
        };
        let evidence = match evidence_result {
            Ok(evidence) => evidence,
            Err(error) => {
                let reason = if context.ct.is_cancelled() {
                    "cancelled"
                } else {
                    evidence_reason(error)
                };
                return Err(FastPathHandback::new(reason));
            }
        };
        let evidence = match normalize_fast_evidence(
            context, p.source, &p.target, &monitor, deadline, evidence,
        )
        .await
        {
            Ok(evidence) => evidence,
            Err(reason) => return Err(FastPathHandback::new(reason)),
        };
        if p.source == fast_path::EvidenceSourceKind::LocalOcr {
            let Some(completion_timeout) = remaining() else {
                return Err(FastPathHandback::new(
                    "goal_timeout_before_completion_precheck",
                ));
            };
            let completion_evidence = match run_with_goal_budget(
                context,
                deadline,
                collect_fast_completion_evidence(
                    context,
                    p.source,
                    &p.target,
                    &p.completion,
                    completion_timeout.min(Duration::from_secs(3)),
                ),
            )
            .await
            {
                Some(Ok(evidence)) => evidence,
                Some(Err(_)) => {
                    return Err(FastPathHandback::new(
                        "native_completion_evidence_unavailable",
                    ));
                }
                None => {
                    return Err(FastPathHandback::new(if context.ct.is_cancelled() {
                        "cancelled"
                    } else {
                        "goal_timeout_before_completion_precheck"
                    }));
                }
            };
            if fast_path::native_completion_matches_for_window(
                &completion_evidence,
                &p.target,
                &p.completion,
            ) && ocr_completion_window_is_current(
                context,
                &p.target,
                &evidence.source.window_id,
                evidence.source.process_id,
                &completion_evidence.source.window_id,
                deadline,
            )
            .await
            {
                return Ok(FastPathExtraction::AlreadyComplete);
            }
        }
        let plans = fast_path::build_candidates(
            p,
            &evidence,
            &monitor,
            observation.image_width,
            observation.image_height,
        )
        .map_err(|error| FastPathHandback::new(validation_reason(error)))?;
        if p.source != fast_path::EvidenceSourceKind::LocalOcr
            && fast_path::completion_matches(&evidence, &p.completion)
        {
            return Ok(FastPathExtraction::AlreadyComplete);
        }
        if plans.is_empty() {
            return Err(FastPathHandback::new("no_permitted_candidates"));
        }
        Ok(FastPathExtraction::Ready(FastPathPrepared {
            evidence,
            plans,
        }))
    }

    async fn infer_fast_path(
        &self,
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
        p: &fast_path::FastPathParams,
        prepared: &FastPathPrepared,
        run: &mut FastPathRun,
        deadline: Instant,
    ) -> Result<FastPathSelection, FastPathHandback> {
        run.enter(FastPathStage::Infer);
        let Some(inference_timeout) = deadline.checked_duration_since(Instant::now()) else {
            return Err(FastPathHandback::new("goal_timeout_before_inference"));
        };
        let views = fast_path::candidate_views(&prepared.plans);
        let evidence_revision = prepared.evidence.revision();
        let inference_started = Instant::now();
        let selection_future = typesafe::choose(
            &p.goal,
            &p.executed_actions,
            &p.target,
            &p.observation_id,
            &evidence_revision,
            &views,
            inference_timeout,
        );
        tokio::pin!(selection_future);
        let selection = tokio::select! {
            selection = &mut selection_future => selection,
            _ = context.ct.cancelled() => {
                run.timings.inference_ms = inference_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::new("cancelled"));
            }
        };
        run.timings.inference_ms = inference_started.elapsed().as_millis() as u64;
        let selection = selection.map_err(|error| FastPathHandback::new(error.reason()))?;
        let selection_json = serde_json::json!({
            "candidate_id": selection.choice,
            "confidence": selection.confidence,
            "probability": selection.probability,
            "model": selection.model,
            "evidence_revision": evidence_revision,
            "usage": {
                "input_tokens": selection.input_tokens,
                "output_tokens": selection.output_tokens,
            },
        });
        if selection.choice == fast_path::RESERVED_REOBSERVE {
            return Err(FastPathHandback::with_selection(
                "model_requested_reobserve",
                &selection_json,
            ));
        }
        if selection.choice == fast_path::RESERVED_ABSTAIN {
            return Err(FastPathHandback::with_selection(
                "model_abstained",
                &selection_json,
            ));
        }
        let candidate = fast_path::find_candidate(&prepared.plans, &selection.choice)
            .cloned()
            .ok_or_else(|| {
                FastPathHandback::with_selection("candidate_membership_failed", &selection_json)
            })?;
        Ok(FastPathSelection {
            selection,
            selection_json,
            candidate,
        })
    }

    async fn revalidate_fast_path<'a>(
        &'a self,
        context: &rmcp::service::RequestContext<rmcp::service::RoleServer>,
        p: &fast_path::FastPathParams,
        selected: &FastPathSelection,
        run: &mut FastPathRun,
        deadline: Instant,
    ) -> Result<Option<FastPathAdmission<'a>>, FastPathHandback> {
        run.enter(FastPathStage::Revalidate);
        let revalidation_started = Instant::now();
        let serial = match self
            .acquire_action_lock_with_deadline(context, Some(deadline))
            .await
        {
            Ok(guard) => guard,
            Err(reason) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    if reason.contains("timeout") {
                        "goal_timeout_while_waiting_for_input"
                    } else {
                        "cancelled"
                    },
                    &selected.selection_json,
                ));
            }
        };
        let current_snapshot_result = run_with_goal_budget(context, deadline, async {
            backend::monitors().await.map(backend::selectable)
        })
        .await;
        let current_snapshot = match current_snapshot_result {
            Some(Ok(snapshot)) => snapshot,
            Some(Err(_)) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    "display_snapshot_unavailable_at_dispatch",
                    &selected.selection_json,
                ));
            }
            None => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    if context.ct.is_cancelled() {
                        "cancelled"
                    } else {
                        "goal_timeout_before_revalidation"
                    },
                    &selected.selection_json,
                ));
            }
        };
        let current_fingerprint = backend::fingerprint(&current_snapshot);
        let current_observation = match self
            .state
            .validate_observation(&p.observation_id, current_fingerprint)
        {
            Ok(observation) => observation,
            Err(_) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    "stale_observation_at_dispatch",
                    &selected.selection_json,
                ));
            }
        };
        let Some(current_monitor) = current_snapshot
            .iter()
            .find(|monitor| monitor.name == current_observation.monitor)
            .cloned()
        else {
            run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
            return Err(FastPathHandback::with_selection(
                "observation_monitor_unavailable_at_dispatch",
                &selected.selection_json,
            ));
        };
        if !self.state.actionable() {
            run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
            return Err(FastPathHandback::with_selection(
                "display_event_channel_unhealthy_at_dispatch",
                &selected.selection_json,
            ));
        }
        let Some(revalidation_timeout) = deadline.checked_duration_since(Instant::now()) else {
            run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
            return Err(FastPathHandback::with_selection(
                "goal_timeout_before_revalidation",
                &selected.selection_json,
            ));
        };
        let revalidation_timeout = match p.source {
            fast_path::EvidenceSourceKind::LocalOcr => revalidation_timeout,
            _ => revalidation_timeout.min(Duration::from_secs(3)),
        };
        let current_evidence_result = match p.source {
            fast_path::EvidenceSourceKind::LocalOcr => {
                let Some(region) = p.ocr_region.as_ref() else {
                    run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                    return Err(FastPathHandback::with_selection(
                        "invalid_ocr_scope",
                        &selected.selection_json,
                    ));
                };
                match run_with_goal_budget(
                    context,
                    deadline,
                    self.collect_ocr_evidence_for_observation(
                        context,
                        &p.observation_id,
                        &p.target,
                        region,
                        &p.ocr_languages,
                        true,
                        revalidation_timeout,
                    ),
                )
                .await
                {
                    Some(result) => result,
                    None => Err(accessibility::EvidenceError::Timeout),
                }
            }
            source => collect_fast_evidence(context, source, &p.target, revalidation_timeout).await,
        };
        let current_evidence = match current_evidence_result {
            Ok(evidence) => evidence,
            Err(error) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                let reason = if context.ct.is_cancelled() {
                    "cancelled"
                } else {
                    evidence_reason(error)
                };
                return Err(FastPathHandback::with_selection(
                    reason,
                    &selected.selection_json,
                ));
            }
        };
        let current_evidence = match normalize_fast_evidence(
            context,
            p.source,
            &p.target,
            &current_monitor,
            deadline,
            current_evidence,
        )
        .await
        {
            Ok(evidence) => evidence,
            Err(reason) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    reason,
                    &selected.selection_json,
                ));
            }
        };
        if p.source == fast_path::EvidenceSourceKind::LocalOcr {
            let Some(completion_timeout) = deadline.checked_duration_since(Instant::now()) else {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    "goal_timeout_before_completion_precheck",
                    &selected.selection_json,
                ));
            };
            let completion_result = run_with_goal_budget(
                context,
                deadline,
                collect_fast_completion_evidence(
                    context,
                    p.source,
                    &p.target,
                    &p.completion,
                    completion_timeout.min(Duration::from_secs(3)),
                ),
            )
            .await;
            let completion_evidence = match completion_result {
                Some(Ok(evidence)) => evidence,
                Some(Err(_)) => {
                    run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                    return Err(FastPathHandback::with_selection(
                        "native_completion_evidence_unavailable",
                        &selected.selection_json,
                    ));
                }
                None => {
                    run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                    return Err(FastPathHandback::with_selection(
                        if context.ct.is_cancelled() {
                            "cancelled"
                        } else {
                            "goal_timeout_before_completion_precheck"
                        },
                        &selected.selection_json,
                    ));
                }
            };
            if fast_path::native_completion_matches_for_window(
                &completion_evidence,
                &p.target,
                &p.completion,
            ) && ocr_completion_window_is_current(
                context,
                &p.target,
                &current_evidence.source.window_id,
                current_evidence.source.process_id,
                &completion_evidence.source.window_id,
                deadline,
            )
            .await
            {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Ok(None);
            }
        }
        let current_plans = match fast_path::build_candidates(
            p,
            &current_evidence,
            &current_monitor,
            current_observation.image_width,
            current_observation.image_height,
        ) {
            Ok(plans) => plans,
            Err(error) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    validation_reason(error),
                    &selected.selection_json,
                ));
            }
        };
        let dispatch_window_id = current_evidence.source.window_id.clone();
        let Some(current_selected) =
            fast_path::find_candidate(&current_plans, &selected.selection.choice).cloned()
        else {
            run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
            return Err(FastPathHandback::with_selection(
                "candidate_stale_or_missing",
                &selected.selection_json,
            ));
        };
        if !fast_path::same_candidate(&selected.candidate, &current_selected) {
            run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
            return Err(FastPathHandback::with_selection(
                "candidate_changed_at_dispatch",
                &selected.selection_json,
            ));
        }
        let active_window = match read_active_window_for_dispatch(context, deadline).await {
            Ok(window) => window,
            Err(reason) => {
                run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
                return Err(FastPathHandback::with_selection(
                    reason,
                    &selected.selection_json,
                ));
            }
        };
        let Some(expected_active_window) = dispatch_window_binding(
            p.source,
            &p.target,
            &current_evidence,
            current_monitor.id,
            &active_window,
        ) else {
            run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
            return Err(FastPathHandback::with_selection(
                "target_window_changed_at_dispatch",
                &selected.selection_json,
            ));
        };
        let action = match &current_selected.action {
            fast_path::ActionSpec::Click { x, y } => ActionKind::Click {
                x: *x,
                y: *y,
                button: ClickButton::Left,
                double: false,
            },
            fast_path::ActionSpec::TypeText { text } => ActionKind::TypeText {
                text: text.clone(),
                paste: None,
            },
        };
        let browser_candidate =
            (p.source == fast_path::EvidenceSourceKind::BrowserExtension).then(|| {
                BrowserCandidateBinding {
                    target: p.target.clone(),
                    evidence_revision: current_selected.source_revision.clone(),
                    browser_window_id: current_evidence.source.browser_window_id.clone(),
                    browser_tab_id: current_evidence.source.browser_tab_id.clone(),
                    document_id: current_evidence.source.document_id.clone(),
                    window_id: current_evidence.source.window_id.clone(),
                }
            });
        let dispatch_pid = expected_active_window.pid;
        run.timings.revalidation_ms = revalidation_started.elapsed().as_millis() as u64;
        let Some(action_budget) = deadline
            .checked_duration_since(Instant::now())
            .filter(|budget| *budget >= Duration::from_millis(500))
        else {
            return Err(FastPathHandback::with_selection(
                "goal_timeout_before_input",
                &selected.selection_json,
            ));
        };
        Ok(Some(FastPathAdmission {
            serial,
            candidate: current_selected,
            action,
            dispatch_window_id,
            dispatch_pid,
            expected_active_window: Some(expected_active_window),
            browser_candidate,
            action_budget,
        }))
    }

    async fn dispatch_fast_path_action(
        &self,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        p: &fast_path::FastPathParams,
        run: &mut FastPathRun,
        admission: FastPathAdmission<'_>,
        selection: &FastPathSelection,
        deadline: Instant,
    ) -> Result<CallToolResult, McpError> {
        run.enter(FastPathStage::Dispatch);
        let FastPathAdmission {
            serial,
            candidate,
            action,
            dispatch_window_id,
            dispatch_pid,
            expected_active_window,
            browser_candidate,
            action_budget,
        } = admission;
        let action_ct = context.ct.child_token();
        let action_abort = action_ct.clone();
        let action_timings = Arc::new(Mutex::new(ActionTimings::default()));
        let parent_ct = context.ct.clone();
        let mut action_context = context.clone();
        action_context.ct = action_ct.clone();
        let deadline_task = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(action_budget) => action_ct.cancel(),
                _ = parent_ct.cancelled() => action_ct.cancel(),
            }
        });
        // Borrow the guard: the Fast Path must keep serialization through
        // fresh source completion verification, not just input delivery.
        let action_future = self.computer_action_locked(
            action_context,
            ActionParams {
                observation_id: p.observation_id.clone(),
                action,
            },
            &serial,
            Some(action_timings.clone()),
            expected_active_window,
            browser_candidate,
        );
        // Await the existing executor after cancelling its child context. The
        // borrowed guard stays in this caller's scope and must remain held
        // through verification; dropping the future at the budget boundary
        // could otherwise let a later action overlap a running worker.
        let action_result = match action_future.await {
            Ok(result) => result,
            Err(_) => {
                action_abort.cancel();
                deadline_task.abort();
                let measured = *action_timings.lock().unwrap();
                run.timings.input_ms = measured.input_ms;
                run.timings.capture_ms = measured.capture_ms;
                return run.handback(
                    "action_dispatch_failed",
                    Some(serde_json::json!({
                        "selection": selection.selection_json,
                        "effect": "unknown",
                        "do_not_replay": true,
                        "actions_executed": 1,
                        "action_history": fast_action_history(
                            &candidate,
                            fast_path::ActionEffect::Unknown,
                            true,
                        ),
                    })),
                );
            }
        };
        deadline_task.abort();
        let measured = *action_timings.lock().unwrap();
        run.timings.input_ms = measured.input_ms;
        run.timings.capture_ms = measured.capture_ms;
        run.enter(FastPathStage::Verify);
        let action_value = action_result.structured_content.clone().unwrap_or_else(|| {
            serde_json::json!({
                "outcome": "partial",
                "effect": "unknown",
                "do_not_replay": true,
            })
        });
        let effect = match action_value
            .get("effect")
            .and_then(serde_json::Value::as_str)
        {
            Some(effect) => fast_path::ActionEffect::parse(Some(effect)),
            None if action_value.get("error").is_some() => fast_path::ActionEffect::None,
            None => fast_path::ActionEffect::Unknown,
        };
        let action_count = effect.action_count();
        let do_not_replay = action_value
            .get("do_not_replay")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(effect.may_have_delivered());
        let mut goal_complete = false;
        let mut post_evidence_revision = None;
        let mut reason = match effect {
            fast_path::ActionEffect::None => "action_rejected_before_input",
            fast_path::ActionEffect::Completed => "completion_not_yet_verified",
            fast_path::ActionEffect::Partial => "input_partially_delivered",
            fast_path::ActionEffect::Unknown => "input_delivery_unknown",
        };
        if effect == fast_path::ActionEffect::Completed
            && action_value
                .get("outcome")
                .and_then(serde_json::Value::as_str)
                == Some("ok")
        {
            let remaining = || deadline.checked_duration_since(Instant::now());
            if let Some(post_timeout) = remaining() {
                let post_timeout = match p.source {
                    fast_path::EvidenceSourceKind::LocalOcr => post_timeout,
                    _ => post_timeout.min(Duration::from_secs(3)),
                };
                let mut post_evidence = match p.source {
                    source => {
                        collect_fast_completion_evidence(
                            &context,
                            source,
                            &p.target,
                            &p.completion,
                            post_timeout,
                        )
                        .await
                    }
                };
                // Media startup is asynchronous. Observe only while waiting; never
                // replay the delivered click or ask Jev to click a loading player.
                if p.authorization == fast_path::AuthorizationScope::MediaPlayback {
                    let wait_until = deadline.min(Instant::now() + Duration::from_secs(3));
                    loop {
                        if let Ok(evidence) = &post_evidence {
                            if !fast_path::evidence_matches_target(
                                &p.target,
                                evidence,
                                Some(&dispatch_window_id),
                            ) || fast_path::completion_matches(evidence, &p.completion)
                            {
                                break;
                            }
                        }
                        if Instant::now() + Duration::from_millis(150) >= wait_until {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        let budget = wait_until.saturating_duration_since(Instant::now());
                        if budget.is_zero() {
                            break;
                        }
                        post_evidence = collect_fast_completion_evidence(
                            &context,
                            p.source,
                            &p.target,
                            &p.completion,
                            budget,
                        )
                        .await;
                    }
                }
                match post_evidence {
                    Ok(post_evidence) => {
                        post_evidence_revision = Some(post_evidence.revision());
                        let completion_matches = match p.source {
                            fast_path::EvidenceSourceKind::LocalOcr => {
                                ocr_completion_window_is_current(
                                    &context,
                                    &p.target,
                                    &dispatch_window_id,
                                    Some(dispatch_pid),
                                    &post_evidence.source.window_id,
                                    deadline,
                                )
                                .await
                                    && fast_path::native_completion_matches_for_window(
                                        &post_evidence,
                                        &p.target,
                                        &p.completion,
                                    )
                            }
                            _ => fast_path::completion_matches_for_target(
                                &post_evidence,
                                &p.target,
                                Some(dispatch_window_id.as_str()),
                                &p.completion,
                            ),
                        };
                        if completion_matches {
                            goal_complete = true;
                            reason = if matches!(
                                p.source,
                                fast_path::EvidenceSourceKind::NativeAccessibility
                                    | fast_path::EvidenceSourceKind::LocalOcr
                            ) {
                                "completion_verified_from_fresh_native_evidence"
                            } else {
                                "completion_verified_from_fresh_source_evidence"
                            };
                        } else {
                            reason = "completion_not_observed_after_action";
                        }
                    }
                    Err(_) => reason = "completion_observation_unavailable_after_action",
                }
            } else {
                reason = "goal_timeout_before_completion_check";
            }
        }
        let final_do_not_replay = if goal_complete {
            false
        } else {
            do_not_replay || effect.may_have_delivered()
        };
        let mut value = serde_json::json!({
            "outcome": if goal_complete { "completed" } else { "handback" },
            "status": if goal_complete { "completed" } else { "handback" },
            "reason": reason,
            "effect": effect.as_str(),
            "do_not_replay": final_do_not_replay,
            "actions_executed": action_count,
            "action_history": fast_action_history(&candidate, effect, final_do_not_replay),
            "progress": {
                "goal_complete": goal_complete,
                "completion_observed": goal_complete,
                "selected_candidate_id": selection.selection.choice,
            },
            "selection": selection.selection_json,
            "post_evidence_revision": post_evidence_revision,
            "timings_ms": run.timings.value(),
        });
        if let Some(observation) = action_value.get("observation") {
            value["observation"] = observation.clone();
            value["final_observation"] = observation.clone();
        }
        if let Some(display_changed) = action_value.get("display_changed_during_action") {
            value["display_changed_during_action"] = display_changed.clone();
        }
        wrap_fast_action_result(value, action_result, !goal_complete)
    }

    /// Delegate one bounded Goal to optional local evidence + Jev.
    /// Jev chooses only server-created candidate IDs; the server reobserves,
    /// validates, and executes one existing Action at a time, up to the
    /// caller's action limit and the shared 30-second ceiling.
    /// Browser actions additionally require an exact trusted origin configured
    /// by the server operator; tab permission alone never authorizes input.
    #[tool(name = "computer_goal")]
    async fn computer_goal(
        &self,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        Parameters(p): Parameters<fast_path::FastPathParams>,
    ) -> Result<CallToolResult, McpError> {
        const MAX_REOBSERVES: u8 = 10;
        let mut validation_run = FastPathRun::new(&p.observation_id);
        if let Err(error) = p.validate() {
            return validation_run.handback(validation_reason(error), None);
        }
        if fast_path::goal_mentions_blocked_operation(&p.goal) {
            return validation_run.handback("goal_consequence_not_allowed", None);
        }
        if fast_path::goal_may_contain_sensitive_value(&p.goal)
            || p.approved_literals
                .iter()
                .any(|literal| fast_path::goal_contains_literal(&p.goal, &literal.text))
        {
            return validation_run.handback("goal_contains_sensitive_input", None);
        }

        let max_actions = p.limits.max_actions;
        let deadline = validation_run.deadline(p.limits.timeout_ms);
        let mut progress = FastPathGoalProgress::new();
        let mut observation_id = p.observation_id.clone();
        let mut previous_source_revision: Option<String> = None;
        let mut refreshes = 0u8;
        let mut iterations = 0u8;
        let mut last_value = serde_json::json!({});
        let mut last_observation = None;
        let mut images = Vec::new();

        loop {
            if iterations >= max_actions.saturating_add(MAX_REOBSERVES) {
                return Ok(progress.response(
                    last_value,
                    "goal_iteration_limit_reached",
                    false,
                    max_actions,
                    refreshes,
                    &observation_id,
                    last_observation.as_ref(),
                    images,
                ));
            }
            iterations = iterations.saturating_add(1);
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(progress.response(
                    last_value,
                    "goal_timeout",
                    false,
                    max_actions,
                    refreshes,
                    &observation_id,
                    last_observation.as_ref(),
                    images,
                ));
            };
            let timeout_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
            if timeout_ms < fast_path::MIN_TIMEOUT_MS {
                return Ok(progress.response(
                    last_value,
                    "goal_timeout",
                    false,
                    max_actions,
                    refreshes,
                    &observation_id,
                    last_observation.as_ref(),
                    images,
                ));
            }
            if progress.actions_executed >= max_actions {
                return Ok(progress.response(
                    last_value,
                    "max_actions_reached",
                    false,
                    max_actions,
                    refreshes,
                    &observation_id,
                    last_observation.as_ref(),
                    images,
                ));
            }

            let mut step_params = p.clone();
            step_params.executed_actions = progress
                .action_history
                .iter()
                .filter(|action| action["effect"].as_str() == Some("completed"))
                .filter_map(|action| action["description"].as_str().map(str::to_owned))
                .collect();
            step_params.observation_id = observation_id.clone();
            step_params.limits.timeout_ms = timeout_ms;

            // The shared deadline cancels the step token so in-flight input
            // uses the existing effect/no-replay cleanup path.
            let mut step_context = context.clone();
            let step_token = context.ct.child_token();
            step_context.ct = step_token.clone();
            let deadline_token = step_token.clone();
            let parent_token = context.ct.clone();
            let timer = tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::time::sleep(remaining) => deadline_token.cancel(),
                    _ = parent_token.cancelled() => deadline_token.cancel(),
                }
            });
            let step_result = self
                .computer_goal_step(
                    step_context,
                    step_params,
                    previous_source_revision.as_deref(),
                )
                .await;
            timer.abort();
            let step_result = match step_result {
                Ok(result) => result,
                Err(_error) if progress.actions_executed > 0 => {
                    return Ok(progress.response(
                        last_value,
                        "goal_step_failed_after_progress",
                        false,
                        max_actions,
                        refreshes,
                        &observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                }
                Err(error) => return Err(error),
            };

            let step_value = step_result.structured_content.clone().unwrap_or_default();
            if let Some(step_observation) = step_value.get("observation") {
                last_observation = Some(step_observation.clone());
                if let Some(id) = step_observation["observation_id"].as_str() {
                    observation_id = id.to_owned();
                }
            }
            let step_images: Vec<Content> = step_result
                .content
                .iter()
                .filter_map(|block| block.as_image().map(|_| block.clone()))
                .collect();
            if !step_images.is_empty() {
                images = step_images;
            }
            progress.absorb(&step_value);
            last_value = step_value.clone();
            let reason = step_value["reason"]
                .as_str()
                .unwrap_or("goal_step_failed")
                .to_owned();

            if step_value["outcome"].as_str() == Some("completed") {
                return Ok(progress.response(
                    step_value,
                    &reason,
                    true,
                    max_actions,
                    refreshes,
                    &observation_id,
                    last_observation.as_ref(),
                    images,
                ));
            }

            if reason == "model_requested_reobserve" {
                if refreshes >= MAX_REOBSERVES {
                    return Ok(progress.response(
                        step_value,
                        "reobserve_limit_reached",
                        false,
                        max_actions,
                        refreshes,
                        &observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                }
                let monitor = self
                    .state
                    .observations
                    .lock()
                    .unwrap()
                    .get(&observation_id)
                    .map(|observation| observation.monitor.clone());
                let Some(monitor) = monitor else {
                    return Ok(progress.response(
                        step_value,
                        "reobserve_observation_expired",
                        false,
                        max_actions,
                        refreshes,
                        &observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                };
                let Some(_) = deadline.checked_duration_since(Instant::now()) else {
                    return Ok(progress.response(
                        step_value,
                        "goal_timeout_before_reobserve",
                        false,
                        max_actions,
                        refreshes,
                        &observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                };
                let refresh_started = Instant::now();
                let refreshed = run_with_goal_budget(
                    &context,
                    deadline,
                    self.computer_observe(Parameters(ObserveParams { monitor })),
                )
                .await;
                let refresh_result = match refreshed {
                    Some(Ok(result)) => result,
                    Some(Err(_)) | None => {
                        return Ok(progress.response(
                            step_value,
                            "reobserve_failed",
                            false,
                            max_actions,
                            refreshes,
                            &observation_id,
                            last_observation.as_ref(),
                            images,
                        ));
                    }
                };
                let refresh_value = refresh_result
                    .structured_content
                    .clone()
                    .unwrap_or_default();
                let Some(next_observation_id) =
                    refresh_value["observation_id"].as_str().map(str::to_owned)
                else {
                    return Ok(progress.response(
                        step_value,
                        "reobserve_failed",
                        false,
                        max_actions,
                        refreshes,
                        &observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                };
                observation_id = next_observation_id;
                last_observation = Some(refresh_value);
                let refreshed_images: Vec<Content> = refresh_result
                    .content
                    .iter()
                    .filter_map(|block| block.as_image().map(|_| block.clone()))
                    .collect();
                if !refreshed_images.is_empty() {
                    images = refreshed_images;
                }
                refreshes = refreshes.saturating_add(1);
                progress.add_refresh(refresh_started.elapsed());
                continue;
            }

            if reason == "completion_not_observed_after_action"
                && step_value["effect"].as_str() == Some("completed")
            {
                let Some(next_observation_id) = step_value["observation"]["observation_id"]
                    .as_str()
                    .map(str::to_owned)
                else {
                    return Ok(progress.response(
                        step_value,
                        "post_action_observation_missing",
                        false,
                        max_actions,
                        refreshes,
                        &observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                };
                if progress.actions_executed >= max_actions {
                    return Ok(progress.response(
                        step_value,
                        "max_actions_reached",
                        false,
                        max_actions,
                        refreshes,
                        &next_observation_id,
                        last_observation.as_ref(),
                        images,
                    ));
                }
                previous_source_revision = step_value["selection"]["evidence_revision"]
                    .as_str()
                    .map(str::to_owned);
                observation_id = next_observation_id;
                continue;
            }

            return Ok(progress.response(
                step_value,
                &reason,
                false,
                max_actions,
                refreshes,
                &observation_id,
                last_observation.as_ref(),
                images,
            ));
        }
    }

    async fn computer_goal_step(
        &self,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        p: fast_path::FastPathParams,
        previous_source_revision: Option<&str>,
    ) -> Result<CallToolResult, McpError> {
        let mut run = FastPathRun::new(&p.observation_id);

        if let Err(error) = p.validate() {
            return run.handback(validation_reason(error), None);
        }
        if fast_path::goal_mentions_blocked_operation(&p.goal) {
            return run.handback("goal_consequence_not_allowed", None);
        }
        // A literal is caller-approved local input, not semantic state for a
        // remote model. Reject a goal that repeats one rather than risking
        // disclosure through the Goal field.
        if fast_path::goal_may_contain_sensitive_value(&p.goal)
            || p.approved_literals
                .iter()
                .any(|literal| fast_path::goal_contains_literal(&p.goal, &literal.text))
        {
            return run.handback("goal_contains_sensitive_input", None);
        }
        let deadline = run.deadline(p.limits.timeout_ms);
        let extraction = match self
            .extract_fast_path(&context, &p, &mut run, deadline)
            .await
        {
            Ok(extraction) => extraction,
            Err(handback) => return handback.respond(&mut run),
        };
        if matches!(extraction, FastPathExtraction::AlreadyComplete) {
            run.enter(FastPathStage::Verify);
            let value = serde_json::json!({
                "outcome": "completed",
                "status": "completed",
                "reason": "completion_already_observed",
                "effect": "none",
                "do_not_replay": false,
                "actions_executed": 0,
                "action_history": [],
                "progress": {
                    "goal_complete": true,
                    "completion_observed": true,
                },
                "observation_id": p.observation_id,
                "timings_ms": run.timings.value(),
            });
            let mut result = CallToolResult::success(vec![Content::text(value.to_string())]);
            result.structured_content = Some(value);
            return Ok(result);
        }
        let FastPathExtraction::Ready(prepared) = extraction else {
            unreachable!("completed Fast Path extraction handled above")
        };
        if previous_source_revision.is_some_and(|previous| prepared.evidence.revision() == previous)
        {
            return run.handback("no_observable_progress", None);
        }
        let selection = match self
            .infer_fast_path(&context, &p, &prepared, &mut run, deadline)
            .await
        {
            Ok(selection) => selection,
            Err(handback) => return handback.respond(&mut run),
        };
        let admission = match self
            .revalidate_fast_path(&context, &p, &selection, &mut run, deadline)
            .await
        {
            Ok(Some(admission)) => admission,
            Ok(None) => {
                run.enter(FastPathStage::Verify);
                let value = serde_json::json!({
                    "outcome": "completed",
                    "status": "completed",
                    "reason": "completion_already_observed_before_input",
                    "effect": "none",
                    "do_not_replay": false,
                    "actions_executed": 0,
                    "action_history": [],
                    "progress": {
                        "goal_complete": true,
                        "completion_observed": true,
                    },
                    "observation_id": p.observation_id,
                    "selection": selection.selection_json,
                    "timings_ms": run.timings.value(),
                });
                let mut result = CallToolResult::success(vec![Content::text(value.to_string())]);
                result.structured_content = Some(value);
                return Ok(result);
            }
            Err(handback) => return handback.respond(&mut run),
        };
        self.dispatch_fast_path_action(context, &p, &mut run, admission, &selection, deadline)
            .await
    }

    /// Execute one click, scroll, key, type_text, or drag Action against the
    /// referenced observation, then return a fresh observation of that
    /// Monitor (the drag destination for drags). type_text replaces the
    /// clipboard; key/type_text act on the focused window, which must live on
    /// the observed monitor.
    /// Input coordinates are image pixels; the server performs all scaling.
    /// Serialized with every other action. On partial/unknown delivery or a
    /// failed post-action capture, do NOT repeat the action — call
    /// computer_observe and assess first.
    #[tool(name = "computer_action")]
    async fn computer_action(
        &self,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        Parameters(p): Parameters<ActionParams>,
    ) -> Result<CallToolResult, McpError> {
        let serial = match self.acquire_action_lock(&context).await {
            Ok(guard) => guard,
            Err(reason) => return action_err("CANCELLED", reason),
        };
        self.computer_action_locked(context, p, &serial, None, None, None)
            .await
    }

    async fn computer_action_locked(
        &self,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
        p: ActionParams,
        _serial: &tokio::sync::MutexGuard<'_, ()>,
        action_timings: Option<Arc<Mutex<ActionTimings>>>,
        expected_active_window: Option<DispatchWindowBinding>,
        browser_candidate: Option<BrowserCandidateBinding>,
    ) -> Result<CallToolResult, McpError> {
        if context.ct.is_cancelled() {
            return action_err(
                "CANCELLED",
                "request cancelled before input admission; no input was attempted",
            );
        }
        let snapshot = match tokio::select! {
            result = backend::monitors() => result.map(backend::selectable),
            _ = context.ct.cancelled() => {
                return action_err(
                    "CANCELLED",
                    "request cancelled before input admission; no input was attempted",
                );
            }
        } {
            Ok(m) => m,
            Err(e) => return action_backend_error(e),
        };
        let fp = backend::fingerprint(&snapshot);
        if !self.state.actionable() {
            return action_err(
                "EVENT_CHANNEL_UNHEALTHY",
                "display-change notification channel is down; mutation paused until it recovers",
            );
        }
        let obs = match self.state.validate_observation(&p.observation_id, fp) {
            Ok(o) => o,
            Err(msg) => {
                return action_err(
                    "STALE_OBSERVATION",
                    format!("{msg}; call computer_observe for a fresh observation_id"),
                );
            }
        };
        let Some(monitor) = snapshot.iter().find(|m| m.name == obs.monitor) else {
            return action_err("MONITOR_NOT_FOUND", "observation's monitor is gone");
        };
        // One abort signal per action: stops on a mid-action display change
        // (generation), on a worker reply timeout, or on MCP cancellation.
        let abort = backend::Abort::new(self.state.generation.clone(), obs.generation);
        // A display change racing in since validation must reject as stale,
        // not surface as an input-backend failure from an instant abort.
        if self.state.generation.load(Ordering::SeqCst) != obs.generation {
            return action_err(
                "STALE_OBSERVATION",
                "display configuration changed; call computer_observe for a fresh observation_id",
            );
        }
        // notifications/cancelled (or client disconnect) cancels the backend
        // operation: flag + socket shutdown interrupt wedged roundtrips too.
        let _cancel_watch = AbortOnDrop(tokio::spawn({
            let abort = abort.clone();
            let ct = context.ct.clone();
            async move {
                ct.cancelled().await;
                abort.cancel();
            }
        }));
        // Keyboard/text actions act on the focused window; it must live on
        // the observed monitor, else reject before any input.
        if matches!(
            p.action,
            ActionKind::Key { .. } | ActionKind::TypeText { .. }
        ) {
            match tokio::select! {
                result = backend::focused_monitor() => result,
                _ = context.ct.cancelled() => {
                    return action_err(
                        "CANCELLED",
                        "request cancelled before input admission; no input was attempted",
                    );
                }
            } {
                Ok(Some(name)) if name == obs.monitor => {}
                Ok(_) => {
                    return action_err(
                        "FOCUS_MISMATCH",
                        "focus is on another monitor or nothing is focused; click the target first",
                    );
                }
                Err(e) => return action_backend_error(e),
            }
        }

        // Post-action observation target: the drag destination monitor.
        let mut post_monitor = obs.monitor.clone();
        let input_started = Instant::now();
        let delivery = match &p.action {
            ActionKind::Key { key, mods } => {
                if let Some(bad) = mods.iter().find(|m| modifier(m).is_none()) {
                    return action_err(
                        "INVALID_MODIFIER",
                        format!("unknown modifier {bad:?}; use ctrl/shift/alt/super"),
                    );
                }
                if let Err(reason) = validate_input_window(
                    &context,
                    expected_active_window.as_ref(),
                    browser_candidate.as_ref(),
                    monitor,
                )
                .await
                {
                    return if reason == "cancelled" {
                        action_err(
                            "CANCELLED",
                            "request cancelled before input admission; no input was attempted",
                        )
                    } else {
                        action_err(
                            "FOCUS_MISMATCH",
                            "target window could not be verified immediately before input; no input was attempted",
                        )
                    };
                }
                match key_chord(&self.keyboard, mods, key, abort.clone()).await {
                    Ok(d) => d,
                    Err(msg) => return action_err("INVALID_KEY", msg),
                }
            }
            ActionKind::TypeText { text, paste } => {
                // Bound the clipboard payload before spawning wl-copy.
                if text.len() > MAX_TYPE_TEXT_BYTES {
                    return action_err(
                        "PAYLOAD_TOO_LARGE",
                        format!("text exceeds the {MAX_TYPE_TEXT_BYTES} byte limit"),
                    );
                }
                // Validate the paste mode before touching the clipboard: an
                // invalid value must be a rejection, not a clipboard clobber.
                let mods: Vec<String> = match paste.as_deref().unwrap_or("ctrl_v") {
                    "ctrl_v" => vec!["ctrl".into()],
                    "ctrl_shift_v" => vec!["ctrl".into(), "shift".into()],
                    _ => {
                        return action_err(
                            "INVALID_PASTE",
                            "paste must be \"ctrl_v\" or \"ctrl_shift_v\"",
                        );
                    }
                };
                if let Err(reason) = validate_input_window(
                    &context,
                    expected_active_window.as_ref(),
                    browser_candidate.as_ref(),
                    monitor,
                )
                .await
                {
                    return if reason == "cancelled" {
                        action_err(
                            "CANCELLED",
                            "request cancelled before input admission; no input was attempted",
                        )
                    } else {
                        action_err(
                            "FOCUS_MISMATCH",
                            "target window could not be verified immediately before input; no input was attempted",
                        )
                    };
                }
                type_text(&self.keyboard, text, mods, abort.clone()).await
            }
            ActionKind::Click { .. } | ActionKind::Scroll { .. } | ActionKind::Drag { .. } => {
                let Some(layout) = backend::layout_box(&snapshot) else {
                    return action_err("NO_LAYOUT", "no selectable monitors");
                };
                // Drag resolves its destination observation BEFORE any press:
                // it must be current under the same Display Configuration.
                let dst = match &p.action {
                    ActionKind::Drag {
                        dst_observation_id, ..
                    } => {
                        let dst_obs = match self.state.validate_observation(dst_observation_id, fp)
                        {
                            Ok(o) => o,
                            Err(msg) => {
                                return action_err(
                                    "STALE_OBSERVATION",
                                    format!("destination: {msg}; observe the destination again"),
                                );
                            }
                        };
                        let Some(dst_monitor) = snapshot.iter().find(|m| m.name == dst_obs.monitor)
                        else {
                            return action_err(
                                "MONITOR_NOT_FOUND",
                                "destination observation's monitor is gone",
                            );
                        };
                        Some((dst_obs, dst_monitor))
                    }
                    _ => None,
                };
                let (px, py) = match &p.action {
                    ActionKind::Click { x, y, .. }
                    | ActionKind::Scroll { x, y, .. }
                    | ActionKind::Drag { x, y, .. } => (*x, *y),
                    _ => unreachable!(),
                };
                let Some((ax, ay)) =
                    backend::map_point(px, py, obs.image_width, obs.image_height, monitor, &layout)
                else {
                    return action_err(
                        "INVALID_COORDINATES",
                        format!(
                            "point ({px}, {py}) is outside the {}x{} image",
                            obs.image_width, obs.image_height
                        ),
                    );
                };
                let mut ops = vec![
                    PointerOp::Move {
                        x: ax,
                        y: ay,
                        x_extent: layout.x_extent,
                        y_extent: layout.y_extent,
                    },
                    PointerOp::Frame,
                ];
                match &p.action {
                    ActionKind::Drag { dst_x, dst_y, .. } => {
                        let (dst_obs, dst_monitor) = dst.unwrap();
                        post_monitor = dst_monitor.name.clone();
                        let Some((dst_ax, dst_ay)) = backend::map_point(
                            *dst_x,
                            *dst_y,
                            dst_obs.image_width,
                            dst_obs.image_height,
                            dst_monitor,
                            &layout,
                        ) else {
                            return action_err(
                                "INVALID_COORDINATES",
                                format!(
                                    "destination point ({dst_x}, {dst_y}) is outside the {}x{} image",
                                    dst_obs.image_width, dst_obs.image_height
                                ),
                            );
                        };
                        // One move/press/move/release sequence. A straight
                        // jump registers as a selection, not a drag, in most
                        // toolkits — so hold briefly, then emit a bounded
                        // number of interpolated steps before releasing.
                        ops.push(PointerOp::Button {
                            code: backend::BTN_LEFT,
                            pressed: true,
                        });
                        ops.push(PointerOp::Frame);
                        ops.push(PointerOp::Wait { ms: 250 });
                        const STEPS: u32 = 8;
                        for i in 1..=STEPS {
                            let t = i as f64 / STEPS as f64;
                            ops.push(PointerOp::Move {
                                x: (ax as f64 + (dst_ax as f64 - ax as f64) * t).round() as u32,
                                y: (ay as f64 + (dst_ay as f64 - ay as f64) * t).round() as u32,
                                x_extent: layout.x_extent,
                                y_extent: layout.y_extent,
                            });
                            ops.push(PointerOp::Frame);
                            ops.push(PointerOp::Wait { ms: 20 });
                        }
                        ops.push(PointerOp::Button {
                            code: backend::BTN_LEFT,
                            pressed: false,
                        });
                        ops.push(PointerOp::Frame);
                    }
                    ActionKind::Click { button, double, .. } => {
                        let code = match button {
                            ClickButton::Left => backend::BTN_LEFT,
                            ClickButton::Right => backend::BTN_RIGHT,
                        };
                        let presses = if *double { 2 } else { 1 };
                        for _ in 0..presses {
                            ops.push(PointerOp::Button {
                                code,
                                pressed: true,
                            });
                            // Deliver a real press interval. GTK/XWayland can
                            // miss a press/release flushed in one instant.
                            ops.push(PointerOp::Frame);
                            ops.push(PointerOp::Wait { ms: 20 });
                            ops.push(PointerOp::Button {
                                code,
                                pressed: false,
                            });
                        }
                        ops.push(PointerOp::Frame);
                    }
                    ActionKind::Scroll { dx, dy, .. } => {
                        if *dx == 0 && *dy == 0 {
                            return action_err(
                                "INVALID_SCROLL",
                                "dx and dy are both 0; nothing to scroll",
                            );
                        }
                        if *dy != 0 {
                            ops.push(PointerOp::Scroll {
                                axis: 0,
                                steps: *dy,
                            });
                        }
                        if *dx != 0 {
                            ops.push(PointerOp::Scroll {
                                axis: 1,
                                steps: *dx,
                            });
                        }
                        ops.push(PointerOp::Frame);
                    }
                    _ => unreachable!(),
                }
                if let Err(reason) = validate_input_window(
                    &context,
                    expected_active_window.as_ref(),
                    browser_candidate.as_ref(),
                    monitor,
                )
                .await
                {
                    return if reason == "cancelled" {
                        action_err(
                            "CANCELLED",
                            "request cancelled before input admission; no input was attempted",
                        )
                    } else {
                        action_err(
                            "FOCUS_MISMATCH",
                            "target window could not be verified immediately before input; no input was attempted",
                        )
                    };
                }
                let pointer = self.pointer.clone();
                let abort = abort.clone();
                tokio::task::spawn_blocking(move || pointer.apply(ops, abort))
                    .await
                    .unwrap_or(backend::Delivery::Unknown)
            }
        };
        // Invalidate before releasing the shared action lock. A pending Goal
        // cannot reuse its source after this dispatch attempt, including when
        // the backend reports no confirmed delivery. Pre-dispatch rejections
        // (bad coordinates, unknown keys, and so on) retain the observation.
        self.invalidate_after_input_attempt(&p);
        if let Some(action_timings) = &action_timings {
            action_timings.lock().unwrap().input_ms = input_started.elapsed().as_millis() as u64;
        }
        let effect = match delivery {
            backend::Delivery::Completed => "completed",
            backend::Delivery::Partial => "partial",
            backend::Delivery::Unknown => "unknown",
            backend::Delivery::None => "none",
        };
        let action_json = serde_json::to_value(&p.action).unwrap_or_default();
        let cancelled_after_delivery = || {
            let mut value = serde_json::json!({
                "outcome": "partial",
                "effect": effect,
                "action": action_json,
                "do_not_replay": true,
                "display_changed_during_action": false,
            });
            value["error"] = serde_json::json!({
                "code": "CANCELLED_AFTER_INPUT",
                "message": "request cancelled after input began; call computer_observe and do not repeat the action",
            });
            partial_result(value, None)
        };
        if context.ct.is_cancelled() {
            if delivery == backend::Delivery::None {
                return action_err(
                    "CANCELLED",
                    "request cancelled before any input was sent; no side effect occurred",
                );
            }
            return cancelled_after_delivery();
        }
        // Bounded settling interval before post-action capture. This is an
        // observation time, not proof the application finished reacting.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {}
            _ = context.ct.cancelled() => {
                if delivery == backend::Delivery::None {
                    return action_err(
                        "CANCELLED",
                        "request cancelled before any input was sent; no side effect occurred",
                    );
                }
                return cancelled_after_delivery();
            }
        }

        if delivery == backend::Delivery::None {
            if abort.is_cancelled() {
                return action_err(
                    "CANCELLED",
                    "request cancelled before any input was sent; no side effect occurred",
                );
            }
            // Stopped by a display change, not by cancel or a dead backend.
            if self.state.generation.load(Ordering::SeqCst) != obs.generation {
                return action_err(
                    "STALE_OBSERVATION",
                    "display configuration changed; call computer_observe for a fresh observation_id",
                );
            }
            return action_err(
                "INPUT_BACKEND_UNAVAILABLE",
                "input backend unavailable or action rejected before any input was sent",
            );
        }

        // Post-action observation on the target monitor (drag destination),
        // with the same capture bracketing as computer_observe: an image
        // captured across a configuration change is never recorded as an
        // actionable observation. `verified` is the (generation, fingerprint)
        // pair of the freshest post-capture snapshot that survived a bracket.
        let capture_started = Instant::now();
        let mut post_obs = None;
        let mut verified = None;
        for _ in 0..2 {
            let Some(Ok(pre)) = run_until_cancelled(&context, async {
                backend::monitors().await.map(backend::selectable)
            })
            .await
            else {
                break;
            };
            let gen_pre = self.state.generation.load(Ordering::SeqCst);
            let fp_pre = backend::fingerprint(&pre);
            let Some(m) = pre.iter().find(|m| m.name == post_monitor) else {
                break;
            };
            let Some(Ok(png)) = run_until_cancelled(&context, backend::capture(&m.name)).await
            else {
                break;
            };
            let Some(Ok(post)) = run_until_cancelled(&context, async {
                backend::monitors().await.map(backend::selectable)
            })
            .await
            else {
                break;
            };
            let gen_post = self.state.generation.load(Ordering::SeqCst);
            let fp_post = backend::fingerprint(&post);
            verified = Some((gen_post, fp_post));
            if gen_pre != gen_post || fp_pre != fp_post {
                continue;
            }
            if let Some((w, h)) = backend::png_size(&png) {
                post_obs = Some((m.clone(), png, w, h));
            }
            break;
        }
        // Display configuration changed between validation and the verified
        // post-action snapshot: even completed input gets a partial result.
        if let Some(action_timings) = &action_timings {
            action_timings.lock().unwrap().capture_ms =
                capture_started.elapsed().as_millis() as u64;
        }
        let raced = match verified {
            Some((g, f)) => g != obs.generation || f != fp,
            None => true,
        };

        match post_obs {
            Some((m, png, w, h)) if delivery == backend::Delivery::Completed && !raced => {
                let (g, f) = verified.unwrap();
                let mut v = serde_json::json!({
                    "outcome": "ok",
                    "effect": "completed",
                    "do_not_replay": false,
                    "action": action_json,
                });
                v["observation"] = self.record_observation(&m, w, h, g, f, &png);
                ok_result(v, Some(png))
            }
            _ => {
                let mut v = serde_json::json!({
                    "outcome": "partial",
                    "effect": effect,
                    "action": action_json,
                    "do_not_replay": true,
                    "display_changed_during_action": raced,
                });
                match post_obs {
                    Some((m, png, w, h)) => {
                        // Valid observation under the NEW configuration; it is
                        // current and actionable even though the action raced.
                        let (g, f) = verified.unwrap();
                        v["observation"] = self.record_observation(&m, w, h, g, f, &png);
                        partial_result(v, Some(png))
                    }
                    None => {
                        v["error"] = serde_json::json!({"code": "OBSERVATION_FAILED_AFTER_INPUT", "message": "input was attempted but no stable post-action observation could be produced; call computer_observe, do not repeat the action"});
                        partial_result(v, None)
                    }
                }
            }
        }
    }
}

/// Payload bound for type_text: clipboard replacement of huge text is
/// rejected before wl-copy spawns.
const MAX_TYPE_TEXT_BYTES: usize = 1024 * 1024;

/// Modifier name -> xkb modifier name on the uploaded keymap.
fn modifier(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some("Control"),
        "shift" => Some("Shift"),
        "alt" | "mod1" => Some("Mod1"),
        "super" | "meta" | "logo" | "mod4" => Some("Mod4"),
        _ => None,
    }
}

/// One batched event list: set depressed mods, key down+up, clear mods. The
/// keyboard session resolves names before emitting anything, so an unknown
/// name is Err (a rejection); a mid-batch transport failure is
/// Partial/Unknown with a best-effort release of keys and modifiers.
async fn key_chord(
    keyboard: &Arc<Keyboard>,
    mods: &[String],
    key: &str,
    abort: Arc<backend::Abort>,
) -> Result<backend::Delivery, String> {
    // Caller validated every modifier name already.
    let names: Vec<String> = mods
        .iter()
        .map(|m| modifier(m).unwrap().to_string())
        .collect();
    let has_mods = !names.is_empty();
    let mut ops: Vec<KeyOp> = Vec::new();
    if has_mods {
        ops.push(KeyOp::Mods { names });
    }
    ops.push(KeyOp::Key {
        name: key.to_string(),
        pressed: true,
    });
    ops.push(KeyOp::Key {
        name: key.to_string(),
        pressed: false,
    });
    if has_mods {
        ops.push(KeyOp::Mods { names: vec![] });
    }
    let kb = keyboard.clone();
    tokio::task::spawn_blocking(move || kb.apply(ops, abort))
        .await
        .unwrap_or(Ok(backend::Delivery::Unknown))
}

/// Clipboard-set then paste shortcut. Clipboard replacement alone is a side
/// effect: a paste failure after it is `partial`, never `none`.
async fn type_text(
    keyboard: &Arc<Keyboard>,
    text: &str,
    mods: Vec<String>,
    abort: Arc<backend::Abort>,
) -> backend::Delivery {
    // A cancelled request must not reach the clipboard: checked before
    // wl-copy spawns, and the subprocess is killed if cancel lands mid-run.
    match backend::clipboard_set(text, &abort).await {
        Ok(()) => {}
        // wl-copy never spawned: clipboard untouched, nothing happened.
        Err(backend::BackendError::Missing(_) | backend::BackendError::Cancelled) => {
            return backend::Delivery::None;
        }
        // wl-copy ran but failed (bad exit, timeout, broken stdin, killed on
        // cancel): the clipboard may already have been replaced, so report
        // unknown.
        Err(_) => return backend::Delivery::Unknown,
    }
    match key_chord(keyboard, &mods, "v", abort).await {
        // The paste never left the keyboard (backend down, action aborted),
        // but the clipboard was still replaced: that is a side effect.
        Ok(backend::Delivery::None) | Err(_) => backend::Delivery::Partial,
        Ok(d) => d,
    }
}

/// Aborts the spawned task when the request scope ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Result with possible input side effects: isError + a prominent no-replay
/// warning text block, then the compact JSON and optional image.
fn partial_result(
    value: serde_json::Value,
    image_png: Option<Vec<u8>>,
) -> Result<CallToolResult, McpError> {
    let mut content = vec![Content::text(
        "WARNING: input may already have been delivered. Do NOT repeat this action; call computer_observe and assess the screen first.",
    )];
    content.push(Content::text(value.to_string()));
    if let Some(png) = image_png {
        content.push(Content::image(
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png),
            "image/png",
        ));
    }
    let mut result = CallToolResult::error(content);
    result.structured_content = Some(value);
    Ok(result)
}

#[tool_handler]
impl ServerHandler for ComputerUse {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        let mut info = rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        );
        info.server_info.name = "computer-use-mcp".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "Screenshot-based control of the user's Hyprland desktop. \
             computer_monitors lists selectable monitors; computer_observe returns a \
             PNG screenshot and an opaque observation_id; computer_action performs one \
             click/scroll/key/type_text/drag action at observation coordinates and \
             returns a fresh screenshot. computer_goal is an experimental opt-in \
             native, browser, or local-OCR evidence path with TypeSafe selection. Browser \
             input requires the server operator's exact-origin allowlist. It hands back \
             when source identity, permissions, or completion cannot be verified. \
             Never repeat a partial/unknown action blindly."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--native-browser-host")
    {
        native_messaging::run()?;
        return Ok(());
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none()
        && std::env::var_os("XDG_RUNTIME_DIR").is_none()
    {
        eprintln!("warning: no Hyprland session environment; tools will report MISSING_DEPENDENCY");
    }
    // The browser bridge is optional. If its private runtime socket cannot be
    // created, existing screenshot and native tools remain available.
    let _ = native_messaging::start_browser_bridge();
    let service = ComputerUse::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_completion_stays_bound_to_the_original_compositor_window() {
        let target = fast_path::TargetScope {
            application: "Fixture App".into(),
            window: Some("OCR Fixture Window".into()),
            window_id: Some("0xocr-window".into()),
        };
        let active = serde_json::json!({
            "class": "fixture app", "title": "OCR Fixture Window",
            "address": "0xocr-window", "pid": 123
        });
        assert!(ocr_completion_window_matches(
            &active,
            &target,
            "0xocr-window",
            123
        ));

        let mut changed = active.clone();
        changed["address"] = serde_json::json!("0xother-window");
        assert!(!ocr_completion_window_matches(
            &changed,
            &target,
            "0xocr-window",
            123
        ));
        changed = active.clone();
        changed["title"] = serde_json::json!("Different title");
        assert!(!ocr_completion_window_matches(
            &changed,
            &target,
            "0xocr-window",
            123
        ));
        changed = active.clone();
        changed["class"] = serde_json::json!("other app");
        assert!(!ocr_completion_window_matches(
            &changed,
            &target,
            "0xocr-window",
            123
        ));
        changed = active.clone();
        changed["pid"] = serde_json::json!(456);
        assert!(!ocr_completion_window_matches(
            &changed,
            &target,
            "0xocr-window",
            123
        ));
    }

    fn test_state() -> State {
        State {
            epoch: Uuid::new_v4(),
            generation: Arc::new(AtomicU64::new(0)),
            events_healthy: Arc::new(AtomicBool::new(true)),
            wayland_healthy: Arc::new(AtomicBool::new(true)),
            observations: Mutex::new(HashMap::new()),
        }
    }

    fn insert_observation(state: &State, id: &str) {
        state.observations.lock().unwrap().insert(
            id.to_string(),
            Observation {
                monitor: "eDP-1".into(),
                image_width: 100,
                image_height: 100,
                generation: state.generation.load(Ordering::SeqCst),
                fingerprint: 0,
                image_png: None,
            },
        );
    }

    #[test]
    fn observation_survives_matching_generation() {
        let state = test_state();
        insert_observation(&state, "obs-1");
        assert!(state.validate_observation("obs-1", 0).is_ok());
    }

    #[test]
    fn generation_bump_invalidates_observation() {
        // Any display event, change-and-restore, or socket loss bumps the
        // generation; fingerprint equality cannot rescue an old observation.
        let state = test_state();
        insert_observation(&state, "obs-1");
        state.generation.fetch_add(1, Ordering::SeqCst);
        assert!(state.validate_observation("obs-1", 0).is_err());
    }

    #[test]
    fn newer_observation_supersedes_same_monitor() {
        let state = test_state();
        insert_observation(&state, "obs-old");
        // Simulate the observe path: keep only latest per monitor.
        let mut observations = state.observations.lock().unwrap();
        observations.retain(|_, o| o.monitor != "eDP-1");
        observations.insert(
            "obs-new".to_string(),
            Observation {
                monitor: "eDP-1".into(),
                image_width: 100,
                image_height: 100,
                generation: 0,
                fingerprint: 0,
                image_png: None,
            },
        );
        drop(observations);
        assert!(state.validate_observation("obs-old", 0).is_err());
        assert!(state.validate_observation("obs-new", 0).is_ok());
    }

    #[test]
    fn unknown_observation_rejected() {
        // Covers server restart: a new process has an empty store.
        let state = test_state();
        assert!(state.validate_observation("never-issued", 0).is_err());
    }

    #[test]
    fn input_admission_invalidates_observation_without_replacement() {
        let state = test_state();
        insert_observation(&state, "obs-1");
        state.invalidate_observation("obs-1");
        assert!(state.validate_observation("obs-1", 0).is_err());
    }

    #[test]
    fn action_history_is_bounded_and_redacts_literals() {
        let candidate = fast_path::CandidatePlan {
            id: "candidate-1".into(),
            description: "local-only".into(),
            element_id: "window/0".into(),
            source_revision: "revision-1".into(),
            action: fast_path::ActionSpec::TypeText {
                text: "SECRET-LITERAL".into(),
            },
        };
        let history = fast_action_history(&candidate, fast_path::ActionEffect::Partial, true);
        assert_eq!(history.as_array().unwrap().len(), 1);
        assert_eq!(history[0]["action"]["literal"], "redacted");
        assert!(!history.to_string().contains("SECRET-LITERAL"));
    }

    #[test]
    fn handback_after_a_delivered_step_preserves_history_and_blocks_blind_goal_replay() {
        let mut progress = FastPathGoalProgress::new();
        progress.absorb(&serde_json::json!({
            "reason": "completion_not_observed_after_action",
            "effect": "completed",
            "do_not_replay": false,
            "actions_executed": 1,
            "action_history": [{"candidate_id": "candidate-1", "effect": "completed"}],
            "selection": {"candidate_id": "candidate-1"},
            "timings_ms": {"inference_ms": 10, "input_ms": 2}
        }));

        let result = progress.result(
            serde_json::json!({"reason": "typesafe_service_error"}),
            "typesafe_service_error",
            false,
            2,
            0,
            "obs-next",
            None,
        );

        assert_eq!(result["actions_executed"], 1);
        assert_eq!(result["action_history"][0]["candidate_id"], "candidate-1");
        assert_eq!(result["do_not_replay"], true);
        assert_eq!(result["effect"], "completed");
        assert_eq!(result["timings_ms"]["inference_ms"], 10);
        assert_eq!(result["progress"]["remaining_actions"], 1);
    }

    #[test]
    fn native_mapping_requires_matching_process_frame_and_display() {
        let mut evidence: accessibility::Evidence = serde_json::from_value(serde_json::json!({
            "source":{"application":"fixture","window":"Fixture","window_id":"123:app:frame","source_kind":"native_accessibility","visible":true,"focused":true,"occluded":false},
            "coordinate_space":"unknown","native_frame":[56.0,12.0,1240.0,1415.0],"truncated":false,
            "elements":[{"id":"tab","role":"page tab","name":"Details","x":100.0,"y":125.0,"width":125.0,"height":50.0}]
        })).unwrap();
        let target = fast_path::TargetScope {
            application: "fixture".into(),
            window: Some("Fixture".into()),
            window_id: None,
        };
        let monitor = Monitor {
            id: 0,
            name: "eDP-1".into(),
            description: "fixture".into(),
            width: 2560,
            height: 1440,
            scale: 1.25,
            x: 0,
            y: 0,
            transform: 0,
            disabled: false,
        };
        let window = serde_json::json!({"address":"0x1","pid":123,"class":"fixture","title":"Fixture","monitor":0,"at":[45,10],"size":[992,1132],"xwayland":true});
        let original = evidence.clone();
        map_native_coordinates(&mut evidence, &target, &monitor, &window).unwrap();
        assert_eq!(evidence.coordinate_space, "desktop_logical");
        assert_eq!(
            (evidence.elements[0].x, evidence.elements[0].y),
            (80.0, 100.0)
        );
        for field in ["pid", "at", "xwayland"] {
            let mut changed = window.clone();
            changed[field] = match field {
                "pid" => serde_json::json!(124),
                "at" => serde_json::json!([46, 10]),
                _ => serde_json::json!(false),
            };
            assert!(
                map_native_coordinates(&mut original.clone(), &target, &monitor, &changed).is_err()
            );
        }
        let mut rotated = monitor;
        rotated.transform = 1;
        assert!(map_native_coordinates(&mut original.clone(), &target, &rotated, &window).is_err());
    }

    #[test]
    fn browser_policy_requires_an_exact_origin_and_defaults_to_denial() {
        let policy = r#"["https://approved.example","http://127.0.0.1:8000"]"#;
        assert!(browser_origin_allowed("https://approved.example", policy));
        assert!(browser_origin_allowed("http://127.0.0.1:8000", policy));
        for origin in [
            "",
            "null",
            "https://approved.example.evil",
            "https://sub.approved.example",
            "https://approved.example/path",
            "https://approved.example?token=secret",
            "https://user@approved.example",
            "http://127.0.0.1:8001",
        ] {
            assert!(!browser_origin_allowed(origin, policy), "{origin}");
        }
        for policy in ["", "*", r#"["*"]"#, r#"["https://*.example"]"#, "{}"] {
            assert!(!browser_origin_allowed("https://approved.example", policy));
        }
    }

    #[test]
    fn chrome_viewport_uses_measured_document_geometry_for_both_platforms() {
        for native in [false, true] {
            let geometry = accessibility::BrowserGeometry {
                process_id: 123,
                frame: if native {
                    [0.0, 0.0, 992.0, 1132.0]
                } else {
                    [56.0, 12.0, 1240.0, 1415.0]
                },
                document: if native {
                    [0.0, 108.0, 1240.0, 1307.0]
                } else {
                    [56.0, 99.0, 1240.0, 1328.0]
                },
            };
            let viewport = accessibility::BrowserViewport {
                screen_x: 0.0,
                screen_y: 0.0,
                width: if native { 992.0 } else { 1240.0 },
                height: if native { 1046.0 } else { 1328.0 },
                device_pixel_ratio: if native { 1.25 } else { 1.0 },
                visual_scale: 1.0,
            };
            let window =
                serde_json::json!({"pid":123,"at":[45,10],"size":[992,1132],"xwayland":!native});
            let (x, y, sx, sy) =
                chrome_viewport_geometry(&geometry, &viewport, &window, 1.25).unwrap();
            assert_eq!(x, 45.0);
            assert!((x + viewport.width * sx - 1037.0).abs() < 0.001);
            assert!((y + viewport.height * sy - 1142.0).abs() < 0.001);
            let mut other = window.clone();
            other["pid"] = serde_json::json!(456);
            assert!(chrome_viewport_geometry(&geometry, &viewport, &other, 1.25).is_err());
            let mut zoomed = viewport;
            zoomed.width -= 20.0;
            assert!(chrome_viewport_geometry(&geometry, &zoomed, &window, 1.25).is_err());
        }
    }

    #[test]
    fn browser_css_geometry_is_mapped_only_inside_the_focused_verified_window() {
        let mut monitor = Monitor {
            name: "eDP-1".into(),
            description: "fixture".into(),
            x: 0,
            y: 0,
            width: 1600,
            height: 900,
            scale: 1.25,
            transform: 0,
            disabled: false,
            id: 3,
        };
        let target = fast_path::TargetScope {
            application: "Firefox".into(),
            window: Some("Fixture tab".into()),
            window_id: None,
        };
        let mut evidence: accessibility::Evidence = serde_json::from_value(serde_json::json!({
            "source": {
                "application": "Firefox", "window": "Fixture tab",
                "window_id": "1:2:document-1", "revision": "document-1",
                "visible": true, "focused": true, "occluded": false,
                "source_kind": "browser_extension", "active_tab": true,
                "browser_window_id": "1", "browser_tab_id": "2",
                "document_id": "document-1", "geometry_verified": true
            },
            "coordinate_space": "browser_viewport_css",
            "browser_viewport": {
                "screen_x": 20.0, "screen_y": 30.0,
                "width": 500.0, "height": 300.0,
                "device_pixel_ratio": 1.25, "visual_scale": 1.0
            },
            "captured_at_unix_ms": 1,
            "truncated": false,
            "elements": [{
                "id": "tab-1", "role": "tab", "name": "Open details",
                "x": 12.0, "y": 22.0, "width": 80.0, "height": 30.0,
                "visible": true, "enabled": true, "showing": true,
                "focused": false, "selected": false,
                "editable": false, "protected": false
            }]
        }))
        .unwrap();
        let window = serde_json::json!({
            "class": "firefox", "title": "Fixture tab — Mozilla Firefox",
            "address": "0x1", "pid": 123, "monitor": 3, "xwayland": true,
            "at": [10, 10], "size": [600, 400]
        });
        let clients = vec![window.clone()];

        map_browser_coordinates(&mut evidence, &target, &monitor, &window, &clients, None).unwrap();
        assert_eq!(evidence.coordinate_space, "desktop_logical");
        assert_eq!(evidence.elements[0].x, 32.0);
        assert_eq!(evidence.elements[0].y, 52.0);
        let binding = dispatch_window_binding(
            fast_path::EvidenceSourceKind::BrowserExtension,
            &target,
            &evidence,
            monitor.id,
            &window,
        )
        .unwrap();
        assert!(binding.matches(&window));
        let mut missing_pid = window.clone();
        missing_pid.as_object_mut().unwrap().remove("pid");
        assert!(
            dispatch_window_binding(
                fast_path::EvidenceSourceKind::BrowserExtension,
                &target,
                &evidence,
                monitor.id,
                &missing_pid,
            )
            .is_none()
        );
        let mut another_window = window.clone();
        another_window["address"] = serde_json::json!("0x2");
        assert!(!binding.matches(&another_window));
        let mut moved_window = window.clone();
        moved_window["at"][0] = serde_json::json!(11);
        assert!(!binding.matches(&moved_window));

        let mut wayland_window = window.clone();
        wayland_window["xwayland"] = serde_json::json!(false);
        let mut relative = evidence.clone();
        relative.coordinate_space = "browser_viewport_css".into();
        relative.elements[0].x = 12.0;
        relative.elements[0].y = 22.0;
        relative.browser_viewport.as_mut().unwrap().screen_x = 0.0;
        relative.browser_viewport.as_mut().unwrap().screen_y = 50.0;
        map_browser_coordinates(
            &mut relative,
            &target,
            &monitor,
            &wayland_window,
            std::slice::from_ref(&wayland_window),
            None,
        )
        .unwrap();
        assert_eq!(relative.elements[0].x, 22.0);
        assert_eq!(relative.elements[0].y, 82.0);

        let mut native_chrome = relative.clone();
        native_chrome.coordinate_space = "browser_viewport_css".into();
        native_chrome.source.application = "Chrome".into();
        let chrome_target = fast_path::TargetScope {
            application: "Chrome".into(),
            window: Some("Fixture tab".into()),
            window_id: None,
        };
        let mut chrome_window = wayland_window.clone();
        chrome_window["class"] = serde_json::json!("google-chrome");
        assert_eq!(
            map_browser_coordinates(
                &mut native_chrome,
                &chrome_target,
                &monitor,
                &chrome_window,
                std::slice::from_ref(&chrome_window),
                None,
            ),
            Err("browser_accessibility_geometry_unavailable")
        );

        let mut duplicate_window = window.clone();
        duplicate_window["address"] = serde_json::json!("0x2");
        let mut ambiguous = evidence.clone();
        ambiguous.coordinate_space = "browser_viewport_css".into();
        assert_eq!(
            map_browser_coordinates(
                &mut ambiguous,
                &target,
                &monitor,
                &window,
                &[window.clone(), duplicate_window],
                None,
            ),
            Err("browser_window_identity_is_ambiguous")
        );

        let mut bad = evidence.clone();
        bad.coordinate_space = "browser_viewport_css".into();
        bad.browser_viewport.as_mut().unwrap().device_pixel_ratio = 1.0;
        assert_eq!(
            map_browser_coordinates(&mut bad, &target, &monitor, &window, &clients, None),
            Err("browser_zoom_or_display_scale_is_unverified")
        );

        monitor.transform = 1;
        let mut rotated = evidence;
        rotated.coordinate_space = "browser_viewport_css".into();
        rotated.elements[0].x = 12.0;
        rotated.elements[0].y = 22.0;
        assert_eq!(
            map_browser_coordinates(&mut rotated, &target, &monitor, &window, &clients, None),
            Err("browser_window_is_not_the_focused_supported_surface")
        );
    }

    #[test]
    fn browser_candidate_binding_rejects_a_same_title_tab_or_changed_target() {
        let target = fast_path::TargetScope {
            application: "Firefox".into(),
            window: Some("Fixture tab".into()),
            window_id: None,
        };
        let mut evidence: accessibility::Evidence = serde_json::from_value(serde_json::json!({
            "source": {
                "application": "Firefox", "window": "Fixture tab",
                "window_id": "1:2:document-1", "revision": "document-1",
                "visible": true, "focused": true, "occluded": false,
                "source_kind": "browser_extension", "active_tab": true,
                "browser_window_id": "1", "browser_tab_id": "2",
                "document_id": "document-1", "geometry_verified": true
            },
            "coordinate_space": "desktop_logical",
            "window_geometry": {"x": 10.0, "y": 10.0, "width": 600.0, "height": 400.0},
            "browser_viewport": {
                "screen_x": 20.0, "screen_y": 30.0,
                "width": 500.0, "height": 300.0,
                "device_pixel_ratio": 1.25, "visual_scale": 1.0
            },
            "captured_at_unix_ms": 1,
            "truncated": false,
            "elements": [{
                "id": "tab-1", "role": "tab", "name": "Open details",
                "x": 32.0, "y": 52.0, "width": 100.0, "height": 37.5,
                "visible": true, "enabled": true, "showing": true,
                "focused": false, "selected": false,
                "editable": false, "protected": false
            }]
        }))
        .unwrap();
        evidence.browser_actions_authorized = true;
        let expected = BrowserCandidateBinding {
            target,
            evidence_revision: evidence.revision(),
            browser_window_id: evidence.source.browser_window_id.clone(),
            browser_tab_id: evidence.source.browser_tab_id.clone(),
            document_id: evidence.source.document_id.clone(),
            window_id: evidence.source.window_id.clone(),
        };
        assert!(browser_candidate_matches(&expected, &evidence));

        let mut other_tab = evidence.clone();
        other_tab.source.browser_tab_id = "3".into();
        other_tab.source.window_id = "1:3:document-1".into();
        assert!(!browser_candidate_matches(&expected, &other_tab));

        let mut moved_tab = evidence.clone();
        moved_tab.elements[0].x += 2.0;
        assert!(!browser_candidate_matches(&expected, &moved_tab));
    }

    #[test]
    fn non_ascii_app_names_remain_distinct_when_binding_native_windows() {
        assert_ne!(
            normalized_app_identity("메모"),
            normalized_app_identity("계산기")
        );
        assert_ne!(
            normalized_app_identity("메모 2"),
            normalized_app_identity("계산기 2")
        );
        assert_eq!(
            normalized_app_identity("GNOME_Text Editor"),
            normalized_app_identity("gnome-text-editor")
        );

        let monitor = Monitor {
            name: "eDP-1".into(),
            description: "fixture".into(),
            x: 0,
            y: 0,
            width: 1600,
            height: 900,
            scale: 1.0,
            transform: 0,
            disabled: false,
            id: 3,
        };
        let target = fast_path::TargetScope {
            application: "Foo-Bar".into(),
            window: Some("Same title".into()),
            window_id: None,
        };
        let evidence = accessibility::Evidence {
            source: accessibility::EvidenceSource {
                application: "Foo-Bar".into(),
                window: "Same title".into(),
                window_id: "123:foo-app:window-1".into(),
                process_id: Some(123),
                revision: "revision-1".into(),
                visible: true,
                focused: true,
                occluded: false,
                source_kind: "native_accessibility".into(),
                active_tab: false,
                browser_window_id: String::new(),
                browser_tab_id: String::new(),
                document_id: String::new(),
                geometry_verified: false,
                browser_origin: String::new(),
            },
            browser_actions_authorized: false,
            coordinate_space: "desktop_logical".into(),
            native_frame: None,
            window_geometry: None,
            browser_viewport: None,
            captured_at_unix_ms: 0,
            elements: Vec::new(),
            truncated: false,
        };
        let wrong_app = serde_json::json!({
            "class": "foo_bar", "title": "Same title", "address": "0x1",
            "pid": 456, "monitor": 3, "at": [0, 0], "size": [800, 600]
        });
        assert!(
            dispatch_window_binding(
                fast_path::EvidenceSourceKind::NativeAccessibility,
                &target,
                &evidence,
                monitor.id,
                &wrong_app,
            )
            .is_none()
        );
        let matching_process = serde_json::json!({
            "class": "foo_bar", "title": "Same title", "address": "0x1",
            "pid": 123, "monitor": 3, "at": [0, 0], "size": [800, 600]
        });
        let binding = dispatch_window_binding(
            fast_path::EvidenceSourceKind::NativeAccessibility,
            &target,
            &evidence,
            monitor.id,
            &matching_process,
        )
        .unwrap();
        assert!(binding.matches(&matching_process));
        let mut different_process = matching_process.clone();
        different_process["pid"] = serde_json::json!(456);
        assert!(!binding.matches(&different_process));
    }
}
