# Keymap notes

## Why the defaults changed

The first cut bound splits to `ctrl+alt+right/down`, zoom to
`ctrl+alt+z` — chords GNOME (mutter, Wayland) grabs for workspace
switching before the app ever sees them, so splits silently did
nothing. The new defaults follow the official Mac client's muscle
memory, translated the way the prior-art Linux client
(willothy/rex-linux) translates it: `cmd+d` splits right →
`ctrl+shift+d`, `cmd+shift+d` splits below → `ctrl+shift+e`, zoom →
`ctrl+shift+z`, and pane focus moves on `ctrl+shift+arrows` (verified
ungrabbed on this machine). The picker moved to `ctrl+shift+p`
(cmd+shift+p muscle memory, per user request); `ctrl+alt+n` (new
session) never conflicted and stays.

## Canonical action names

`client.keymap` now declares the server's canonical names —
`pane.split.right`, `pane.split.down`, `pane.zoom`,
`pane.focus.left/right/up/down` (plus our own `app.picker`,
`app.new_session`) — instead of the old `app.split`/`app.zoom`.
init.lua written for the Mac client's action names rebinds ours with no
changes. The directions live in the action names, not binding `args`,
exactly as rex-linux declares them. A live `client.keymap` round trip
against the local server returns all 9 bindings verbatim with no
diagnostics; the server carries no built-in defaults of its own for
`pane.*` (the only extra resolved binding is the user's init.lua
sequence).

## How the flow works

- `declare_keymap` (client.rs) sends the 9 actions + 9 default
  bindings; the server stacks init.lua on top and returns the resolved
  set. `resolved_local_keys` keeps only the defaults still bound to OUR
  actions and publishes them as `ClientEvent::Keymap`.
- The UI (`main_key` in app.rs) handles exactly those keys locally:
  the `ctrl+shift` family (splits, zoom, focus) and the `ctrl+alt`
  family (picker, new session). Anything rebound or removed flows to
  `client.key`, where a Lua binding actually fires.
- Spelling: both sides canonicalize to the protocol's mod order
  (shift, ctrl, alt, super), so the internal name for `ctrl+shift+d`
  is `shift+ctrl+d` — `normalize_key` (client.rs) and `binding_key`
  (app.rs) agree.
- Focus movement: `focus_neighbor` (app.rs) picks the nearest pane
  that lies wholly in the pressed direction and overlaps the focused
  pane's rect on the perpendicular axis, using the server's placement
  fractions; a step off the layout (or a zoomed single-pane window) is
  a no-op, then the actor's existing `session.focus_block` does the
  move.
- `ctrl+shift+v` is deliberately NOT handled locally — rex-linux binds
  it to `pane.paste`, we have no paste action, so it flows to
  `client.key` like any other chord. Plain `ctrl+v` still reads the
  clipboard and pastes through `client.key` text.
