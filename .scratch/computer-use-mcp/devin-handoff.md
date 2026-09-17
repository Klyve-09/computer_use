# Devin Handoff — Rust Computer Use MCP

Status: APPROVED HANDOFF. Product requirements, technical decisions, verification seams, and ticket breakdown were approved by the user on 2026-09-17 and published to GitHub. The user submits this handoff to Devin manually unless they explicitly authorize submission.

## Objective

Build a Rust stdio MCP that gives the user's current Linux Codex screenshot-based control of the active Hyprland desktop, including mixed-scale multi-monitor use and cross-monitor drag. Codex performs the planning; the server provides observation and one-action execution.

## Authoritative inputs

- [Specification](spec.md): scope, public tool behavior, coordinate/configuration contract, error semantics, and verification requirements.
- [Ticket index](issues/README.md): five vertical slices and blocking edges.
- [Design verification](verification.md): observed results, corrections, and explicit limits.
- [Backend research](backend-research.md) and [MCP research](mcp-research.md): primary sources and alternatives. The specification prevails where it selects a different option from a research note.
- [Domain glossary](../../CONTEXT.md): use its Monitor, Observation, Action, Desktop, Display Configuration, and Cross-monitor Drag terminology.

Repository: `Klyve-09/computer_use`. Local project: `/home/hyh/computer_use`. Authoritative tracker: GitHub Issues, accessed with `gh`.

Published specification: [Issue #1](https://github.com/Klyve-09/computer_use/issues/1).

## Required implementation workflow

Read and follow both skills before editing product code:

1. `/home/hyh/.agents/skills/implement/SKILL.md`
2. `/home/hyh/.agents/skills/ponytail/SKILL.md` — full mode.

Also read the repository's `AGENTS.md` and tracker/domain conventions. The specified absolute skill paths belong to the user's environment; if a remote implementing environment cannot access them, obtain those exact skill documents before implementation rather than silently skipping them.

Devin owns product code, tests, and deployment/setup code. Astra owns design, disposable prototypes, and review. Work an unblocked ticket in a fresh implementation context; follow the implementation skill's testing and review workflow. Keep all newly authored implementation-facing documents, verification notes, ticket updates, and handoff material in English.

## Scope and decisions

- Personal local Hyprland/Wayland use, with the current Codex as the first supported client. Multiple Monitors and different scales are mandatory, not a later extension.
- Three tools: read-only Monitor discovery, read-only Observation, and one-action mutation returning a post-action screenshot.
- Pointer actions use coordinates from an opaque Observation ID, not model-calculated desktop coordinates. Keep transform math on the server.
- Support click/right/double click, horizontal/vertical scroll, shortcuts/keys, Korean/multiline clipboard text, same-monitor drag, and Cross-monitor Drag.
- Use the official Rust MCP SDK; existing `grim`, `hyprctl`, and `wl-copy`; a persistent Wayland virtual pointer for the complete pointer feature set; verified keyboard press/release handling.
- Support rotated/portrait/flipped layouts, negative positions, fractional mixed scale, and connection/layout/scale changes during a session.
- Session/generation plus fresh configuration checks invalidate old observations. Validate event coverage; change-and-restore cannot be guaranteed from polling alone.
- Serialize input; release held inputs on interruption as far as possible. Distinguish input effects from image capture success and explicitly forbid blind replay after partial or uncertain effects.
- Clipboard replacement is accepted; arbitrary clipboard restoration, DOM/OCR, other compositor/OS backends, remote service, and an autonomous planner are outside scope.

## Implementation order

| Local ticket | Deliverable | Direct blockers | Published issue |
|---|---|---|---|
| [01](https://github.com/Klyve-09/computer_use/issues/2) | Codex sees selected Monitor screenshots | None | #2 `ready-for-agent` |
| [02](https://github.com/Klyve-09/computer_use/issues/3) | Mixed-scale click and scroll | #2 | #3 `ready-for-agent` |
| [03](https://github.com/Klyve-09/computer_use/issues/4) | Browser address workflow and Korean/multiline input | #3 | #4 `ready-for-agent` |
| [04](https://github.com/Klyve-09/computer_use/issues/5) | Same/cross-monitor drag | #3 | #5 `ready-for-agent` |
| [05](https://github.com/Klyve-09/computer_use/issues/6) | Display-change/interruption recovery and operational completion | #4, #5 | #6 `ready-for-agent` |

Tickets 03 and 04 have no mutual dependency. Each slice carries its own validation; do not postpone ordinary correctness checks to 05.

## Prototype evidence and boundaries

Local branch: `prototype/computer-use-coordinates`.

Commit: `5c853b613b9bebbdb4743ad4cf4bb4471561dc14`.

Worktree: `/home/hyh/computer_use-prototypes/coordinates`.

Open its `coordinate-prototype.html` directly in a browser to explore mixed scales, portrait mapping, change-and-restore, and cross-monitor drag endpoints. Its README records reproducible calculations and the C virtual-pointer probe. The HTML/C artifacts are disposable primary evidence and must not be promoted into product code.

On the actual laptop, capture dimensions and a no-op pointer move were verified. Real application input and physical multi-monitor operations remain unverified. Rust is not installed in the current PATH. Do not translate this limited evidence into a claim of a functioning MCP.

## Acceptance and verification

Use the complete matrix in the specification. At minimum:

1. Build and exercise the public stdio protocol, including schemas and all failure variants; verify actual current Codex sees the image and result metadata.
2. Run the browser address-entry/navigation/observation scenario through Codex's actual MCP calls.
3. Verify native Wayland and XWayland input plus Korean/multiline text and terminal paste selection.
4. Verify mixed scales/rotation/negative layouts/edges and cross-monitor drag in a compositor environment. Separate synthetic arithmetic, virtual-output, and physical-device evidence.
5. Exercise runtime display changes, change-and-restore, event connection loss, cancellation, held-input cleanup, and successful input followed by failed capture. Report race limits truthfully.
6. Document startup, dependencies, exact Codex registration, clipboard effects, focus behavior, and recovery steps. Preserve existing user configuration.

Return exact check commands, outcomes, outstanding findings, and tracker links. If a required backend capability fails, fix or document the concrete blocker; do not silently reduce the approved multi-monitor scope.

## Publication record

The specification and tickets were published with `ready-for-agent`. Native blocking relationships are attached: #3 is blocked by #2; #4 and #5 are blocked by #3; #6 is blocked by #4 and #5. The prototype source is published on branch [`prototype/computer-use-coordinates`](https://github.com/Klyve-09/computer_use/tree/prototype/computer-use-coordinates) at commit `5c853b613b9bebbdb4743ad4cf4bb4471561dc14`. The parent spec issue remains open.

## Copyable prompt after approval

Read `/home/hyh/computer_use/.scratch/computer-use-mcp/devin-handoff.md` and its approved specification and tickets. Follow the required implement and ponytail skills, then implement the unblocked tickets with their acceptance checks. Report actual Codex/GUI evidence and verification limits. Start with [Issue #2](https://github.com/Klyve-09/computer_use/issues/2); do not claim the product works from the disposable prototype alone.
