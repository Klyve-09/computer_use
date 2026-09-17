# Computer Use

Tools that let an AI assistant interact with the user's computer to carry out requested tasks.

## Language

**Computer Use MCP**:
The computer-control tool service exposed to an AI assistant. It is the project's deliverable; the assistant consuming its tools supplies the task-level decisions.
_Avoid_: Autonomous agent, assistant application

**Desktop**:
The user's active graphical workspace, shared by the user and the assistant. It includes browser windows and other desktop applications.
_Avoid_: Isolated desktop, browser-only workspace

**Observation**:
A screenshot of a selected monitor that the assistant uses to understand the visible state and choose its next action.
_Avoid_: DOM snapshot, accessibility tree

**Action**:
A mouse or keyboard interaction requested by the assistant, with pointer targets expressed as coordinates in a specified monitor's screenshot.
_Avoid_: Task, autonomous workflow

**Monitor**:
A connected display that the assistant can identify and select for observation or an action target.
_Avoid_: Window, desktop

**Display Configuration**:
The connected monitors and their current arrangement, dimensions, scales, and orientations. A change invalidates the previous observations as a basis for actions.
_Avoid_: Resolution alone

**Cross-monitor Drag**:
A single drag action whose starting and ending points are on different monitors.
_Avoid_: Two separate drags
