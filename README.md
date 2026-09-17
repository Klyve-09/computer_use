# Computer Use MCP

A local stdio [MCP](https://modelcontextprotocol.io) server giving an AI
assistant screenshot-based observation of the user's Hyprland/Wayland desktop.
Approved specification: GitHub issue
[`Klyve-09/computer_use#1`](https://github.com/Klyve-09/computer_use/issues/1).

## Status

Implemented (issues #2–#3): `computer_monitors`, `computer_observe`, and
`computer_action` (left/right click, double-click, signed horizontal/vertical
scroll). Keyboard/text actions and drag arrive in issues #4–#5; recovery
hardening in #6.

## Requirements

- A running Hyprland session (tested on Hyprland 0.56.2)
- `grim` and `hyprctl` on `PATH` (both used by the server)
- Rust stable toolchain to build (`rustup` works)

The server discovers the live session itself
(`HYPRLAND_INSTANCE_SIGNATURE`, `XDG_RUNTIME_DIR`/`/run/user/<uid>`,
`WAYLAND_DISPLAY`), so clients that scrub the environment — including Codex —
still work. With multiple concurrent Hyprland instances it picks the first
with an event socket.

## Build and run

```sh
cargo build --release
./target/release/computer-use-mcp   # stdio server; logs go to stderr only
```

## Register with Codex

```sh
codex mcp add computer-use -- /path/to/computer-use-mcp
```

One command; no env flags needed because of the session discovery above.

## Tools

| Tool | Effect |
|---|---|
| `computer_monitors` | Lists selectable monitors: id (Hyprland output name), description, oriented logical bounds, scale, transform, plus an opaque `revision` and `events_healthy`. |
| `computer_observe` | `{"monitor": "<id>"}` → PNG image block, `observation_id`, actual image dimensions, monitor summary, `revision`, `actionable`. |
| `computer_action` | `{"observation_id": "<id>", "action": {"kind": "click"|"scroll", ...}}` → performs one input action at image-pixel coordinates and returns a fresh observation of the same monitor. `effect` reports `none`/`completed`/`partial`/`unknown`; on partial/unknown or post-action capture failure the result says not to replay. |

Observations are bound to a session epoch + event-driven generation + a
geometry fingerprint. Two notification channels advance the generation:

1. The Hyprland IPC event socket (`monitoradded*`, `monitorremoved*`,
   `configreloaded`; any disconnect also invalidates).
2. A `wl_output` listener — required because Hyprland 0.56.2's IPC socket
   emits **no** event for runtime reconfigures like
   `hl.monitor({scale = ...})`. Verified live: the IPC socket stayed silent
   while `wl_output` delivered scale/mode/geometry/done events.

A change-and-restore still invalidates: both channels are continuously
connected, so they observe each half of the transition. A server restart
changes the epoch, invalidating every old observation ID. A newer
observation of a monitor supersedes earlier IDs for that monitor.

## Limits

- Capture bound: 10 s timeout, 48 MiB max PNG payload. `hyprctl` bound: 5 s.
- Coordinates in returned images are screenshot pixels; the mapping into
  desktop coordinates lives behind `computer_action` (not yet implemented).
- Screenshots are held in memory per request only; nothing is persisted.
- If either notification channel is down, `actionable` reports `false`
  (`events_healthy` / `wayland_events_healthy` say which); observations still
  return for diagnostics.
- Change-and-restore that happens *between* a channel's loss and reconnect
  cannot be distinguished — that's why every disconnect bumps the
  generation unconditionally.
- `wl_output` reports integer scale factors only; it is used purely as an
  invalidation signal, never for coordinate math.
- Pointer input uses one persistent output-unmapped
  `zwlr_virtual_pointer_v1` mapped onto the bounding box of the whole logical
  layout; scroll uses wheel `axis_discrete` + `axis` (120/step). Input is
  serialized and bounded (10 s pointer wait, 150 ms post-action settle).

## Verification

```sh
cargo test    # unit tests + live stdio JSON-RPC tests (need a Hyprland session)
```

The integration tests speak raw MCP over the server's stdio: initialize,
`tools/list`, `computer_monitors`, `computer_observe`, and error variants.
Live Codex verification was performed with `codex exec`.
