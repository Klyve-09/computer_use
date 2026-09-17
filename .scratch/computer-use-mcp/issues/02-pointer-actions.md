# 02: Click and scroll on mixed-scale Monitors

Status: Approved on 2026-09-17; ready-for-agent.

## Parent

[Approved specification](https://github.com/Klyve-09/computer_use/issues/1).

## What to build

Allow Codex to click and scroll using coordinates from a selected Monitor's Observation, then receive a new screenshot. This slice must work across the user's multi-Monitor layouts rather than hard-code the current laptop screen.

## Acceptance criteria

- [ ] Add `computer_action` with one click or scroll action per call. Support left/right, single/double click and signed horizontal/vertical wheel steps at an observed point.
- [ ] Resolve coordinates from the referenced image dimensions into oriented logical Monitor coordinates, then into the complete layout's absolute pointer frame. Support fractional scales, negative origins, portrait/rotated/flipped outputs, different resolutions, and valid edge pixels.
- [ ] Use one persistent output-unmapped virtual pointer for motion, button, and axis events; do not emulate scroll as a keyboard symbol or use a one-shot tool that cannot preserve button state.
- [ ] Reject unknown/superseded/old-session frames, stale configuration, non-finite coordinates, and out-of-bounds points before any input. Serialize actions and check the current configuration immediately before dispatch.
- [ ] A real GUI fixture receives the intended click/scroll and the result includes a post-action screenshot and new Observation metadata. Verify native Wayland and XWayland paths where available.
- [ ] Check points across at least two compositor outputs with mixed scales and negative/rotated placement. Virtual outputs are acceptable reproducible evidence; explicitly state whether physical multi-Monitor hardware was used.
- [ ] Report input effect as none/completed/partial/unknown, independently of screenshot success. If input occurred but capture failed, require observation rather than replay. Do not assume protocol roundtrip alone proves the application received a click.
- [ ] Match the specification's `isError` policy and cover all result variants in any declared output schema. Verify Codex receives the explicit no-replay guidance on failures with possible side effects.
- [ ] Track and release held buttons on ordinary completion and failures. Basic cleanup and stale checks are required here; later recovery work exercises configuration changes and interruption during a longer sequence.
- [ ] Add behavior-level MCP checks and coordinate boundary fixtures; record the demonstrated backend mapping on the deployed Hyprland version.

## Blocked by

- https://github.com/Klyve-09/computer_use/issues/2

## Design evidence

The disposable prototype on [prototype source](https://github.com/Klyve-09/computer_use/tree/5c853b613b9bebbdb4743ad4cf4bb4471561dc14), demonstrates the coordinate model and stale-generation rejection. Its C probe proves only protocol discovery and one single-monitor no-op movement; this ticket must supply actual application-delivery evidence.
