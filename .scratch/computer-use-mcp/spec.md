# Rust Computer Use MCP for Linux Codex

Status: Approved by the user on 2026-09-17, including technical decisions, verification seams, and the five-ticket breakdown. Authorized for GitHub publication and manual Devin handoff.

## Problem Statement

The user wants Codex on their Linux desktop to observe and operate graphical applications through MCP, providing the computer-use workflow they associate with Codex on Windows/macOS. The repository currently contains agent workflow documentation and no product implementation.

## Solution

Deliver a local Rust stdio MCP server for the user's Hyprland/Wayland session. Codex selects a Monitor, reads an Observation, and requests one Action at a time. The server translates screenshot coordinates, performs mouse or keyboard input, and returns a post-action Observation. Codex remains responsible for interpreting the screen and deciding what to do next.

Support the user's active Desktop, including browsers and ordinary applications; multiple Monitors with mixed scales, rotation, and negative layout positions; runtime Display Configuration changes; and Cross-monitor Drag. The user's manual input can interfere with automation because the Desktop is shared.

The first end-to-end demonstration is Codex observing a browser, clicking its address bar, entering a designated address, pressing Enter, and observing the destination page. An additional GUI text demonstration verifies Korean and multiline input.

## User Stories

1. As the user, I want to register one local server command in my current Codex so that computer-use tools appear in my existing workflow.
2. As Codex, I want to enumerate connected Monitors so that I can select the intended screen.
3. As Codex, I want a screenshot-only Observation so that I can decide what to do without causing input.
4. As Codex, I want screenshot dimensions and an opaque Observation ID so that I can refer to the exact coordinate space I saw.
5. As Codex, I want to specify pointer targets in screenshot pixels so that the server handles display scaling.
6. As the user, I want clicks to work on Monitors with different resolutions and fractional scales.
7. As the user, I want portrait, rotated, and flipped screen configurations to be mapped consistently with their displayed screenshots.
8. As the user, I want Monitors left of or above another Monitor to work despite negative desktop positions.
9. As Codex, I want left, right, and double-clicks so that I can use normal application controls.
10. As Codex, I want horizontal and vertical scrolling at a chosen screen location so that I can navigate content.
11. As Codex, I want keyboard shortcuts and individual keys so that I can navigate applications and submit an address.
12. As the user, I want Korean and multiline text entered exactly, independently of the current input-language mode.
13. As the user, I accept clipboard replacement for text entry and want that behavior documented.
14. As Codex, I want a same-monitor drag so that I can move items and manipulate controls.
15. As Codex, I want a Cross-monitor Drag whose endpoints use different Monitor screenshots so that mixed scaling does not misplace the drop.
16. As Codex, I want an Observation after an Action so that I can inspect its visible result.
17. As Codex, I want stale observations rejected after connection, disconnection, scale, orientation, or layout changes so that I can refresh before acting.
18. As Codex, I want a configuration change followed by restoration to invalidate earlier observations too.
19. As the user, I want interrupted actions to release held inputs as far as the session permits.
20. As Codex, I want partial or uncertain input delivery distinguished from no input so that I do not blindly repeat an action.
21. As Codex, I want a failed post-action screenshot distinguished from a failed action so that I can observe again without repeating input.
22. As the user, I want clear dependency and session diagnostics so that I can start the server without guessing about missing tools.
23. As the user, I want the delivered feature tested through Codex and ordinary GUI applications, with simulated and real-device evidence clearly distinguished.

## Implementation Decisions

### Runtime and dependencies

- One local Rust process, stdio MCP, using the official Rust MCP SDK (`rmcp`) with the required server/schema/stdio features. Pin resolved dependencies in the lockfile and verify the chosen release against the installed Codex. Research found `rmcp` 3.4.0; revalidate compatibility during implementation rather than assuming version numbers prove it.
- Use `hyprctl` for Monitor snapshots, the Hyprland event socket for configuration invalidation, and `grim` for selected-Monitor PNG capture. Pass arguments directly to subprocesses; input text is stdin data, never shell code.
- Use a persistent, output-unmapped Wayland virtual pointer via established Rust Wayland protocol bindings for motion, buttons, scrolling, and drag. Scrolling is mandatory, so a dispatcher-only pointer implementation is insufficient.
- Use Hyprland keyboard dispatchers with explicit press/release cleanup, subject to real application verification on the deployed version. If that path fails required shortcut behavior, record the evidence and use the smallest functioning virtual-keyboard path; preserve the approved API and text semantics.
- Use `wl-copy` for Unicode text and an explicit paste shortcut. Default is Ctrl+V, with Ctrl+Shift+V supported for callers targeting terminals. Never infer paste behavior from a window title. Clipboard restoration is not required.
- Keep concrete platform operations and coordinate logic small. No generic backend framework, remote service, or privileged input daemon is required.

### Public tools and targeting

- `computer_monitors`: read-only discovery of selectable active Monitors, their identifiers, logical bounds, scale/orientation, and current opaque Display Configuration revision.
- `computer_observe`: read-only selected-Monitor PNG plus a new opaque Observation ID, actual image dimensions, Monitor identifier, and Display Configuration revision. It bootstraps input and permits recovery without replaying an Action.
- `computer_action`: one discriminated Action: click (left/right, single/double), scroll (signed horizontal/vertical wheel steps), key (key plus modifiers), type_text (exact UTF-8 and paste shortcut), or drag (start/end Observation references and points). A drag or shortcut is one Action; action arrays and arbitrary command execution are not exposed.
- Pointer positions are finite coordinates in the referenced returned image, with half-open bounds from zero to image width/height. Reject invalid points before moving the pointer; do not clamp a wrong point into a different target.
- Keyboard and text actions refer to an Observation of the intended Monitor and act on the currently focused application there. They do not silently pick or focus an application. If focus is on another Monitor at validation time, return a caller-visible error before input; Codex can first click the intended target.
- On success, return the post-action image and metadata for the target Monitor; for Cross-monitor Drag, return the destination Monitor. Codex may separately observe the source if needed.
- Keep the latest Observation metadata per Monitor in memory; superseded, disconnected, or old-session Observation IDs are rejected. Screenshots need not be persisted. Two drag endpoints can reference separate current Monitor observations under the same configuration revision.

### Coordinate mapping

- Derive oriented logical Monitor rectangles from the actual compositor state and validate the derivation against capture dimensions. Use the actual returned image size, not an assumed native resolution or integer scale.
- An already upright rendered screenshot maps into its oriented logical rectangle; do not apply a second rotation or flip to its coordinates.
- The disposable prototype established this mapping: desktop x = Monitor x + image x × logical width / image width; desktop y is analogous. Preserve precision until the final backend conversion and define edge rounding with fixtures so the last valid image pixel stays inside its target Monitor.
- For the output-unmapped virtual pointer, translate signed desktop positions into the non-negative bounding rectangle of the complete logical Monitor layout and normalize using that rectangle's dimensions. Hyprland 0.56.2 source supports this mapping; multi-output delivery must still be tested.
- Cross-monitor Drag validates both endpoints before pressing, then uses one persistent pointer and a bounded move sequence with button release at the destination. Layout gaps, rotations, and mixed scale belong in acceptance fixtures and compositor integration cases. The caller supplies endpoints, not an arbitrary scripting language.

### Configuration changes and execution

- An Observation belongs to a process/session epoch, monotonic configuration generation, and geometry fingerprint. A geometry hash alone is insufficient: change-and-restore can return to identical geometry.
- Advance/invalidate on relevant Monitor/configuration events, detected snapshot changes, event connection loss/reconnect, or uncertain state. Establish event monitoring before issuing actionable observations. Use fresh snapshots around capture and immediately before input; reject observations already known stale before sending input.
- Verify event coverage for each supported display-change mechanism, including runtime scale/layout changes. Hyprland's event socket has no replay cursor, and generic config-reload events alone do not prove every change-and-restore is observable. If required mutations are not covered, add the necessary compositor/Wayland output notifications before claiming that guarantee; polling alone cannot detect a change that returns between snapshots.
- If the event connection is unhealthy, allow diagnostics but pause mutation until it is re-established and new observations are issued. Server restart invalidates all previous Observation IDs.
- Serialize actions. Validate the whole request, both drag endpoints, and target state before any side effect. Track held buttons/modifiers and release on completion, timeout, cancellation, detected configuration change, and orderly shutdown.
- A display change can race an input sequence. Stop further intended input when detected, prioritize releases, invalidate observations, and report partial/unknown delivery. There is no atomic compositor transaction that makes mid-action rollback possible; do not claim zero input unless it is known.
- Capture after input completion with a bounded settling interval; this is an observation time, not proof the application has finished loading. Codex can call observe again.
- Bound capture, subprocess, and input-sequence duration and payload sizes; document the chosen bounds and return actionable failures. Do not introduce automatic side-effect retries.

### Result semantics

- Return image content plus compact JSON text and equivalent structured content, using the SDK's MCP result types. Keep diagnostic logs on stderr, never protocol stdout.
- Distinguish input effect as `none`, `completed`, `partial`, or `unknown`; distinguish Observation success from input effect. These describe attempted input delivery, not application-level task success.
- An Action rejected before side effects has effect `none`. A paste that replaced the clipboard but failed to inject the shortcut has a side effect and must not be described as having done nothing.
- If input completed and capture failed, report a partial result with effect `completed` and explicit instruction to observe again, not repeat the action. Partial/unknown delivery also requires observation and assessment before any retry.
- Unknown tools or invalid protocol envelopes use MCP protocol errors; caller-visible execution failures use structured tool results. State whether an Observation is current/actionable when a display change is detected around capture.
- Set `isError` to false for fully successful results and true for caller-visible execution failures, including rejected input, partial/unknown delivery, and capture failure after completed input. Every result with a possible side effect must prominently say not to replay automatically and instruct the caller to observe and assess. MCP does not guarantee client retry behavior; verify the actual Codex handling rather than treating this flag as a retry control.
- Declare output schemas only if they cover every success, rejected, partial, unknown, and capture-failure variant returned by that tool. Compact JSON text must agree with the structured content.

## Testing Decisions

### Approved verification seams

The principal seam is the public stdio MCP boundary: initialize, list tools, invoke discovery/observation/actions, and inspect image/result behavior. Test user-visible behavior rather than helper implementations.

Use controlled compositor/input/capture fixtures only where deterministic display layouts, disconnections, or failures cannot be driven reliably through a real desktop. Keep the pure coordinate/configuration model separately exercisable for boundary and state-transition cases. These fixtures do not substitute for real GUI delivery checks.

### Required evidence

1. Current Codex initializes the server, discovers the three tools, displays a PNG Observation, invokes input, and receives a new image. No production server exists yet, so the prototype did not prove MCP integration.
2. The browser address-entry scenario runs through actual MCP tool calls; a harmless local fixture page may be the designated address to keep the demonstration repeatable.
3. Native Wayland and XWayland GUI fixtures receive clicks, double/right-click, scroll, shortcuts, and Korean/multiline paste as intended. Terminal paste is separately checked with the explicit terminal shortcut.
4. Coordinate fixtures cover normal/rotated/flipped transforms, mixed scales including 1.0/1.25/1.5/2.0, other valid compositor scales, negative origins, unequal resolutions, layout gaps, and edge/out-of-bounds coordinates.
5. A compositor integration environment exercises at least two outputs with mixed scales and Cross-monitor Drag. A nested/headless compositor is acceptable for reproducible automated evidence, but report it as virtual output evidence; document any outstanding physical-device verification.
6. Change scale/layout, disconnect a Monitor, change then restore geometry, restart the server, and interrupt the event connection. Old Observation IDs must not authorize subsequent input.
7. Inject cancellation/failure during a drag and shortcut, clipboard success followed by paste failure, and input success followed by capture failure. Verify release attempts, effect classification, fresh-observation recovery, and no automatic replay.
8. Verify concurrent requests cannot interleave held input, bounds are enforced, stdout remains valid MCP, and a missing dependency/session has a useful diagnostic.

There are no existing product tests to reuse. The disposable prototype is design evidence, not production test coverage.

## Out of Scope

- Windows/macOS and other Linux compositor backends in this release.
- DOM or accessibility-tree automation, OCR pipelines, autonomous planning, and a separate assistant UI.
- Remote HTTP MCP, isolated desktops as the user's normal execution mode, and elevated uinput services.
- Restoring arbitrary clipboard contents, action batching, replaying side effects automatically, and guaranteeing application success from input delivery alone.
- Touch/pen gestures and application-specific automation adapters.

## Further Notes

- Product implementation belongs to Devin. Astra supplies design evidence and review; the prototype is not promoted into the product.
- Required implementation skills: `implement` and `ponytail` (full), as specified by the repository's agent instructions.
- Prototype: [retained source and verdict](https://github.com/Klyve-09/computer_use/tree/5c853b613b9bebbdb4743ad4cf4bb4471561dc14) on branch `prototype/computer-use-coordinates`. The single HTML model and C protocol probe are retained there as primary sources.
- Actual evidence on the current hardware: selected-Monitor capture at native and scale-1 sizes; virtual-pointer v2 discovery; one no-op absolute movement with unchanged measured cursor position. No physical multi-monitor test, real click/scroll/drag delivery, text paste, or Codex MCP image roundtrip has yet been performed.
- Rust is absent from the current PATH. Provision and record a toolchain as part of the first implementation slice; no global toolchain or Codex configuration was changed during design validation.
- Supporting sources: [official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk), [MCP tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools), [Hyprland pointer mapping source](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/pointer/PointerManager.cpp), [virtual pointer protocol](https://github.com/swaywm/wlr-protocols/blob/master/unstable/wlr-virtual-pointer-unstable-v1.xml), [grim](https://github.com/emersion/grim), and [wl-clipboard](https://github.com/bugaevc/wl-clipboard).
