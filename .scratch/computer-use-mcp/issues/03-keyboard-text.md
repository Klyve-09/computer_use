# 03: Enter addresses and Korean text through Codex

Status: Approved on 2026-09-17; ready-for-agent.

## Parent

[Approved specification](https://github.com/Klyve-09/computer_use/issues/1).

## What to build

Complete the first browser workflow: Codex observes and clicks the address bar, enters a designated URL, presses Enter, and observes the loaded page. Also allow exact Korean and multiline text entry in normal GUI applications.

## Acceptance criteria

- [ ] Add single-key/modifier shortcuts and UTF-8 `type_text` variants to `computer_action`, preserving the one-action/post-action-observation contract.
- [ ] Keyboard/text actions refer to a current Observation and use the focused application on that Monitor; reject a known Monitor/focus mismatch before input. Do not silently choose an application.
- [ ] Verify keyboard press/release behavior on the deployed Hyprland version. Release modifiers on completion/failure; distinguish partial or uncertain delivery from no effect.
- [ ] Send exact text through clipboard stdin without shell interpolation, independent of the active language mode. Test Korean, newlines, quotes, and leading dashes. Document clipboard replacement.
- [ ] Support default Ctrl+V and explicit Ctrl+Shift+V paste; let the caller select the latter for terminal use. Report clipboard replacement followed by paste failure as a side effect rather than effect `none`.
- [ ] Demonstrate Korean and multiline paste in a native Wayland GUI and XWayland GUI, and the terminal shortcut in a disposable terminal/text fixture. Record receiving-app behavior; do not claim pasted newlines can never submit an action.
- [ ] Run the full designated-address workflow using actual Codex MCP calls and actual screenshot observation. A local harmless fixture page is sufficient; preserve evidence without recording unrelated desktop content.
- [ ] Add MCP behavior checks for text preservation, modifier cleanup, stale/focus rejection, and input-success/capture-failure recovery; no automatic input replay.

## Blocked by

- https://github.com/Klyve-09/computer_use/issues/3

## Implementation boundaries

Do not add OCR, DOM automation, application-title heuristics, or a planner. The browser workflow is performed by Codex using the generic tools.
