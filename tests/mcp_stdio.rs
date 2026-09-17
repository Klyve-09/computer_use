use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Drives the real server over stdio JSON-RPC, the public seam Codex uses.
/// Requires a live Hyprland session (hyprctl/grim); skipped otherwise.
struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn start() -> Option<Self> {
        Self::start_with(&[])
    }

    fn start_with(extra_env: &[(String, String)]) -> Option<Self> {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            eprintln!("skipping: not in a Hyprland session");
            return None;
        }
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_computer-use-mcp"));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Some(Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id").and_then(Value::as_u64) == Some(self.next_id) {
                return v;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{msg}").unwrap();
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn init(client: &mut Client) -> Value {
    let r = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "stdio-test", "version": "0"}
        }),
    );
    client.notify("notifications/initialized");
    r
}

#[test]
fn initialize_and_list_tools() {
    let Some(mut c) = Client::start() else { return };
    let init_result = init(&mut c);
    assert!(init_result.get("error").is_none(), "{init_result}");
    assert!(init_result["result"]["capabilities"]["tools"].is_object());

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"computer_monitors"), "{names:?}");
    assert!(names.contains(&"computer_observe"), "{names:?}");
}

#[test]
fn monitors_then_observe_real_display() {
    let Some(mut c) = Client::start() else { return };
    init(&mut c);

    let monitors = c.call_tool("computer_monitors", json!({}));
    assert_ne!(monitors["result"]["isError"], json!(true), "{monitors}");
    let structured = &monitors["result"]["structuredContent"];
    let list = structured["monitors"].as_array().unwrap();
    assert!(!list.is_empty());
    let first = &list[0];
    assert!(first["id"].is_string());
    assert!(first["logical_bounds"]["width"].as_u64().unwrap() > 0);
    assert!(structured["revision"].is_string());
    // Compact JSON text block must agree with structuredContent.
    let text = &monitors["result"]["content"][0]["text"];
    assert_eq!(
        serde_json::from_str::<Value>(text.as_str().unwrap()).unwrap(),
        *structured
    );

    let observe = c.call_tool("computer_observe", json!({"monitor": first["id"]}));
    assert_ne!(observe["result"]["isError"], json!(true), "{observe}");
    let obs = &observe["result"]["structuredContent"];
    assert!(obs["observation_id"].is_string());
    let image = observe["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "image")
        .expect("image block missing");
    assert_eq!(image["mimeType"], "image/png");
    assert!(image["data"].as_str().unwrap().len() > 1000);
    assert!(obs["image"]["width_px"].as_u64().unwrap() > 0);

    // Unknown monitor is a caller-visible execution failure, not a protocol error.
    let bad = c.call_tool("computer_observe", json!({"monitor": "NOPE-9"}));
    assert_eq!(bad["result"]["isError"], json!(true));
    assert_eq!(
        bad["result"]["structuredContent"]["error"]["code"],
        "MONITOR_NOT_FOUND"
    );
}

#[test]
fn action_rejects_stale_and_invalid_without_input() {
    let Some(mut c) = Client::start() else { return };
    init(&mut c);

    // Unknown observation -> STALE_OBSERVATION, isError, no side effect.
    let stale = c.call_tool(
        "computer_action",
        json!({"observation_id": "bogus", "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    assert_eq!(stale["result"]["isError"], json!(true), "{stale}");
    assert_eq!(
        stale["result"]["structuredContent"]["error"]["code"],
        "STALE_OBSERVATION"
    );

    // Out-of-bounds point -> INVALID_COORDINATES, rejected before input.
    let obs = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let w = obs["result"]["structuredContent"]["image"]["width_px"]
        .as_u64()
        .unwrap();
    let bad = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "click", "x": w + 50, "y": 0, "button": "left"}}),
    );
    assert_eq!(bad["result"]["isError"], json!(true), "{bad}");
    assert_eq!(
        bad["result"]["structuredContent"]["error"]["code"],
        "INVALID_COORDINATES"
    );

    // A superseded observation is rejected too.
    let obs2 = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    assert!(obs2["result"]["structuredContent"]["observation_id"].is_string());
    let superseded = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    assert_eq!(
        superseded["result"]["structuredContent"]["error"]["code"],
        "STALE_OBSERVATION"
    );
}

#[test]
fn key_and_text_rejections() {
    let Some(mut c) = Client::start() else { return };
    init(&mut c);

    // Stale/unknown observation -> rejected before any input.
    for action in [
        json!({"kind": "key", "key": "a"}),
        json!({"kind": "type_text", "text": "hello"}),
    ] {
        let r = c.call_tool(
            "computer_action",
            json!({"observation_id": "bogus", "action": action}),
        );
        assert_eq!(
            r["result"]["structuredContent"]["error"]["code"], "STALE_OBSERVATION",
            "{r}"
        );
        assert_eq!(r["result"]["structuredContent"]["effect"], "none");
    }

    let obs = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Unknown modifier name -> rejected before any key event.
    let bad_mod = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "key", "key": "a", "mods": ["banana"]}}),
    );
    let code = bad_mod["result"]["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or("");
    // Focus may not be on eDP-1 in a test run; either rejection is acceptable.
    assert!(
        code == "INVALID_MODIFIER" || code == "FOCUS_MISMATCH",
        "{bad_mod}"
    );

    // Invalid paste mode must reject BEFORE the clipboard is replaced.
    if Command::new("wl-copy")
        .arg("sentinel")
        // wl-copy forks a clipboard daemon; null stdio so it can't hold the
        // test harness's pipes open.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        let bad_paste = c.call_tool(
            "computer_action",
            json!({"observation_id": oid, "action": {"kind": "type_text", "text": "CLOBBER", "paste": "bogus"}}),
        );
        let code = bad_paste["result"]["structuredContent"]["error"]["code"]
            .as_str()
            .unwrap_or("");
        assert!(
            code == "INVALID_PASTE" || code == "FOCUS_MISMATCH",
            "{bad_paste}"
        );
        let clip = Command::new("wl-paste").output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&clip.stdout).trim_end(),
            "sentinel",
            "clipboard must be untouched"
        );
    }

    // Key name that resolves to no keycode -> rejected before any event;
    // the observation is consumed (post-action observe supersedes it only on
    // success, so reuse a fresh one if needed).
    let bad_key = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "key", "key": "NoSuchKeysym42"}}),
    );
    let code = bad_key["result"]["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or("");
    // Focus may not be on eDP-1 in a test run; either rejection is acceptable.
    assert!(
        code == "INVALID_KEY" || code == "FOCUS_MISMATCH",
        "{bad_key}"
    );
}

#[test]
fn drag_rejects_stale_or_bad_destination() {
    let Some(mut c) = Client::start() else { return };
    init(&mut c);
    let obs = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Unknown destination observation -> rejected before any press.
    let stale_dst = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "drag", "x": 10, "y": 10, "dst_observation_id": "bogus", "dst_x": 20, "dst_y": 20}}),
    );
    assert_eq!(
        stale_dst["result"]["structuredContent"]["error"]["code"], "STALE_OBSERVATION",
        "{stale_dst}"
    );
    assert_eq!(stale_dst["result"]["structuredContent"]["effect"], "none");

    // Destination point outside its image -> INVALID_COORDINATES.
    let obs2 = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid2 = obs2["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dw = obs2["result"]["structuredContent"]["image"]["width_px"]
        .as_u64()
        .unwrap();
    let bad_dst = c.call_tool(
        "computer_action",
        json!({"observation_id": oid2, "action": {"kind": "drag", "x": 10, "y": 10, "dst_observation_id": oid2, "dst_x": dw + 100, "dst_y": 0}}),
    );
    assert_eq!(
        bad_dst["result"]["structuredContent"]["error"]["code"], "INVALID_COORDINATES",
        "{bad_dst}"
    );
}

#[test]
fn unknown_tool_is_protocol_error() {
    let Some(mut c) = Client::start() else { return };
    init(&mut c);
    let r = c.call_tool("computer_nope", json!({}));
    assert!(r.get("error").is_some());
}

// ---------------------------------------------------------------------------
// Regression tests for the 2026-09-17 review findings. They drive the real
// stdio server against controlled seams: a fake Hyprland event socket
// (COMPUTER_USE_EVENT_SOCKET), PATH-shadowed wl-copy/grim wrappers, and a
// silent input socket (COMPUTER_USE_INPUT_SOCKET). No user clipboard is
// touched; real input sent is a harmless Shift_L press at most.
// ---------------------------------------------------------------------------

/// Controlled Hyprland event socket. The pump forwards `configreloaded`
/// whenever a `trigger` file appears in the rig dir; accepted peers are kept
/// alive so the server's watcher stays connected and healthy.
struct Rig {
    dir: std::path::PathBuf,
    // Held only to keep accepted watcher connections open for the test.
    _keep: std::sync::Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>>,
}

fn event_rig() -> Rig {
    let dir = std::env::temp_dir().join(format!(
        "cut-rig-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(dir.join("events.sock")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<std::os::unix::net::UnixStream>();
    std::thread::spawn(move || {
        while let Ok((peer, _)) = listener.accept() {
            let _ = tx.send(peer);
        }
    });
    let keep = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let pump_dir = dir.clone();
    let pump_keep = keep.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(5));
            while let Ok(peer) = rx.try_recv() {
                pump_keep.lock().unwrap().push(peer);
            }
            let trigger = pump_dir.join("trigger");
            if !trigger.exists() {
                continue;
            }
            std::fs::remove_file(&trigger).ok();
            use std::io::Write;
            for peer in pump_keep.lock().unwrap().iter_mut() {
                let _ = peer.write_all(b"configreloaded>>\n");
            }
        }
    });
    Rig { dir, _keep: keep }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn script(rig: &Rig, name: &str, body: &str) {
    let path = rig.dir.join(name);
    std::fs::write(&path, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn rig_env(rig: &Rig) -> Vec<(String, String)> {
    let path = format!("{}:{}", rig.dir.display(), std::env::var("PATH").unwrap());
    vec![
        (
            "COMPUTER_USE_EVENT_SOCKET".into(),
            rig.dir.join("events.sock").display().to_string(),
        ),
        ("PATH".into(), path),
    ]
}

/// A config event arriving mid-post-action-capture must never produce an
/// `outcome: ok` result with a fresh actionable observation (review P1).
#[test]
fn config_change_during_post_action_capture_is_partial() {
    let rig = event_rig();
    // Wrap real grim: after capturing, trip the event trigger and wait so
    // the event lands inside the server's capture bracket.
    script(
        &rig,
        "grim",
        &format!(
            "#!/bin/sh\n/usr/bin/grim \"$@\"\nif [ -e \"{d}/race_capture\" ]; then touch \"{d}/trigger\"; sleep 0.3; fi\n",
            d = rig.dir.display()
        ),
    );
    let Some(mut c) = Client::start_with(&rig_env(&rig)) else {
        return;
    };
    init(&mut c);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let obs = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    std::fs::write(rig.dir.join("race_capture"), "").unwrap();
    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "key", "key": "Shift_L"}}),
    );
    let sc = &r["result"]["structuredContent"];
    if sc["error"]["code"] == "FOCUS_MISMATCH" {
        eprintln!("focus not on eDP-1; skipping assertions");
        return;
    }
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    assert_eq!(sc["outcome"], "partial", "{r}");
    assert_eq!(sc["display_changed_during_action"], json!(true), "{r}");
    assert_eq!(sc["do_not_replay"], json!(true), "{r}");
}

/// Clipboard replaced but paste aborted before any key event: effect must
/// report a side effect (partial), never `none` (review P1).
#[test]
fn clipboard_replaced_then_aborted_paste_reports_partial() {
    let rig = event_rig();
    // Simulate a successful clipboard replacement into a marker file; the
    // user's real clipboard is untouched.
    script(
        &rig,
        "wl-copy",
        &format!(
            "#!/usr/bin/python3\nimport pathlib,sys,time\np=pathlib.Path(\"{d}\")\n(p/\"clipboard\").write_bytes(sys.stdin.buffer.read())\n(p/\"trigger\").touch()\ntime.sleep(0.25)\n",
            d = rig.dir.display()
        ),
    );
    let Some(mut c) = Client::start_with(&rig_env(&rig)) else {
        return;
    };
    init(&mut c);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let obs = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": "regression-clip"}}),
    );
    let sc = &r["result"]["structuredContent"];
    if sc["error"]["code"] == "FOCUS_MISMATCH" {
        eprintln!("focus not on eDP-1; skipping assertions");
        return;
    }
    assert_eq!(
        std::fs::read_to_string(rig.dir.join("clipboard")).unwrap(),
        "regression-clip"
    );
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    assert_eq!(sc["effect"], "partial", "{r}");
    assert_eq!(sc["do_not_replay"], json!(true), "{r}");
}

/// A wedged input connection must be terminated by the reply timeout —
/// response returns, no hang, no overlap with a following request
/// (review P1). The silent socket never answers the Wayland roundtrip.
#[test]
fn wedged_input_connection_times_out_and_recovers() {
    let rig = event_rig();
    let sock = rig.dir.join("input.sock");
    let silent = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new(); // keep peers alive so connects stay wedged
        while let Ok((peer, _)) = silent.accept() {
            held.push(peer);
        }
    });
    let mut env = rig_env(&rig);
    env.push((
        "COMPUTER_USE_INPUT_SOCKET".into(),
        sock.display().to_string(),
    ));
    let Some(mut c) = Client::start_with(&env) else {
        return;
    };
    init(&mut c);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let obs = c.call_tool("computer_observe", json!({"monitor": "eDP-1"}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let start = std::time::Instant::now();
    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    // The worker's wedged connect is interrupted by timeout + socket
    // shutdown; the reply must arrive, not hang. Allow generous headroom.
    assert!(start.elapsed() < std::time::Duration::from_secs(20), "{r}");
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    assert_eq!(sc["error"]["code"], "INPUT_BACKEND_UNAVAILABLE", "{r}");

    // The server is still responsive afterwards: the action lock was held
    // through teardown, so no delayed input can overlap later requests.
    let mon = c.call_tool("computer_monitors", json!({}));
    assert_ne!(mon["result"]["isError"], json!(true), "{mon}");
}
