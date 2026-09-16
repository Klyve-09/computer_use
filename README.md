# Disposable computer-use coordinate prototype

This branch contains design evidence, not the product implementation. The product remains Rust and is assigned to Devin after spec/ticket approval.

## Questions and verdicts

1. Can monitor-local screenshot coordinates cover mixed scale, negative origins, and portrait displays? **Yes in the model.** Store the observed image size and the monitor's oriented logical extent; map by their ratio plus the monitor origin. Do not rotate an already oriented screenshot a second time.
2. Can an old screenshot be rejected after the display configuration changes and returns to the same geometry? **Yes with a generation, not a geometry hash alone.** A configuration change advances the generation. Stream loss and server restart must also invalidate observations.
3. Can a cross-monitor drag validate both endpoints before any press? **Yes in the model.** Both frames must belong to the current configuration generation.
4. Does the current compositor expose a usable virtual pointer? **Yes for protocol discovery and one no-op absolute move.** Scroll, button, and held-drag delivery were not exercised.

## Run the model

Open `coordinate-prototype.html` in a browser. It is a single self-contained file with free-play controls and four guided walkthroughs. It never sends real input. Its pure model is in the script block with ID `model`.

Example walkthrough outputs:

| Scenario | Result |
|---|---|
| Laptop 2560x1440 image / 2048x1152 logical, center | Desktop (1024, 576) |
| Left monitor 3840x2160 image / 1920x1080 logical at (-1920,0), center | Desktop (-960, 540) |
| Portrait 1080x1920 image at (2048,-400), center | Desktop (2588, 560) |
| Drag laptop center to left center | (1024,576) to (-960,540) |
| Configuration changes and returns; old observation reused | Rejected before simulated input |
| Coordinate equal to image width/height | Rejected as outside image bounds |

Rendered in headless Chrome at 1280x1500 and visually inspected. Walkthrough calculations were exercised directly with Node; this is exploratory evidence, not a production test suite.

## Actual local observations (2026-09-17)

- `hyprctl -j monitors`: one 2560x1440 output, scale 1.25, normal transform, origin (0,0).
- `grim -o eDP-1 -` returned a 2560x1440 PNG (1,317,017 bytes for the observed screen).
- `grim -o eDP-1 -s 1 -` returned a 2048x1152 PNG (822,725 bytes for that screen).
- PNGs were inspected in memory for dimensions, not retained in this branch. Byte sizes are scene-dependent.
- `pointer-probe` found `zwlr_virtual_pointer_manager_v1` version 2.
- A no-op absolute motion using the current cursor (17,566) and extent (2048,1152) completed a compositor roundtrip; `hyprctl -j cursorpos` still returned (17,566). No click, scroll, or keyboard event was sent.

## Optional protocol probe

Requires the already installed C compiler, Wayland development library, and scanner. It is C only to avoid installing a Rust toolchain for this small design probe.

```sh
wayland-scanner client-header wlr-virtual-pointer-unstable-v1.xml virtual-pointer.h
wayland-scanner private-code wlr-virtual-pointer-unstable-v1.xml virtual-pointer-code.c
cc -Wall -Wextra pointer-probe.c virtual-pointer-code.c -lwayland-client -o pointer-probe
./pointer-probe
```

With no arguments the probe only discovers protocol support. Four numeric arguments send one absolute move: x, y, x extent, y extent. The probe is deliberately not a reusable input backend.

Protocol XML was retrieved from the [upstream mirror](https://github.com/swaywm/wlr-protocols/blob/master/unstable/wlr-virtual-pointer-unstable-v1.xml); its copyright and permissive license remain in the file.

## Verification limits and implementation consequences

- Synthetic monitor geometry proves arithmetic/state decisions only. Physical mixed-scale monitors, rotation, hotplug, and cross-monitor drags remain unverified on real hardware.
- The probe proves neither application receipt nor multi-output absolute-motion semantics. A no-op point is a narrow smoke check, not a calibration sweep.
- Compositor changes can race with input. Recheck before dispatch, observe display events, and release held input on interruption. An action partly emitted before a change cannot be rolled back or honestly reported as never performed.
- Screen capture immediately after input is not proof the application finished rendering. Expose screenshot-only observation so Codex can observe again without replaying the action.
- Clipboard paste and native/XWayland application shortcut behavior remain implementation acceptance work.
- Carry the decisions into the specification; do not promote this HTML or C probe into product code.
