# Verification notes — review-fix changes (2026-09-17)

Scope: fixes for the three P1 findings in `review.md` (diff on top of `a8a5d2d`). These restore already-approved requirements; no new scope.

## Changes

1. **Post-action capture bracketing** (`src/main.rs`): the post-action observation now uses the same before/after snapshot bracket as `computer_observe` — fresh `monitors()` snapshot, generation+fingerprint compare around `grim`, one retry. An image captured across a configuration change is never recorded; a stable observation under the new configuration is still returned inside the `partial` result. `display_changed_during_action` is computed against the verified post-capture snapshot, not a pre-capture one.
2. **Clipboard effect classification** (`src/main.rs type_text`): after `clipboard_set` succeeds, keyboard `Delivery::None`/resolution errors map to `Partial`. `wl-copy` spawn-missing stays `None`; any other wl-copy failure (nonzero exit, timeout, stdin error) maps to `Unknown` since the daemon may already have taken the text.
3. **Timeout/cancellation governs execution** (`src/backend.rs`, `src/main.rs`): the `Guard` tuple became `Abort` — generation check + cancel flag + a `kick` fd (a `try_clone` of the worker's Wayland socket, published before the first connect roundtrip and refreshed per request). On reply timeout or MCP `notifications/cancelled` (wired via `RequestContext.ct`), `cancel()` sets the flag and `shutdown()`s the socket, which wakes wedged blocking roundtrips, and the caller then holds for the worker's teardown acknowledgment while still holding `action_lock`. Workers check `stopped()` before connect, between ops, and inside 20ms `Wait` chunks; a cancelled request drops the session (`Drop` releases held buttons/keys/modifiers).

## Commands run and results

- `cargo fmt --check` — clean.
- `cargo test` — 13 unit + 9 stdio integration tests, all pass (~10.7s; the wedged-input test accounts for ~10s of real timeout).
- `python3 .scratch/computer-use-mcp/review-evidence/reproduce.py` — both defects no longer reproduce:
  - `clipboard replaced then generation invalidated` → `outcome: partial`, `effect: partial`, `isError: true`, `do_not_replay` (previously `effect: none`, rejected-looking).
  - `config event during post-action capture` → `outcome: partial`, `effect: completed`, `display_changed_during_action: true`, no raced observation recorded (previously `outcome: ok` + actionable observation at the new generation).

## New stdio-boundary regression tests (`tests/mcp_stdio.rs`)

- `config_change_during_post_action_capture_is_partial`: PATH-shadowed `grim` wrapper trips the controlled event socket mid-post-action capture → asserts `partial` + `do_not_replay` + `display_changed_during_action`.
- `clipboard_replaced_then_aborted_paste_reports_partial`: fake `wl-copy` consumes text into a marker file (user clipboard untouched), then a `configreloaded` aborts the paste → asserts `effect: partial`.
- `wedged_input_connection_times_out_and_recovers`: `COMPUTER_USE_INPUT_SOCKET` points the pointer worker at a silent Unix socket; the connect roundtrip wedges → 10s timeout + socket shutdown → response arrives (`INPUT_BACKEND_UNAVAILABLE`), and `computer_monitors` still answers afterwards (no server hang).

## Evidence classification

- **Simulated/subprocess fixtures**: wl-copy marker file, grim wrapper, controlled event socket, silent input socket — deterministic but not claims about real wl-clipboard/grim behavior.
- **Real/virtual-device evidence**: the tests run the real stdio server, real `hyprctl`, real `grim` output, and the real Hyprland session on this machine (eDP-1 2560×1440 @1.25). The clipboard test's paste abort went through the real virtual-keyboard worker abort path. The wedged test exercised real `UnixStream::connect` + `shutdown` wakeup of a real wayland-client roundtrip.
- **Not verified here**: a physically wedged compositor, held-input release during a real mid-drag cancel on a physical device, MCP `notifications/cancelled` from an actual Codex client (wiring is `RequestContext.ct` → `abort.cancel()`; code-reviewed but not end-to-end exercised), physical multi-monitor.

## Limits

- After timeout the caller waits **unboundedly** for the teardown acknowledgment. All realistic wedge points (connect/apply roundtrips, `Wait` sleeps) are interruptible via the kick socket or chunked checks; a worker wedged outside those (e.g. inside xkb compile) would block the action path forever — chosen deliberately over any window where delayed input could overlap a later action.
- Orderly process shutdown does not explicitly release held input; worker threads live for the process lifetime and compositor teardown of the virtual devices on connection close is relied upon.
- `Abort.flag` was renamed `cancelled`; a generation bump between observation validation and `Abort` creation is now reported as `STALE_OBSERVATION` rather than `INPUT_BACKEND_UNAVAILABLE`.
