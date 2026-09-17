# Backend research: Hyprland/Wayland computer-use MCP

Research date: 2026-09-17. Scope: a local Rust stdio MCP controlling the user's current Hyprland desktop. This note recommends a backend and records the coordinate, input, clipboard, invalidation, and dependency constraints that the implementation specification should preserve.

## Recommendation

Use the existing capture/display commands plus one small, persistent in-process Wayland virtual-pointer client:

1. `hyprctl -j monitors all` is the authoritative display snapshot and `grim -o <name>` captures one selected monitor.
2. Convert a point from that monitor's returned PNG dimensions into Hyprland logical layout coordinates. Convert that signed global point into the unsigned absolute frame of one output-unmapped `zwlr_virtual_pointer_v1`, using the bounding box of the complete logical monitor layout. Send `motion_absolute` followed by `frame`.
3. Use the same persistent virtual pointer for left/right/middle button down/up, smooth or discrete axes, and drags. Keep a process-local pressed-button set and release every held button on errors and orderly shutdown. A drag is one serialized transaction: absolute move to source, button down, one or more absolute moves, button up, with protocol frames at event boundaries.
4. Continue to use paired `hyprctl dispatch sendkeystate` calls for keyboard shortcuts. Use `sendshortcut` only as a convenience after verifying it against the deployed Hyprland version; explicit down/up makes cleanup and failure behavior observable.
5. For arbitrary text, stream the exact UTF-8 bytes to `wl-copy --type text/plain;charset=utf-8`, then issue the application's paste shortcut. This is the only simple path here that handles Korean and multiline text without translating Unicode to a keyboard layout.

This is the smallest complete backend for the approved first-version features. Scrolling rules out a dispatcher-only implementation because Hyprland's documented dispatchers do not expose a dependable pointer-axis transaction. Using one virtual pointer for motion, buttons, scroll, and drag also avoids splitting one pointer transaction across compositor CLI and Wayland protocol state.

Do not use `movewindow` for dragging. It is a window-management dispatcher, not pointer input, so it cannot model dragging a scrollbar, selecting text, drawing, or dragging an item inside an application.

## Why the alternatives rank this way

### Hyprland dispatchers

Current Hyprland documents a cursor move dispatcher and shortcut/key-state dispatchers. `send_key_state` gives explicit `down`, `repeat`, and `up`; the current source resolves the requested key and calls the shared action implementation. Mouse button keycodes use the `mouse:<evdev code>` form, and a Hyprland maintainer-selected answer gives `mouse:272`/`mouse:273` with separate down/up calls for left/right click. Sources: [Hyprland dispatchers](https://wiki.hypr.land/configuring/core/dispatchers/), [current dispatcher binding source](https://github.com/hyprwm/Hyprland/blob/main/src/config/lua/bindings/LuaBindingsDispatchers.cpp), and [Hyprland mouse-button answer](https://github.com/hyprwm/Hyprland/discussions/14354).

The caveat is that these are compositor implementation features, not a stable cross-compositor automation API. Recent upstream reports include modifier state getting stuck with `sendshortcut` and wheel names producing keyboard `NoSymbol` events instead of pointer axis events. These reports are evidence to test and pin behavior, not API guarantees: [modifier-state report](https://github.com/hyprwm/Hyprland/discussions/14099) and [wheel report](https://github.com/hyprwm/Hyprland/discussions/12004). Explicit down/up plus unconditional cleanup reduces the first risk; neither dispatcher solves scrolling, so dispatchers are recommended only for keyboard shortcuts.

### `zwlr_virtual_pointer_v1`

Hyprland builds and advertises the wlr virtual pointer protocol. The protocol directly models relative motion, absolute motion, buttons, axis scrolling, discrete axis steps, and `frame`; version 2 can map a virtual pointer to an output. Absolute request coordinates are unsigned values in a caller-defined `0..extent` frame, whereas relative motion is in global compositor space. Sources: [protocol definition and compositor support](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1) and [Hyprland protocol build list](https://github.com/hyprwm/Hyprland/blob/main/CMakeLists.txt).

This is the complete first-version pointer backend. Keep one object alive for the MCP process instead of creating one per action so button state spans every event in a drag and seat capabilities remain stable. It still needs defensive button cleanup. A current Hyprland report demonstrates that a virtual-pointer client exiting after a press without a release can leave compositor pointer state damaged, so the implementation must release held buttons before destroying the object and must serialize pointer transactions: [Hyprland stuck-button reproducer](https://github.com/hyprwm/Hyprland/discussions/15423).

Create the pointer without an output mapping. Protocol version 2 says an output argument maps the device to that requested output; such a mapping conflicts with cross-monitor motion. For an unmapped pointer, represent Hyprland's complete logical layout bounding box as the protocol's unsigned absolute frame:

```text
layout_min_x = min(monitor.x)
layout_min_y = min(monitor.y)
layout_max_x = max(monitor.x + monitor.logical_width)
layout_max_y = max(monitor.y + monitor.logical_height)

absolute_x = global_x - layout_min_x
absolute_y = global_y - layout_min_y
x_extent   = layout_max_x - layout_min_x
y_extent   = layout_max_y - layout_min_y
```

Negative Hyprland positions therefore become non-negative protocol coordinates. Fractional scale and rotation have already been resolved into logical monitor rectangles before this conversion. The protocol guarantees only an absolute coordinate frame and optional output mapping; it does not standardize how every compositor maps an unmapped frame onto a non-rectangular multi-output layout. Hyprland 0.56.2's `CPointerManager::warpAbsolute` supplies the deployed-version behavior: when the virtual pointer has no bound output, it builds `topLeft` and `bottomRight` from every monitor's logical box and maps normalized absolute coordinates into that bounding rectangle; when an output is bound, it uses that output's box instead. Source: [Hyprland 0.56.2 `PointerManager.cpp`, `warpAbsolute`](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/pointer/PointerManager.cpp). This grounds the full-layout mapping above for the pinned compositor, including negative logical origins. It does not prove useful cursor routing through layout gaps or every mixed-scale/rotation edge, so retain acceptance tests for mixed scale, rotation, negative origins, gaps, and all monitor edges. If the deployed Hyprland version does not map it correctly, fail the compatibility check rather than silently combine pointer backends.

A throwaway protocol probe on the current single-monitor session negotiated virtual-pointer protocol version 2 and sent an absolute no-op using extent `2048x1152`; the cursor remained at `17,566` as expected. This confirms basic protocol availability and the current one-monitor normalization path only. No live multi-monitor, rotation, fractional-mix, gap, or cross-monitor behavior was exercised by that probe.

### Why not `wlrctl`

`wlrctl` is absent on this host and upstream labels it experimental. Its documented pointer commands are `move <dx> <dy>`, `click [button]`, and `scroll <dy> <dx>`. Its source creates a new virtual pointer for one command; move is relative, click always emits press then release, and there are no separate button-down/button-up or absolute-motion commands. It therefore cannot target an arbitrary screenshot coordinate or hold a button while another command crosses monitors. Installing it would add a dependency without satisfying drag. Sources: [wlrctl manual](https://manpages.debian.org/testing/wlrctl/wlrctl.1.en.html), [upstream README mirrored by Debian Sources](https://sources.debian.org/src/wlrctl/0.2.2-2/README.md), and [pointer implementation](https://sources.debian.org/src/wlrctl/0.2.2-2/pointer.c).

It does not solve text entry. `zwp_virtual_keyboard_v1` sends raw keycodes against a supplied XKB keymap and explicit modifier state; it is suitable for shortcuts, but arbitrary Korean text still depends on the active layout/IME and cannot be represented as a universal Unicode insertion request. Source: [virtual keyboard protocol](https://wayland.app/protocols/virtual-keyboard-unstable-v1).

### Portal/libei

The XDG Remote Desktop portal is the strongest standardized option: it supports relative and stream-relative absolute pointer motion, buttons, axes, keyboard events, and clipboard integration; its documentation recommends EIS/libei over the D-Bus notification methods. However, `Start()` presents a user dialog and the absolute coordinate space belongs to a selected PipeWire stream. That session/consent/capture model is unnecessary for a trusted local Hyprland-only stdio server that already uses `grim`. Keep it as a future portability/security backend, not the first backend. Source: [XDG Remote Desktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html).

`wtype` and `ydotool` are not installed. `ydotool` would also introduce a privileged `/dev/uinput` daemon and physical-device-style coordinates. Neither is needed for the recommended first implementation.

## Coordinates, scale, rotation, and negative layouts

Hyprland's monitor `x`/`y` positions are logical global-layout coordinates. Positions can be negative. The layout size and positions use scaled and transformed resolution; a 3840-wide output at scale 2 occupies 1920 logical units, and a 90-degree transform swaps the logical axes. Hyprland transform values cover normal, 90/180/270 degrees, and flipped variants. Source: [Hyprland monitor documentation](https://wiki.hypr.land/0.54.0/Configuring/Monitors/).

Grim captures an output by name (`grim -o DP-1`). Its layout code obtains logical output geometry, divides physical geometry by scale when it must infer it, and swaps logical width/height for 90/270-degree transforms; its renderer also handles rotation/flip when composing the returned image. Sources: [grim usage](https://github.com/emersion/grim/blob/master/README.md) and [grim output-layout source](https://github.com/emersion/grim/blob/master/output-layout.c).

Use the observed image dimensions rather than assuming `physical / scale` is integral. Coordinates outside the screenshot are invalid and must be rejected; never clamp an assistant-supplied point into a different target:

```text
require 0 <= pixel_x < image_width
require 0 <= pixel_y < image_height
local_x = pixel_x * logical_width  / image_width
local_y = pixel_y * logical_height / image_height
global_x = monitor.x + local_x
global_y = monitor.y + local_y
```

Here `logical_width` and `logical_height` are the monitor's transformed logical extents. Derive them from the display snapshot as `width / scale` and `height / scale`, swapping width and height for transforms 1, 3, 5, and 7; verify these derivations against real rotated fixtures. The PNG is already in the user's visible orientation, so do not rotate or flip the click point a second time. Ratios also isolate rounding at fractional scales. Define edge behavior explicitly (pixel centers and final integer rounding) and test all eight transforms; the upstream docs establish the coordinate spaces but do not promise this application's rounding convention.

Maintain a monotonically increasing in-process display generation driven by a continuously connected Hyprland event-socket reader. Increment it for `monitoraddedv2`, `monitorremoved`, `configreloaded`, socket EOF/error, reconnect, or any parse/sequence uncertainty. An observation stores both the generation and a display-configuration fingerprint: monitor stable identity/name, enabled state, `x`, `y`, `width`, `height`, `scale`, and `transform`. A later change followed by restoration can reproduce the same fingerprint, so fingerprint equality alone is insufficient; an observation is actionable only when its generation still equals the live generation.

Before every action, obtain a fresh `hyprctl -j monitors all` snapshot and require both generation equality and fingerprint equality. Reject the action as stale if either differs or the target disappeared. After every action, obtain another snapshot and confirm the generation and fingerprint again. The post-check can report that the action raced a display change, but it cannot undo input that was already delivered.

Hyprland's event socket reports `monitoraddedv2`, `monitorremoved`, and `configreloaded`. Source: [Hyprland IPC events](https://wiki.hypr.land/0.41.0/IPC/). The generation rule above is deliberately conservative because the socket has no replay cursor: after loss, the client cannot prove that it saw every intervening change. A new connection must invalidate every existing observation even if the next snapshot matches. `hyprctl cursorpos` is explicitly documented as global layout coordinates and is useful for verification: [hyprctl documentation](https://wiki.hypr.land/0.55.0/Configuring/Advanced-and-Cool/Using-hyprctl/).

For a cross-monitor drag, validate both endpoints against the same generation and fingerprint, convert each independently using its own monitor/image dimensions, then execute one uninterrupted down/move/up transaction. A gap between monitors may cause compositor clipping; interpolation should therefore use the logical layout segment and tolerate that physical layouts need not form one rectangle. Always attempt button-up if an intermediate move fails.

These checks narrow races but cannot eliminate them. Hyprland can reconfigure after the pre-snapshot, between protocol events, or immediately after the post-snapshot. No available source provides an atomic operation combining monitor-state validation with input delivery. The API must therefore guarantee rejection of already-known stale observations and detection of many concurrent changes, while explicitly returning an indeterminate/raced result when the generation or post-snapshot changes mid-action. It cannot guarantee that zero input was delivered in that case. For a drag, cleanup (button-up) takes priority over further motion after invalidation.

## Text and shortcut limitations

`wl-copy` supports arbitrary MIME data and allows an explicit MIME type. Send text on stdin rather than as a shell argument to preserve NUL-free UTF-8, newlines, quotes, leading dashes, and avoid command injection. `--paste-once` is unsuitable as a default because some clients request clipboard content more than once and XWayland pasting is known to break. Sources: [`wl-copy(1)`](https://man.archlinux.org/man/wl-copy.1) and the [upstream explanation of repeated client requests](https://github.com/bugaevc/wl-clipboard/issues/107).

Clipboard paste has unavoidable product semantics:

- It replaces the user's clipboard unless the service first reads and later restores the prior offer. Restoration is inherently racy if the user or another app changes the clipboard meanwhile. The simplest honest first version should document that `type_text` changes the clipboard rather than attempt a race-prone restore.
- The receiving app decides how multiline text behaves. A normal GUI editor inserts newlines; a terminal may use bracketed paste or interpret the pasted block according to its own settings. The MCP cannot promise that multiline paste never submits a form or command.
- A single paste shortcut is not universal. Default to `Ctrl+V`, expose a deliberate terminal variant (`Ctrl+Shift+V`), and let the caller choose. Do not infer the application from window titles in the backend.
- Clipboard transport handles Korean Unicode. Shortcut/keycode injection alone does not bypass keyboard layout or IME state.

## Local dependency inventory

Observed on this host without installing or injecting input:

- Present: `/usr/bin/grim`, `/usr/bin/hyprctl`, `/usr/bin/wl-copy`, `/usr/bin/wayland-scanner`, and `pkg-config` metadata for `wayland-client` 1.26.0 and `xkbcommon` 1.13.2.
- Hyprland is 0.56.2; grim's help includes output capture, scale, geometry, cursor, and foreign-toplevel options; wl-clipboard is 2.3.0.
- Absent from `PATH`: `wtype`, `ydotool`, `wlrctl`, `rustc`, and `cargo`.
- The standard Wayland protocols data directory exists, but the searches performed did not find packaged `wlr-virtual-pointer-unstable-v1.xml` or `virtual-keyboard-unstable-v1.xml`. A direct protocol implementation therefore needs vendored, license-preserved XML (or an appropriate Rust protocol crate) plus generated bindings.

The immediate missing dependency is the Rust toolchain. For protocol bindings, use Smithay's `wayland-client` and `wayland-protocols-wlr` crates with the latter's `client` feature. The released crate directly includes the wlr virtual-pointer XML and generated client module, including `motion_absolute`, button, axis, discrete-axis, stop, and frame requests: [crate documentation](https://docs.rs/wayland-protocols-wlr/latest/wayland_protocols_wlr/) and [virtual pointer client API](https://docs.rs/wayland-protocols-wlr/latest/wayland_protocols_wlr/virtual_pointer/v1/client/zwlr_virtual_pointer_v1/struct.ZwlrVirtualPointerV1.html). This is simpler and less error-prone than vendoring XML or adding an external executable. The implementation does not need `wlrctl`, `wtype`, or `ydotool`. A direct virtual keyboard would also need XKB keymap construction, which is unnecessary if shortcuts remain on `hyprctl` and arbitrary text remains clipboard paste.

## Verification required before implementation acceptance

Test against the pinned Hyprland version with `wev` or equivalent event observation, covering: all three button pairs; a held-button move across two outputs; scroll direction and step magnitude if virtual pointer is implemented; modifier cleanup after shortcut failure; Korean and multiline paste into one native Wayland GUI, one XWayland GUI, and a terminal; fractional scale; each rotation/flip class; a negative monitor origin; hotplug/config change between screenshot and action; MCP cancellation and process exit while a button or modifier is held.

No input was injected and no packages were installed during this research.
