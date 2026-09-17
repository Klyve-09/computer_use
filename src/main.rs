mod backend;

use backend::{BackendError, Monitor};
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

// Fields beyond `generation` are consumed by `computer_action` (issue #3);
// they are stored now because they define what an observation binds to.
#[derive(Debug)]
#[allow(dead_code)]
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
        format!("{}-{}-{:016x}", self.epoch, self.generation.load(Ordering::SeqCst), fingerprint)
    }

    /// Used by `computer_action` (issue #3); tested now because stale-
    /// observation rejection is part of this ticket's contract.
    /// An observation authorizes later input only while its generation is
    /// still current. Session epoch is implicit: observations from a previous
    /// process are simply absent. Generation bumps cover display events,
    /// change-and-restore, and event-socket loss.
    #[allow(dead_code)]
    fn observation_current(&self, id: &str) -> Result<(), &'static str> {
        let observations = self.observations.lock().unwrap();
        let obs = observations.get(id).ok_or("unknown or expired observation")?;
        if obs.generation != self.generation.load(Ordering::SeqCst) {
            return Err("stale observation: display configuration changed");
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ComputerUse {
    state: Arc<State>,
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
            tool_router: Self::tool_router(),
        }
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

fn ok_result(value: serde_json::Value, image_png: Option<Vec<u8>>) -> Result<CallToolResult, McpError> {
    let mut content = vec![Content::text(value.to_string())];
    if let Some(png) = image_png {
        content.push(Content::image(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png), "image/png"));
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
    async fn computer_observe(&self, Parameters(p): Parameters<ObserveParams>) -> Result<CallToolResult, McpError> {
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
                    format!("no selectable monitor {:?}; call computer_monitors", p.monitor),
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
            let observation_id = Uuid::new_v4().to_string();
            // Keep only the latest observation per monitor: a newer capture
            // supersedes earlier observation IDs for the same output.
            let mut observations = self.state.observations.lock().unwrap();
            observations.retain(|_, o| o.monitor != monitor.name);
            observations.insert(
                observation_id.clone(),
                Observation {
                    monitor: monitor.name.clone(),
                    image_width: w,
                    image_height: h,
                    generation: gen_post,
                    fingerprint: fp_post,
                },
            );
            return ok_result(
                serde_json::json!({
                    "observation_id": observation_id,
                    "monitor": Self::monitor_summary(monitor),
                    "image": { "width_px": w, "height_px": h, "mime_type": "image/png" },
                    "revision": self.state.revision(fp_post),
                    "actionable": self.state.actionable(),
                }),
                Some(png),
            );
        }
        err_result(
            "DISPLAY_CONFIGURATION_CHANGED",
            "display configuration changed during capture; call computer_observe again",
        )
    }
}

#[tool_handler]
impl ServerHandler for ComputerUse {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        let mut info = rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder().enable_tools().build(),
        );
        info.server_info.name = "computer-use-mcp".into();
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.instructions = Some(
            "Screenshot-based control of the user's Hyprland desktop. \
             computer_monitors lists selectable monitors; computer_observe returns a \
             PNG screenshot and an opaque observation_id for later actions."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() && std::env::var_os("XDG_RUNTIME_DIR").is_none() {
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
        assert!(state.observation_current("obs-1").is_ok());
    }

    #[test]
    fn generation_bump_invalidates_observation() {
        // Any display event, change-and-restore, or socket loss bumps the
        // generation; fingerprint equality cannot rescue an old observation.
        let state = test_state();
        insert_observation(&state, "obs-1");
        state.generation.fetch_add(1, Ordering::SeqCst);
        assert!(state.observation_current("obs-1").is_err());
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
        assert!(state.observation_current("obs-old").is_err());
        assert!(state.observation_current("obs-new").is_ok());
    }

    #[test]
    fn unknown_observation_rejected() {
        // Covers server restart: a new process has an empty store.
        let state = test_state();
        assert!(state.observation_current("never-issued").is_err());
    }
}
