# 05: Recover from display changes and interrupted actions

Status: Implemented and verified on 2026-09-17.

## Parent

[Approved specification](https://github.com/Klyve-09/computer_use/issues/1).

## What to build

Complete the everyday workflow in which the user connects or disconnects Monitors, changes scales/layouts, or interrupts Codex while it is interacting with an application. Codex must receive truthful state, refresh its observations, and continue without stale targeting or automatic duplicated input.

## Acceptance criteria

- [ ] Exercise live configuration change, change-and-restore, event socket loss/reconnect, and server restart through the public tools. All pre-change frames are rejected for subsequent mutation and new observations restore usability.
- [ ] When configuration changes during a drag or shortcut, stop further intended input, prioritize releases, report partial/unknown delivery where appropriate, and require fresh observations. Do not claim an atomic input/configuration guarantee.
- [ ] Verify cancellation, timeouts, and orderly shutdown attempt to release held buttons/modifiers; document that forced termination/compositor failure cannot guarantee cleanup.
- [ ] Verify request serialization prevents keyboard/pointer state interleaving and failed capture after completed input never triggers automatic replay.
- [ ] Demonstrate a complete recover-and-continue sequence in actual Codex: act, encounter a stale/changed configuration, observe again, and complete a harmless task.
- [ ] Provide one-command startup and exact Codex registration instructions, dependency/version diagnostics, clipboard/focus semantics, and an operator recovery procedure. Respect existing user configuration; avoid replacing it wholesale.
- [ ] Run the complete acceptance matrix from the specification, combining MCP boundary checks, controlled failure/layout cases, and real GUI observations. Report simulated, nested/headless, and physical hardware evidence separately.
- [ ] Deliver verification notes with exact commands/results and any remaining physical-hardware limitations. A passing arithmetic simulation alone cannot close the multi-output input requirement.

## Blocked by

- https://github.com/Klyve-09/computer_use/issues/4
- https://github.com/Klyve-09/computer_use/issues/5

## Implementation boundaries

Extend the existing action/configuration path; do not introduce a new service, remote transport, arbitrary command tool, or generalized recovery framework. This ticket completes behavior and operational documentation, not a separate horizontal testing phase.
