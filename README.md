# Computer Use MCP

A local stdio [MCP](https://modelcontextprotocol.io) server giving an AI
assistant screenshot-based observation of the user's Hyprland/Wayland desktop.
Approved specification: GitHub issue
[`Klyve-09/computer_use#1`](https://github.com/Klyve-09/computer_use/issues/1).

## Status

Implemented (issue #2): `computer_monitors` and `computer_observe` —
read-only monitor discovery and per-monitor PNG capture. Input actions
(`computer_action`) arrive in issues #3–#5; recovery hardening in #6.

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

Observations are bound to a session epoch + event-driven generation + a
geometry fingerprint. Any `monitoradded*`/`monitorremoved*`/`configreloaded`
event, event-socket loss, or server restart invalidates earlier observation
IDs; a change-and-restore also invalidates because the generation advances
even when the fingerprint returns to its previous value.

## Limits

- Capture bound: 10 s timeout, 48 MiB max PNG payload. `hyprctl` bound: 5 s.
- Coordinates in returned images are screenshot pixels; the mapping into
  desktop coordinates lives behind `computer_action` (not yet implemented).
- Screenshots are held in memory per request only; nothing is persisted.
- If the Hyprland event socket is unavailable, `actionable` reports `false`
  and `events_healthy` is `false`; observations still return for diagnostics.
- Change-and-restore that happens *between* the event socket's loss and its
  reconnect cannot be distinguished — that's why every disconnect bumps the
  generation unconditionally.

## Verification

```sh
cargo test    # unit tests + live stdio JSON-RPC tests (need a Hyprland session)
```

The integration tests speak raw MCP over the server's stdio: initialize,
`tools/list`, `computer_monitors`, `computer_observe`, and error variants.
Live Codex verification was performed with `codex exec`.
