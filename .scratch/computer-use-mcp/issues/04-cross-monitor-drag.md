# 04: Drag within and between mixed-scale Monitors

Status: Approved on 2026-09-17; ready-for-agent.

## Parent

[Approved specification](https://github.com/Klyve-09/computer_use/issues/1).

## What to build

Allow Codex to move a draggable item within a Monitor or between Monitors using a source and destination selected from their screenshots, including different scales and orientations.

## Acceptance criteria

- [ ] Add a drag variant with source and destination Observation references and image-local coordinates. Both must resolve under the same current Display Configuration before the first press.
- [ ] Execute one serialized move/press/move/release sequence using the persistent pointer. A drag is one MCP Action; do not replace it with a window-manager move command.
- [ ] Convert endpoints independently using each image's dimensions and Monitor geometry. Cover mixed scales, negative positions, portrait/rotated screens, edge points, and layouts with gaps.
- [ ] Bound the drag duration/path and always attempt release if a move, capture, cancellation, or display event interrupts the action. Report partial/unknown effects honestly; never report an already emitted press as no input.
- [ ] Return the destination Monitor's post-action screenshot and new Observation. Source-only inspection remains available through `computer_observe`.
- [ ] Demonstrate an actual GUI drag within a Monitor and across two compositor outputs, observing the resulting drop. Label nested/headless output evidence separately from physical Monitor evidence.
- [ ] Reject a disconnected/stale destination before pressing, and exercise a mid-drag configuration change with release attempts and fresh-observation recovery.
- [ ] Add behavior-level checks for endpoint validation, event ordering, post-action result semantics, and cleanup. Keep the prototype as design evidence rather than copying its HTML/C into the product.

## Blocked by

- https://github.com/Klyve-09/computer_use/issues/3

## Design evidence

Use the Cross-monitor Drag and change-and-restore walkthroughs on prototype [prototype source](https://github.com/Klyve-09/computer_use/tree/5c853b613b9bebbdb4743ad4cf4bb4471561dc14).
