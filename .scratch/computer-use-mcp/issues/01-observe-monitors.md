# 01: Observe a selected Monitor from Codex

Status: Approved on 2026-09-17; ready-for-agent.

## Parent

[Approved specification](https://github.com/Klyve-09/computer_use/issues/1).

## What to build

A runnable Rust stdio MCP that the user's current Codex can launch to list Monitors and view a selected Monitor as an image. Establish the Observation reference and Display Configuration contract used by subsequent Actions, including multiple Monitors and mixed scaling from the start.

## Acceptance criteria

- [ ] Provision/document the Rust toolchain and required existing utilities; choose and lock a compatible official MCP SDK release. The server starts with one documented command in the user's graphical session.
- [ ] Codex completes MCP initialization and lists `computer_monitors` and `computer_observe`; both are read-only tools. Protocol stdout has no diagnostics.
- [ ] Monitor discovery returns usable identifiers and oriented logical geometry, scaling, rotation, and opaque configuration revision for connected selectable outputs.
- [ ] Observing a selected Monitor returns an actual PNG image block, matching dimensions, opaque Observation ID, revision, and equivalent text/structured metadata. Verify the actual Codex displays the image.
- [ ] Capture is bracketed by configuration checks; never issue a current/actionable Observation when configuration changed during capture. Track session epoch, event-invalidated generation, and fresh snapshot fingerprint.
- [ ] Demonstrate old observations becoming invalid after relevant configuration events, change-and-restore, event socket loss/reconnect, and server restart. A pure geometry hash is insufficient.
- [ ] Verify that each supported runtime scale/layout/rotation/hotplug mechanism provides an invalidating notification. Add required Wayland output notifications if Hyprland IPC events leave a gap; polling alone must not be claimed to catch an unobserved change-and-restore.
- [ ] Missing/disconnected Monitors, missing dependencies, failed captures, unavailable session, and bounded payload/time limits have caller-visible diagnostics.
- [ ] Verify native-size and scaled captures on the real current display and use explicit fixtures for rotated/mixed-scale layouts. Keep only bounded in-memory Observation metadata, without persisting desktop screenshots by default.
- [ ] Record the exact commands, client/server versions, and evidence limits; distinguish real desktop and fixture results.

## Blocked by

None (can start immediately).

## Implementation boundaries

Implement and verify this user-visible slice without an input backend or speculative platform abstraction. Later tickets add `computer_action`. Follow the approved specification and the implementation skills required by the handoff.
