mod backend;

use backend::{BackendError, KeyOp, Keyboard, Monitor, Pointer, PointerOp};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock as Content, ErrorData as McpError};
use rmcp::service::ServiceExt;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ObserveParams {
    /// Monitor identifier from `computer_monitors` (the Hyprland output name).
    pub monitor: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClickButton {
    Left,
    Right,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
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
}

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

fn backend_error(e: BackendError) -> Result<CallToolResult, McpError> {
    let code = match &e {
        BackendError::Missing(_) => "MISSING_DEPENDENCY",
        BackendError::Timeout(_) => "BACKEND_TIMEOUT",
        BackendError::TooLarge(_) => "CAPTURE_TOO_LARGE",
        BackendError::Failed(_) => "BACKEND_FAILED",
    };
    err_result(code, e.to_string())
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
            let observation_json = self.record_observation(monitor, w, h, gen_post, fp_post);
            return ok_result(observation_json, Some(png));
        }
        err_result(
            "DISPLAY_CONFIGURATION_CHANGED",
            "display configuration changed during capture; call computer_observe again",
        )
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
        Parameters(p): Parameters<ActionParams>,
    ) -> Result<CallToolResult, McpError> {
        let _serial = self.action_lock.lock().await;

        let snapshot = match backend::monitors().await.map(backend::selectable) {
            Ok(m) => m,
            Err(e) => return backend_error(e),
        };
        let fp = backend::fingerprint(&snapshot);
        let obs = match self.state.validate_observation(&p.observation_id, fp) {
            Ok(o) => o,
            Err(msg) => {
                return action_err(
                    "STALE_OBSERVATION",
                    format!("{msg}; call computer_observe for a fresh observation_id"),
                );
            }
        };
        if !self.state.actionable() {
            return action_err(
                "EVENT_CHANNEL_UNHEALTHY",
                "display-change notification channel is down; mutation paused until it recovers",
            );
        }
        let Some(monitor) = snapshot.iter().find(|m| m.name == obs.monitor) else {
            return action_err("MONITOR_NOT_FOUND", "observation's monitor is gone");
        };
        // Keyboard/text actions act on the focused window; it must live on
        // the observed monitor, else reject before any input.
        if matches!(
            p.action,
            ActionKind::Key { .. } | ActionKind::TypeText { .. }
        ) {
            match backend::focused_monitor().await {
                Ok(Some(name)) if name == obs.monitor => {}
                Ok(_) => {
                    return action_err(
                        "FOCUS_MISMATCH",
                        "focus is on another monitor or nothing is focused; click the target first",
                    );
                }
                Err(e) => return backend_error(e),
            }
        }

        // Post-action observation target: the drag destination monitor.
        let mut post_monitor = obs.monitor.clone();
        let delivery = match &p.action {
            ActionKind::Key { key, mods } => {
                if let Some(bad) = mods.iter().find(|m| modifier(m).is_none()) {
                    return action_err(
                        "INVALID_MODIFIER",
                        format!("unknown modifier {bad:?}; use ctrl/shift/alt/super"),
                    );
                }
                let guard = Some((self.state.generation.clone(), obs.generation));
                match key_chord(&self.keyboard, mods, key, guard).await {
                    Ok(d) => d,
                    Err(msg) => return action_err("INVALID_KEY", msg),
                }
            }
            ActionKind::TypeText { text, paste } => {
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
                let guard = Some((self.state.generation.clone(), obs.generation));
                type_text(&self.keyboard, text, mods, guard).await
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
                let pointer = self.pointer.clone();
                let guard = Some((self.state.generation.clone(), obs.generation));
                tokio::task::spawn_blocking(move || pointer.apply(ops, guard))
                    .await
                    .unwrap_or(backend::Delivery::Unknown)
            }
        };

        // Bounded settling interval before post-action capture. This is an
        // observation time, not proof the application finished reacting.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let post = backend::monitors().await.map(backend::selectable);
        let raced = match &post {
            Ok(m) => {
                backend::fingerprint(m) != fp
                    || self.state.generation.load(Ordering::SeqCst) != obs.generation
            }
            Err(_) => true,
        };

        let effect = match delivery {
            backend::Delivery::Completed => "completed",
            backend::Delivery::Partial => "partial",
            backend::Delivery::Unknown => "unknown",
            backend::Delivery::None => "none",
        };
        let action_json = serde_json::to_value(&p.action).unwrap_or_default();

        if delivery == backend::Delivery::None {
            return action_err(
                "INPUT_BACKEND_UNAVAILABLE",
                "input backend unavailable or action rejected before any input was sent",
            );
        }

        // Post-action observation on the target monitor (drag destination).
        let post_obs = match &post {
            Ok(ms) => match ms.iter().find(|m| m.name == post_monitor) {
                Some(m) => match backend::capture(&m.name).await {
                    Ok(png) => match backend::png_size(&png) {
                        Some((w, h)) => Some((m.clone(), png, w, h)),
                        None => None,
                    },
                    Err(_) => None,
                },
                None => None,
            },
            Err(_) => None,
        };

        if delivery != backend::Delivery::Completed || raced {
            let mut v = serde_json::json!({
                "outcome": "partial",
                "effect": effect,
                "action": action_json,
                "do_not_replay": true,
                "display_changed_during_action": raced,
            });
            match post_obs {
                Some((m, png, w, h)) => {
                    let generation = self.state.generation.load(Ordering::SeqCst);
                    let fp_now = post
                        .as_ref()
                        .map(|ms| backend::fingerprint(ms))
                        .unwrap_or(fp);
                    v["observation"] = self.record_observation(&m, w, h, generation, fp_now);
                    return partial_result(v, Some(png));
                }
                None => {
                    v["error"] = serde_json::json!({"code": "OBSERVATION_FAILED_AFTER_INPUT", "message": "input was attempted but the post-action capture failed; call computer_observe"});
                    return partial_result(v, None);
                }
            }
        }

        match post_obs {
            Some((m, png, w, h)) => {
                let generation = self.state.generation.load(Ordering::SeqCst);
                let fp_now = post
                    .as_ref()
                    .map(|ms| backend::fingerprint(ms))
                    .unwrap_or(fp);
                let mut v = serde_json::json!({
                    "outcome": "ok",
                    "effect": "completed",
                    "action": action_json,
                });
                v["observation"] = self.record_observation(&m, w, h, generation, fp_now);
                ok_result(v, Some(png))
            }
            None => partial_result(
                serde_json::json!({
                    "outcome": "partial",
                    "effect": "completed",
                    "action": action_json,
                    "do_not_replay": true,
                    "error": {"code": "SCREENSHOT_FAILED_AFTER_ACTION", "message": "input completed but the post-action capture failed; call computer_observe, do not repeat the action"},
                }),
                None,
            ),
        }
    }
}

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
    guard: Option<(Arc<AtomicU64>, u64)>,
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
    tokio::task::spawn_blocking(move || kb.apply(ops, guard))
        .await
        .unwrap_or(Ok(backend::Delivery::Unknown))
}

/// Clipboard-set then paste shortcut. Clipboard replacement alone is a side
/// effect: a paste failure after it is `partial`, never `none`.
async fn type_text(
    keyboard: &Arc<Keyboard>,
    text: &str,
    mods: Vec<String>,
    guard: Option<(Arc<AtomicU64>, u64)>,
) -> backend::Delivery {
    if backend::clipboard_set(text).await.is_err() {
        return backend::Delivery::None;
    }
    match key_chord(keyboard, &mods, "v", guard).await {
        Ok(d) => d,
        Err(_) => backend::Delivery::Partial, // 'v'/ctrl/shift always resolve
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
             returns a fresh screenshot. Never repeat a partial/unknown action blindly."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none()
        && std::env::var_os("XDG_RUNTIME_DIR").is_none()
    {
        eprintln!("warning: no Hyprland session environment; tools will report MISSING_DEPENDENCY");
    }
    let service = ComputerUse::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
