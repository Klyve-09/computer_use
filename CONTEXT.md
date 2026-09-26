# Computer Use

Tools that let an AI assistant interact with the user's computer to carry out requested tasks.

## Language

**Computer Use MCP**:
The computer-control tool service exposed to an AI assistant. The assistant supplies task-level decisions and may delegate a bounded Goal to the service.
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

**Goal**:
A small result delegated by the assistant, with a limited scope of permitted interactions and an observable completion condition.
_Avoid_: Unrestricted task, arbitrary autonomous workflow

**UI Evidence**:
Information about visible controls or text in the current Desktop that supplements an Observation when choosing an Action.
_Avoid_: Observation, proof that an action is safe

**Action Candidate**:
A specific permitted interaction grounded in current UI Evidence, from which the next Action may be selected.
_Avoid_: Invented coordinate, unrestricted instruction

**Fast Path**:
Bounded execution of a Goal without returning to the assistant for every Action. It returns control when it cannot establish a safe next step or verify progress.
_Avoid_: Universal application support, autonomous assistant
