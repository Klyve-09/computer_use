# Computer Use MCP — Interview Decisions

Status: Requirements interview completed and shared understanding approved in the conversation on 2026-09-17. The user authorized technical validation, specification/ticket drafting, and preparation of a manual Devin handoff. Concrete technical/specification and ticket-breakdown approval remains pending.

## Confirmed scope

- Deliver a Rust computer-use MCP for the user's current Codex on Linux.
- The user wants to add computer-control capabilities to their Linux Codex workflow, inspired by the experience on Windows/macOS.
- Initially support personal use on the current Hyprland/Wayland desktop. Cross-platform support and validation with other MCP clients are deferred.
- Control the user's active desktop, including browsers and ordinary desktop applications. The user accepts that concurrent manual input can interfere with execution.
- Codex observes screenshots and selects coordinate-based actions. DOM and accessibility-tree integration are deferred pending evidence of need.
- Include click, double-click, right-click, scroll, keyboard shortcuts, text entry, and drag.
- Text entry must support Korean and multiline strings without relying on the current Korean/English input mode.
- Rust may invoke existing Linux utilities. Required external dependencies must be documented.
- Text entry may replace the clipboard contents and paste the requested string. Restoring arbitrary prior clipboard contents is outside the first version.
- Each action call performs one action and returns a screenshot taken afterward. A drag or keyboard shortcut counts as one action. Codex chooses the next action based on the result; failures return an error for Codex to assess.
- Support multiple monitors and different display scales, including mixed scaling across simultaneously connected monitors. The current single-monitor setup is a test environment, not a product scope limit.
- Codex can list monitors, select a monitor, and act using coordinates in that monitor's screenshot. The MCP converts screenshot coordinates into desktop input coordinates, accounting for scale, layout, and orientation. Include portrait displays and monitors positioned left of or above other displays.
- Support monitor connection, disconnection, scale changes, and layout changes during execution. Reject actions based on an outdated display configuration and require a fresh observation before further action.
- Include cross-monitor drag. Its source and destination each specify a monitor and screenshot coordinates, including when their scales differ.

## First end-to-end acceptance scenario

From the user's current Codex, observe the screen, click the browser address bar, enter a designated address, navigate, and observe the resulting page to verify success.

## Design validation and next gate

- Completed a disposable coordinate/configuration model and a narrow live capture/virtual-pointer probe on branch `prototype/computer-use-coordinates`, commit `5c853b613b9bebbdb4743ad4cf4bb4471561dc14`.
- The concrete specification and five ticket drafts define the verification matrix and distinguish synthetic, nested/headless, and physical-device evidence. The currently observed hardware has one active monitor.
- User approved the drafted testing seams and five-ticket breakdown. The specification is published as [Issue #1](https://github.com/Klyve-09/computer_use/issues/1); tickets are [#2](https://github.com/Klyve-09/computer_use/issues/2) through [#6](https://github.com/Klyve-09/computer_use/issues/6), all labelled `ready-for-agent` with native blocking relationships.

## Environment evidence

Read-only inspection on 2026-09-17 found one active monitor at 2560x1440, scale 1.25, position (0,0), and normal orientation. Capture pixel coordinates and input coordinates must be verified rather than assumed equivalent.

The installed Codex CLI's `mcp add --help` explicitly supports command-launched stdio servers. This establishes a local integration path; actual MCP image rendering and tool execution remain to be tested.

## Delivery workflow

Astra owns design, disposable validation prototypes, and review. Product implementation is handed to Devin after approved specs and tickets. Follow `/home/hyh/.agents/skills/to-spec/SKILL.md` and then `/home/hyh/.agents/skills/to-tickets/SKILL.md`; publish according to the repository's GitHub Issues conventions. The eventual handoff must require `/home/hyh/.agents/skills/implement/SKILL.md` and `/home/hyh/.agents/skills/ponytail/SKILL.md`.
