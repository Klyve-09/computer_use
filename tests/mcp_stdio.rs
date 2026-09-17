use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

/// Drives the real server over stdio JSON-RPC, the public seam Codex uses.
/// Requires a live Hyprland session (hyprctl/grim); skipped otherwise.
struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: std::sync::mpsc::Receiver<Value>,
    /// Responses for ids other than the awaited one, in arrival order.
    pending: Vec<Value>,
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
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            while stdout.read_line(&mut line).unwrap_or(0) > 0 {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if tx.send(v).is_err() {
                        return;
                    }
                }
                line.clear();
            }
        });
        Some(Self {
            child,
            stdin,
            rx,
            pending: Vec::new(),
            next_id: 0,
        })
    }

    /// Next response for `id`, buffering anything else; None on timeout.
    fn await_id(&mut self, id: u64, timeout: std::time::Duration) -> Option<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(i) = self
                .pending
                .iter()
                .position(|v| v.get("id").and_then(Value::as_u64) == Some(id))
            {
                return Some(self.pending.remove(i));
            }
            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            match self.rx.recv_timeout(remaining) {
                Ok(v) => self.pending.push(v),
                Err(_) => return None,
            }
        }
    }

    /// Send a request and return its id without waiting for the response.
    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.next_id
    }

    fn send_tool(&mut self, name: &str, arguments: Value) -> u64 {
        self.send("tools/call", json!({"name": name, "arguments": arguments}))
    }

    /// MCP notifications/cancelled for an in-flight or queued request.
    fn cancel(&mut self, id: u64) {
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": "test cancellation"}});
        writeln!(self.stdin, "{msg}").unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.await_id(id, std::time::Duration::from_secs(30))
            .expect("timed out waiting for response")
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
    let rig = event_rig();
    // Deterministic focus precondition: the fixture reports focus on the
    // test monitor so the assertions below are never skipped by
    // FOCUS_MISMATCH.
    let (mon, mon_id) = first_monitor();
    focus_fixture(&rig, mon_id);
    let Some(mut c) = Client::start_with(&rig_env(&rig)) else {
        return;
    };
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

    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Unknown modifier name -> rejected before any key event.
    let bad_mod = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "key", "key": "a", "mods": ["banana"]}}),
    );
    assert_eq!(
        bad_mod["result"]["structuredContent"]["error"]["code"], "INVALID_MODIFIER",
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
        assert_eq!(
            bad_paste["result"]["structuredContent"]["error"]["code"], "INVALID_PASTE",
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
    assert_eq!(
        bad_key["result"]["structuredContent"]["error"]["code"], "INVALID_KEY",
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

/// Absolute path of a binary on the real PATH (skipping rig dirs).
fn real_bin(name: &str) -> std::path::PathBuf {
    let rigs = std::env::temp_dir();
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap()) {
        let p = dir.join(name);
        if p.is_file() && !p.starts_with(&rigs) {
            return p;
        }
    }
    panic!("{name} not found on PATH");
}

/// First selectable monitor's (name, compositor id) from the real session.
fn first_monitor() -> (String, i64) {
    let out = Command::new(real_bin("hyprctl"))
        .args(["-j", "monitors"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let m = v
        .as_array()
        .unwrap()
        .iter()
        .find(|m| !m["disabled"].as_bool().unwrap_or(false))
        .expect("no selectable monitor in this session");
    (
        m["name"].as_str().unwrap().to_string(),
        m["id"].as_i64().unwrap(),
    )
}

/// PATH-shadowed hyprctl that reports the focused window on `monitor_id`
/// and delegates everything else to the real binary. Key/type actions
/// require focus on the observed monitor; this fixture makes that
/// precondition deterministic so regressions always run their assertions
/// instead of silently passing on FOCUS_MISMATCH.
fn focus_fixture(rig: &Rig, monitor_id: i64) {
    let real = real_bin("hyprctl");
    script(
        rig,
        "hyprctl",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"-j\" ] && [ \"$2\" = \"activewindow\" ]; then printf '{{\"monitor\": {monitor_id}}}\\n'; else exec {} \"$@\"; fi\n",
            real.display()
        ),
    );
}

/// Unix listener that accepts peers, holds them open without ever
/// replying, and reports each accept on the returned channel — the
/// "wedged input backend" fixture.
fn silent_listener(sock: &std::path::Path) -> std::sync::mpsc::Receiver<()> {
    let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new(); // keep peers alive so connects stay wedged
        while let Ok((peer, _)) = listener.accept() {
            let _ = tx.send(());
            held.push(peer);
        }
    });
    rx
}

/// A config event arriving mid-post-action-capture must never produce an
/// `outcome: ok` result with a fresh actionable observation (review P1).
#[test]
fn config_change_during_post_action_capture_is_partial() {
    let rig = event_rig();
    // Focus precondition is made deterministic (fixture reports focus on the
    // test monitor); without it this test must fail loudly, not pass.
    let (mon, mon_id) = first_monitor();
    focus_fixture(&rig, mon_id);
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
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
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
    let (mon, mon_id) = first_monitor();
    focus_fixture(&rig, mon_id);
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
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": "regression-clip"}}),
    );
    let sc = &r["result"]["structuredContent"];
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
    // The accept channel proves the worker reached the wedged-roundtrip
    // stage, so the assertions below cannot pass on an unrelated setup
    // failure before the intended timeout/cancellation path.
    let accepted_rx = silent_listener(&sock);
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
    let (mon, _) = first_monitor();
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let start = std::time::Instant::now();
    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    // The worker's wedged roundtrip is interrupted by the reply timeout +
    // socket shutdown; the reply must arrive, not hang. Generous headroom.
    assert!(start.elapsed() < std::time::Duration::from_secs(20), "{r}");
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    // The socket accepted the connection, so the worker wedged in the
    // Wayland roundtrip and the reply-timeout cancel is the only path that
    // frees it: CANCELLED, never an unrelated backend setup failure.
    assert_eq!(sc["error"]["code"], "CANCELLED", "{r}");
    assert_eq!(sc["effect"], "none", "{r}");
    accepted_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("input session never connected to the silent socket");

    // The server is still responsive afterwards: the action lock was held
    // through teardown, so no delayed input can overlap later requests.
    let mon = c.call_tool("computer_monitors", json!({}));
    assert_ne!(mon["result"]["isError"], json!(true), "{mon}");
}

/// A request cancelled while queued behind a wedged action must run no side
/// effects at all once admitted — no clipboard write, no paste (review P1).
#[test]
fn cancelled_queued_action_runs_no_side_effects() {
    let rig = event_rig();
    let (mon, mon_id) = first_monitor();
    focus_fixture(&rig, mon_id);
    // If the cancelled request executes, this marker file appears. The real
    // clipboard is untouched.
    script(
        &rig,
        "wl-copy",
        &format!(
            "#!/usr/bin/python3\nimport pathlib,sys\npathlib.Path(\"{d}/clipboard-marker\").write_bytes(sys.stdin.buffer.read())\n",
            d = rig.dir.display()
        ),
    );
    // Silent input socket: accepts, never answers, so the first action holds
    // the action lock stuck in a Wayland roundtrip.
    let sock = rig.dir.join("input.sock");
    let accepted_rx = silent_listener(&sock);
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
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The wedged action holds the serialized action lock mid-roundtrip.
    let wedged = c.send_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    accepted_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("input session never connected");

    // The queued action waits behind it, then is cancelled before it can run.
    let queued = c.send_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": "CANCELLED_REQUEST_EXECUTED"}}),
    );
    std::thread::sleep(std::time::Duration::from_millis(150));
    c.cancel(queued);
    std::thread::sleep(std::time::Duration::from_millis(50));
    c.cancel(wedged);

    // rmcp drops the response for a cancelled request, so neither produces
    // one. A follow-up request proves the cancelled wedged action released
    // the action lock instead of hanging the worker.
    let done = c.call_tool(
        "computer_action",
        json!({"observation_id": "invalid", "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    assert_eq!(
        done["result"]["structuredContent"]["error"]["code"], "STALE_OBSERVATION",
        "{done}"
    );
    // The regression assertion: once the wedged action was interrupted and
    // the queued slot ran, the cancelled request must have produced no
    // side effect.
    assert!(
        !rig.dir.join("clipboard-marker").exists(),
        "cancelled queued request replaced the clipboard"
    );
}

/// A listener with a full accept backlog wedges `UnixStream::connect`
/// before any kick fd exists; connection setup must still meet a deadline
/// and a following request must not be stuck behind it (review P1).
#[test]
fn full_input_backlog_connect_is_deadline_bound() {
    let rig = event_rig();
    let sock = rig.dir.join("input.sock");
    // listen(0): one queued connection fills the backlog, so the server's
    // blocking connect() parks in the kernel before any session exists.
    let listener =
        socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
    listener
        .bind(&socket2::SockAddr::unix(&sock).unwrap())
        .unwrap();
    listener.listen(0).unwrap();
    let _filler = std::os::unix::net::UnixStream::connect(&sock).unwrap();
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
    let (mon, _) = first_monitor();
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let start = std::time::Instant::now();
    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    // Connect is bounded (~5s deadline) — the wedged worker must not hold
    // the action lock past the reply timeout.
    assert!(
        start.elapsed() < std::time::Duration::from_secs(9),
        "wedged connect did not meet its deadline: {r}"
    );
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    assert_eq!(sc["error"]["code"], "INPUT_BACKEND_UNAVAILABLE", "{r}");
    assert_eq!(sc["effect"], "none", "{r}");

    // A following request answers promptly: no delayed input is in flight.
    let start = std::time::Instant::now();
    let bad = c.call_tool(
        "computer_action",
        json!({"observation_id": "invalid", "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "following request stuck behind the wedged connect: {bad}"
    );
    assert_eq!(
        bad["result"]["structuredContent"]["error"]["code"], "STALE_OBSERVATION",
        "{bad}"
    );
}

/// A wl-copy that reads the text then delays its side effect past the
/// clipboard deadline must be killed before the server returns: its late
/// marker can never appear after the action has reported (review P1).
/// Controlled child; the real clipboard is untouched.
#[test]
fn clipboard_timeout_kills_delayed_side_effect_child() {
    let rig = event_rig();
    let (mon, mon_id) = first_monitor();
    focus_fixture(&rig, mon_id);
    // Reads all of stdin (so the write completes), then sleeps 7s and
    // writes a marker — a side effect landing after the 5s deadline.
    script(
        &rig,
        "wl-copy",
        &format!(
            "#!/usr/bin/python3\nimport pathlib,sys,time\nsys.stdin.buffer.read()\ntime.sleep(7)\npathlib.Path(\"{d}/late-clipboard-marker\").write_text(\"late\")\n",
            d = rig.dir.display()
        ),
    );
    let Some(mut c) = Client::start_with(&rig_env(&rig)) else {
        return;
    };
    init(&mut c);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();

    let start = std::time::Instant::now();
    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": "late clipboard"}}),
    );
    // One deadline bounds the whole write+wait (~5s), not the child's own
    // schedule.
    assert!(
        start.elapsed() < std::time::Duration::from_secs(9),
        "clipboard write+wait escaped its deadline: {r}"
    );
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    // wl-copy ran and may have replaced the clipboard: honest `unknown`,
    // never `none`.
    assert_eq!(sc["effect"], "unknown", "{r}");
    assert_eq!(sc["do_not_replay"], json!(true), "{r}");

    // The child was killed and reaped: wait past its 7s marker schedule
    // and confirm the delayed side effect never lands.
    let remaining = std::time::Duration::from_secs(8).saturating_sub(start.elapsed());
    std::thread::sleep(remaining);
    assert!(
        !rig.dir.join("late-clipboard-marker").exists(),
        "killed wl-copy performed a clipboard side effect after return"
    );
}

/// A wl-copy that never reads stdin must not hold the request or the
/// serialized action lock past the clipboard deadline, and a mid-write
/// cancellation must release the action path too (review P1). Controlled
/// child; the real clipboard is untouched.
#[test]
fn clipboard_blocked_stdin_is_bounded_and_cancellable() {
    let rig = event_rig();
    let (mon, mon_id) = first_monitor();
    focus_fixture(&rig, mon_id);
    // Never reads stdin: a payload larger than the 64KiB pipe buffer
    // blocks write_all indefinitely without the fix. Records its pid so
    // the test can verify the cancelled child was killed and reaped.
    script(
        &rig,
        "wl-copy",
        &format!(
            "#!/usr/bin/python3\nimport os,pathlib,time\npathlib.Path(\"{d}/child-pid\").write_text(str(os.getpid()))\ntime.sleep(30)\n",
            d = rig.dir.display()
        ),
    );
    let Some(mut c) = Client::start_with(&rig_env(&rig)) else {
        return;
    };
    init(&mut c);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let big = "x".repeat(262144);

    // Oversized payloads are rejected before wl-copy ever spawns.
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let too_big = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": "x".repeat(1024 * 1024 + 1)}}),
    );
    assert_eq!(
        too_big["result"]["structuredContent"]["error"]["code"], "PAYLOAD_TOO_LARGE",
        "{too_big}"
    );
    assert_eq!(
        too_big["result"]["structuredContent"]["effect"], "none",
        "{too_big}"
    );
    assert!(
        !rig.dir.join("child-pid").exists(),
        "oversized type_text spawned wl-copy"
    );

    // Phase 1: the deadline bounds the blocked write itself.
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let start = std::time::Instant::now();
    let r = c.call_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": big}}),
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(9),
        "blocked stdin write escaped its deadline: {r}"
    );
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], json!(true), "{r}");
    assert_eq!(sc["effect"], "unknown", "{r}");
    // The timed-out child was killed and reaped, so the lock is free.
    let start = std::time::Instant::now();
    let done = c.call_tool(
        "computer_action",
        json!({"observation_id": "invalid", "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "following action stuck behind the timed-out clipboard child: {done}"
    );
    assert_eq!(
        done["result"]["structuredContent"]["error"]["code"], "STALE_OBSERVATION",
        "{done}"
    );

    // Phase 2: a request cancelled while its stdin write is blocked must
    // also release the action path promptly.
    let obs = c.call_tool("computer_observe", json!({"monitor": mon}));
    let oid = obs["result"]["structuredContent"]["observation_id"]
        .as_str()
        .unwrap()
        .to_string();
    let wedged = c.send_tool(
        "computer_action",
        json!({"observation_id": oid, "action": {"kind": "type_text", "text": big}}),
    );
    // Let the action reach the blocked write, then cancel it.
    std::thread::sleep(std::time::Duration::from_secs(1));
    c.cancel(wedged);
    // rmcp drops the cancelled request's response; a follow-up request
    // proves the cancel killed the child and released the action lock
    // well before the 5s deadline.
    let start = std::time::Instant::now();
    let done = c.call_tool(
        "computer_action",
        json!({"observation_id": "invalid", "action": {"kind": "click", "x": 10, "y": 10, "button": "left"}}),
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(4),
        "following action stuck behind a cancelled blocked clipboard write: {done}"
    );
    assert_eq!(
        done["result"]["structuredContent"]["error"]["code"], "STALE_OBSERVATION",
        "{done}"
    );
    // The cancelled child was killed and reaped before the lock released:
    // a zombie or live wl-copy would still show up in /proc.
    let pid: u32 = std::fs::read_to_string(rig.dir.join("child-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "cancelled wl-copy (pid {pid}) was not killed and reaped"
    );
}
