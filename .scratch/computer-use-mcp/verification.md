# Design Verification — Computer Use MCP

Date: 2026-09-17. This is a design review and prototype evidence record, not product acceptance.

## Confirmed facts

| Check | Evidence | What it establishes |
|---|---|---|
| Repository/tracker | Read tracked files and GitHub issue/label lists with `gh`; no open issues; required triage labels present. | New product implementation, GitHub publication path available. |
| Current desktop | Hyprland 0.56.2 on Wayland; one 2560x1440 output, scale 1.25, origin (0,0). | Current test environment only; no single-monitor scope restriction. |
| Actual capture | `grim` default produced 2560x1440; scale 1 produced 2048x1152 PNG. | Returned image dimensions must participate in mapping. Captures were not retained. |
| Pointer capability | Disposable C registry probe found virtual-pointer version 2. | Required protocol is advertised by this compositor. |
| Pointer motion | No-op absolute move with extent 2048x1152 kept measured cursor at (17,566). | Narrow live single-monitor coordinate smoke evidence; not real click/scroll/drag proof. |
| Coordinate model | Node exercised center points on laptop, negative-origin monitor, and portrait monitor; outputs recorded in the prototype README. | Arithmetic and separate per-monitor endpoint conversion behave as intended for these cases. |
| Invalidation model | Change-and-restore advanced generation; old frame rejected. Invalid image boundary rejected. | Geometry equality alone is insufficient; generation-based state model works in simulation. |
| Prototype UI | Headless Chrome rendered the self-contained HTML; preview was visually inspected. | The guided/free-play artifact is readable and renders. |
| Codex transport | Installed CLI 0.153.3 exposes `mcp add NAME -- COMMAND`. | Local stdio registration is available, not yet an MCP tool/image roundtrip. |
| Upstream backend | Pinned Hyprland 0.56.2 pointer source maps unmapped absolute input to the full logical-layout bounding rectangle; grim handles output transform. | The proposed mapping is source-grounded for this version; live multi-output acceptance is still required. |

## Review corrections incorporated into the spec

- Scrolling is mandatory, so use a complete persistent virtual-pointer path rather than claiming keyboard/CLI dispatchers alone satisfy the pointer requirement.
- Reject invalid coordinates; do not clamp them to a different target.
- A geometry fingerprint must be combined with a session/generation. Require evidence of notification coverage for supported change mechanisms; snapshots cannot detect every unobserved change-and-restore.
- Distinguish input effect (`none`, `completed`, `partial`, `unknown`) from observation success. A failed paste after clipboard replacement is not side-effect free.
- Choose three tools to keep discovery/observation read-only and mutation explicit. This supersedes the two-tool alternative in the research note.
- Use `isError: true` for execution failures, including post-action capture failure, with explicit no-replay guidance and input effect. MCP does not define a retry guarantee. This supersedes the research note's alternative of marking partial results successful.
- Any declared output schema must cover all success and failure variants.
- Keyboard dispatcher behavior must be checked in actual native Wayland/XWayland applications; reading a press/release API is not delivery evidence.

## Remaining implementation acceptance

The following have **not** been proven: a Rust build, actual Codex MCP initialization/image response, click/scroll/drag application delivery, Korean or multiline paste, runtime hotplug delivery, physical mixed-scale multi-monitor behavior, rotated live input, or cancellation cleanup. The spec and tickets explicitly require these checks or clearly labeled virtual-output evidence where physical hardware is unavailable.

No production code, Rust toolchain installation, Codex configuration edit, or GitHub issue publication occurred during this design phase. The prototype is preserved on a separate local branch; the current main worktree contains documentation drafts.

## Approval and publication

- Approved: product requirements and progression to technical validation/spec/tickets/manual handoff preparation, in the user's “넘어가” message.
- Completed: the user approved the concrete specification's test seams and five-ticket breakdown.
- Completed: [specification issue #1](https://github.com/Klyve-09/computer_use/issues/1) and [tickets #2–#6](https://github.com/Klyve-09/computer_use/issues/2) through [#6](https://github.com/Klyve-09/computer_use/issues/6) were published with `ready-for-agent`; native blocking relationships were verified; the prototype branch was pushed; and the handoff was updated. Devin submission remains manual unless explicitly requested.

## Primary evidence

- [Specification](spec.md)
- [Backend research with sources](backend-research.md)
- [MCP research with sources](mcp-research.md)
- Prototype branch `prototype/computer-use-coordinates`, commit `5c853b613b9bebbdb4743ad4cf4bb4471561dc14`; worktree `/home/hyh/computer_use-prototypes/coordinates`.

## Implementation verification log (tickets #2–#6)

Environment: Hyprland 0.56.2, eDP-1 2560x1440 @ scale 1.25, fcitx5 present, Rust stable via rustup, Codex CLI 0.153.3, server `target/release/computer-use-mcp`.

### #6 runtime recovery

Commands were driven over the real stdio MCP boundary (python JSON-RPC harness) and real `codex exec` sessions.

- **Live config change / change-and-restore**: `hyprctl eval 'hl.monitor({output="eDP-1", ..., scale=1.0})'` then restore to 1.25. `wl_output` listener bumped the generation on both halves; pre-change observation IDs reject `STALE_OBSERVATION`; fresh `computer_observe` restores usability. Also verified under `hyprctl reload` (generation bump with identical fingerprint).
- **Mid-drag abort**: drag eDP-1 → HEADLESS-4 with a scale flip ~300 ms in returned `outcome: partial`, `effect: partial`, `display_changed_during_action: true`, `do_not_replay: true`, plus a fresh destination observation. The generation guard stops remaining interpolated moves and releases the held button before returning.
- **Event socket loss/reconnect**: `COMPUTER_USE_EVENT_SOCKET=/tmp/fake-ev.sock` pointed the watcher at a controlled Unix socket; closing the peer produced EOF → generation bump (12→13) → `STALE_OBSERVATION` on the old ID → automatic reconnect within the 1 s backoff → fresh observation actionable, click `completed`.
- **Server restart**: new process epoch invalidates every pre-restart ID (`unknown or expired`).
- **Codex recover-and-continue** (`codex exec`, model gpt-6-astra, danger-full-access): observe eDP-1 → `sleep 15` (scale flipped externally during the sleep) → click on the stale ID rejected `STALE_OBSERVATION` → re-observe → same click `outcome: ok`, `effect: completed`. 53,396 tokens.
- **Serialization**: `action_lock` serializes all actions; pointer/keyboard sessions are single-threaded behind command channels.
- **Cancellation/timeout/shutdown**: every op batch ends with explicit releases; transport failure triggers best-effort release of held buttons/keys/mods; delivery waits are bounded at 10 s. Forced termination (`SIGKILL`) and compositor failure cannot guarantee cleanup — documented in README "Recovery and operator notes".
- **No replay after failed capture**: post-action capture failure returns `partial` + `SCREENSHOT_FAILED_AFTER_ACTION`/`OBSERVATION_FAILED_AFTER_INPUT` + `do_not_replay`; nothing auto-retries.

Evidence classes: physical panel (eDP-1) for observe/click/scroll/key/paste/mid-drag-abort/socket-loss/restart; virtual headless outputs (HEADLESS-*) for cross-output drag and mixed-scale mapping; unit/stdio tests for boundary rejections. No arithmetic-only substitutes were counted as input evidence.

Automated: `cargo test` → 13 unit + 6 stdio tests, all green.
