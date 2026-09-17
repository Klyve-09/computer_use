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
