# Computer Use MCP

A local stdio [MCP](https://modelcontextprotocol.io) server giving an AI
assistant screenshot-based observation of the user's Hyprland/Wayland desktop.
Approved specification: GitHub issue
[`Klyve-09/computer_use#1`](https://github.com/Klyve-09/computer_use/issues/1).

## Status

Implemented (issues #2–#5): `computer_monitors`, `computer_observe`, and
`computer_action` — click/right-click/double-click, signed horizontal and
vertical scroll, single keys and modifier chords, `type_text` (exact UTF-8
text via clipboard + paste shortcut), and `drag` within or between
monitors. Recovery hardening in #6.

## Requirements

- A running Hyprland session (tested on Hyprland 0.56.2)
- `grim`, `hyprctl`, and `wl-copy` on `PATH` (`wl-copy` for `type_text`)
- `libxkbcommon` (used to build the virtual keyboard's keymap)
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
| `computer_action` | `{"observation_id": "<id>", "action": {"kind": ...}}` → performs one input action and returns a fresh observation of the same monitor. `effect` reports `none`/`completed`/`partial`/`unknown`; on partial/unknown or post-action capture failure the result says not to replay. |

Action kinds:

- `click` — `{x, y, button: "left"|"right", double}` at image-pixel coords.
- `scroll` — `{x, y, dx, dy}` signed wheel steps at image-pixel coords.
- `key` — `{key: "<keysym>", mods: ["ctrl"|"shift"|"alt"|"super"]}` sent to the
  focused window on the observed monitor (key names are XKB keysyms like
  `Return`, `a`, `F5`, `Left`; a bare modifier key name like `Control_L`
  also works).
- `type_text` — `{text: "<exact UTF-8>", paste: "ctrl_v"|"ctrl_shift_v"}`.
  **Replaces the user's clipboard** (that side effect is reported even when
  paste fails). `ctrl_v` is the default; use `ctrl_shift_v` for terminals.
- `drag` — `{x, y, dst_observation_id, dst_x, dst_y}`. One
  move/press/move/release on the persistent pointer: press at the source
  image point, hold 250 ms, eight interpolated steps (~20 ms apart) so
  toolkits recognize a drag rather than a jump, release at the destination.
  Both endpoints resolve independently from their own observations, which
  must be current under the same Display Configuration — a stale or
  disconnected destination rejects before the press. The post-action
  observation is captured on the *destination* monitor.

`key`/`type_text` require the observed monitor to contain the focused window
(`FOCUS_MISMATCH` otherwise — click the target first). Keyboard input goes
through a persistent `zwlr_virtual_keyboard_v1` device: Hyprland 0.56.2's
`hl.dsp.send_key_state`/`send_shortcut` Lua dispatchers return `ok` but
deliver nothing to Wayland-native clients (verified live). Modifiers are set
via the protocol's `modifiers` request — the path that bypasses IME
composition, verified working with an active fcitx5 Hangul IME. Bare letter
keys go through the IME exactly like a physical keyboard, so an active IME
in a composing mode will transform them; use `type_text` for text entry.

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
- Coordinates in returned images are screenshot pixels; `computer_action`
  maps them into desktop coordinates server-side (fractional scale,
  rotation, negative origins, gaps).
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
  layout; scroll uses wheel `axis_discrete` + `axis` (120/step). Keyboard
  input uses one persistent `zwp_virtual_keyboard_v1` with a default-RMLVO
  keymap uploaded at connect; keys resolve keysym→keycode against that
  keymap, so exotic layouts resolve fewer names. Both sessions bound their
  wait at 10 s; post-action settle is 150 ms.
- Held buttons, keys, and depressed modifiers are tracked and released
  best-effort on transport failure; a wedged session is dropped and rebuilt
  on the next action.
- Clipboard replacement by `type_text` is permanent (no restore).

## Verification

```sh
cargo test    # unit tests + live stdio JSON-RPC tests (need a Hyprland session)
```

The integration tests speak raw MCP over the server's stdio: initialize,
`tools/list`, `computer_monitors`, `computer_observe`, and error variants
including stale-observation, invalid-coordinate, invalid-modifier, and
invalid-key rejections for `computer_action`.

Live evidence (Hyprland 0.56.2, scale 1.25, fcitx5 IME present):

- `type_text` into gnome-text-editor (native Wayland): `첫 줄\n둘째 줄
  --dash "q" 三` verified byte-exact by selecting the buffer with Ctrl+A /
  Ctrl+C through the same virtual keyboard and reading `wl-paste` back.
- `type_text` + `ctrl_shift_v` into kitty (native Wayland terminal) and into
  an XWayland Alacritty: `echo 'IME 한글 paste'` / `echo 'X11 한글 OK'`
  pasted and executed, confirmed by the printed output.
- Modifier chords arrive with the correct mask (kitty `--debug-input` shows
  `mods: ctrl+shift`); a following `Return` arrives with `mods: none`
  (modifiers released).
- `codex exec` performed the full browser workflow: `computer_monitors` →
  `computer_observe` → click Firefox's address bar → `type_text
  "example.com"` → `key Return` → `computer_observe` showing the loaded
  Example Domain page; every action reported `effect: completed`.
- Drag: a press+move+release in gnome-text-editor produced a live text
  selection; a drag from a selected block in a gte window on HEADLESS-3 to
  an empty gte window on eDP-1 dropped the text into the destination
  document (verified visually — the destination buffer shows the dragged
  lines). A `hl.monitor({scale})` change racing a drag returned
  `outcome: partial` + `display_changed_during_action` + `do_not_replay`
  with a fresh destination observation; the button was released.
  Headless-output evidence is labelled as such — a virtual output shares the
  compositor code path but is not a physical panel.

Not verified: bare letter keys under a composing IME are transformed (by
design — same as a physical keyboard); non-US physical layouts are
untested (keysyms resolve against the uploaded default keymap).
