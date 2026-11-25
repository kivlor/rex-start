# Issue #4 — Key input through `client.key` (branch `issue-4/client-key`)

## Step 0 — recon

- `gh issue view 4`: send keystrokes and paste through `client.key` (ordered method,
  gives `init.lua` Lua bindings first chance); declare app actions via `client.keymap`;
  respect `limits.max_message_bytes` from the welcome (>= 64 KiB guaranteed, this
  server: 1,048,576).
- `docs/API-rev19.md` "Order of Requests": `client.key` is on the ordered list —
  serialized with layout/focus methods, keys arrive in typed order.
- "Message Size": a request larger than `limits.max_message_bytes` **closes the
  connection** (status 1009), so check size before sending and chunk.
- `rex-conn::hello` returns the whole welcome `Value`; `welcome["limits"]["max_message_bytes"]`.
- Actor (`rex-app/src/client.rs`): single-threaded command loop, one `Rpc::call` awaited
  at a time — naturally serialized, perfect for ordered methods.

### Live probes (session `keyprobe`)

- Multi-character `text` in one `client.key` call is accepted and arrives whole:
  `{"key":{"key":"key_a","text":"hello",...,"action":"press"}}` → `{"handled":false}`,
  `rex capture` shows `hello` on the prompt line.
- `enter` (text "", unshifted_codepoint 13) executes the pending line.
- `arrow_left` (text "", cp 0) arrives as `^[[D` movement.
- `ctrl` combo: `key_x` + mods `"ctrl"` (text "", cp 120) sends `^X` (line cancel).

### client.key schema (condensed; verbatim from `rex api describe client.key`)

Input: `{session_id (req), key (req, object), prefix?, block_id?}` where `key` =
`{key (req: W3C KeyboardEvent code in snake_case, e.g. key_a/enter/arrow_left),
mods (req: "shift+ctrl+alt+super+..." joined with +), consumed_mods (req),
text (req: the text the key typed; empty for keys that type nothing),
unshifted_codepoint (req: e.g. 110 for n; bindings like ctrl+n match on it),
composing (req), action (req: press|release|repeat)}`, additionalProperties false.
Output: `{handled: bool}` — true if a Lua function took the key, else it went to
the block. `prefix` = earlier keys of a sequence (e.g. `["ctrl+b"]` when `key` is
`x`). Without `block_id` the key goes to the session's focused block.

### client.keymap schema (condensed; verbatim from `rex api describe client.keymap`)

Input: `{actions? (the client's own actions: name req; title/description/category/
kind (client|lua|block)/args-schema/source/repeats/keywords), defaults? (built-in
layers, lowest first: name req + bindings [{keys (array), action, args?, mode?}] +
removals?), overrides? (user-recorded layers, above config + scripts),
server_actions? (all|configured|none)}`.
Output: `{bindings (req, resolved in order to try), diagnostics (req), config (req:
path/exists/generation/instance_id/hash), actions?, modes?}`.
Notes: bindings stack bottom→top: `defaults`, config file, this client's scripts,
`overrides`. A key bound to a Lua function comes back bound to `client.key.forward`
— i.e. "send it via client.key". Names with dots are client/server actions; config
file actions never contain a dot.

### `~/.config/rex/init.lua` — DOES NOT EXIST on this machine (checked 2025-…).
Step 4 will have to **create** it (flagged in the final report).

## Design decisions (Step 1)

- **Per-keystroke round trip for typed keys, coalesced chunks for paste.**
  Rationale: `client.key` is an ordered method and the actor is a serialized
  loop, so each key's response precedes the next request — typed order is
  guaranteed by construction, and a round trip per keystroke is imperceptible
  at human typing speed. Coalescing typed keys would only help programmatic
  floods, which we don't generate. Paste coalesces the other way: one
  `client.key` call per chunk (server accepts multi-char `text` — verified
  live), chunks sized to stay under `limits.max_message_bytes` (request
  oversize **closes the connection**, so margin is conservative:
  `max_message_bytes - 1 KiB`, floor 1 KiB, floor at the guaranteed 64 KiB).
- Paste is not bracketed — the official clients send paste as key text through
  the same path; programs that want bracketed paste see the characters arrive
  as if typed. (Issue says use `client.key`; `block.event` is the escape hatch
  we don't need.)
- `client.keymap` called once at startup, declaring the app's own actions
  (`app.split`, `app.zoom`, `app.picker`, `app.new_session`) plus a `default`
  layer binding ctrl+alt+* to them, so `init.lua` can rebind them and the
  keymap reports them honestly. Local ctrl-alt-* handling keeps precedence
  (the app handles them before falling through to the terminal path).

## Step 1 — plumbing (commit 6711819)

- `rex-conn`: new `pub fn chunk_utf8(text, max_bytes)` — UTF-8-safe chunker,
  shared by the app's paste and the example. A character wider than the chunk
  goes out whole (cutting it would loop forever).
- `rex-app/src/client.rs`: `ClientCmd::SendKey { key: KeyEvent, prefix }` and
  `ClientCmd::Paste(String)`; `KeyEvent { key, mods, text, unshifted_codepoint }`
  builds the `client.key` key object (action=press, composing=false,
  consumed_mods=""). Actor reads `welcome["limits"]["max_message_bytes"]`
  (default 64 KiB if absent). `declare_keymap()` runs at startup: declares
  `app.split` / `app.zoom` / `app.picker` / `app.new_session` (kind client)
  plus a `default` layer binding ctrl-alt-right/down/z/p/n to them.
- Decision (in code comment): per-keystroke round trip for typed keys
  (ordered method + serialized actor ⇒ typed order by construction); paste
  coalesces one chunk per `client.key` (server accepts multi-char `text`),
  chunk = max_message_bytes - 1 KiB (floor 1 KiB) so no request can trip the
  1009 close.

## Step 2 — integration evidence

`crates/rex-conn/examples/client_key.rs` (manual example; a live server can't
be assumed from `cargo test`). Against the local server it verified, via
`com.superlogical.terminal.format` (result field `content`, not `text`):

- PASS plain text: multi-char `text` in one call, seen twice (tty echo + cat).
- PASS multi-KB paste: 84,999 bytes, `REX_KEY_CHUNK_BYTES=8192` → 11 chunks
  (and 1 chunk with the default limit); end marker `paste-line-04999` on screen.
- PASS ctrl combo: `key_d`+`ctrl` ends cat; shell runs `echo ctrl-d-worked`.
- PASS arrow key: at a readline reader (`bash -c 'read -e x; echo GOT:$x'`),
  two `arrow_left` presses + `X` → `GOT:aXbc`. (In canonical `cat`, arrows are
  literal bytes — same as a local terminal; earlier live probes show `^[[D`
  arriving, so delivery itself is fine.)
- Session created/attached; block_id from `session.view` →
  `windows[0].focused_block_id`. `client.key` itself needs no block_id (goes
  to the focused block).
- Unit tests for `chunk_utf8` in rex-app (3) pass; sessions keytest-*/keyarrow
  killed after each run.

## Step 3 — GPUI wiring

- `app.rs`: `main_key` now handles, in order: (1) the app's own ctrl-alt-*
  bindings locally (right/down/z/p/n — they keep precedence), (2) ctrl+v →
  `cx.read_from_clipboard()` → `ClientCmd::Paste` (gpui 0.2.2 exposes no
  paste event, so ctrl+v is the paste binding; noted), (3) every other
  keystroke → `keystroke_to_key_event` → `ClientCmd::SendKey`.
- `keystroke_to_key_event`: maps gpui keystrokes to the protocol's key
  objects — W3C codes in snake_case (key_a, digit_7, minus, arrow_left,
  enter, ...), mods "shift+ctrl+alt+super" (gpui's `platform` modifier maps
  to `super`), text only for printable presses (named keys type nothing),
  unshifted_codepoint from the unshifted key so ctrl+n-style bindings match
  on layout. Chorded keys lose their text (the terminal gets the control
  code via key+mods). Function/kypad keys and bare modifiers → None
  (unhandled, for #5/#6).
- 6 new unit tests for the mapping (13 pass in rex-app total).
- Smoke (`WAYLAND_DISPLAY=wayland-0 REX_APP_DEBUG=1 … -s keyprobe`, timeout 15):
  max_message_bytes logged, "keymap declared: 5 resolved bindings" (our
  defaults), session attached, first frame 80x24 rendered, no panics.

## Step 4 — init.lua binding acceptance

**`~/.config/rex/init.lua` did NOT exist; it was CREATED for this test.**
Full content (new file):

```lua
-- rex config — CREATED NEW (this file did not exist before) for the
-- issue-4 acceptance test: a Lua key binding that fires through
-- client.key. ctrl+a then k renames the session the key was typed in.
rex.bind("ctrl+a>k", function(ctx, ev)
  rex.session.set_label{ label = "key-binding-fired" }
  return true
end)
```

- `rex config check`: bindings: 1, no diagnostics.
- `rex config reload` was needed before the live server picked it up
  (generation 2). `rex keymap` then listed `ctrl+a k  client.key.forward  init.lua:4`.
- Acceptance: created session `keybind`, sent client.key with
  `prefix:["ctrl+a"]`, key `key_k` → **`{"handled":true}`**, and the
  session's label changed to `key-binding-fired` (visible in `rex ls`) —
  the Lua function ran and consumed the key.
- The app uses the same client.key path, so GUI-typed ctrl+a k fires it
  identically (keymap bindings surface as client.key.forward, which is
  what SendKey sends).
- Cleanup: killed keybind/key-binding-fired, keyprobe, keytest-* sessions;
  no probe-stream-*.bin were created (only the two committed fixtures
  remain, untouched).

## Step 5 — final checks (all green)

- `cargo fmt --all -- --check`: pass (after one `cargo fmt --all` run; the
  diff was pure reflow of code added in step 3).
- `cargo clippy --workspace --all-targets -- -D warnings`: pass (one fix:
  elide lifetimes on `chunk_utf8`).
- `cargo test --workspace`: 27 tests pass, 0 fail (13 in rex-app incl. the
  6 new keystroke-mapping tests, 3 chunk_utf8 tests, plus the pre-existing
  view/stream/probe suites).
- Final GUI smoke with the new init.lua present: "keymap declared: 6
  resolved bindings" (5 app defaults + init.lua's ctrl+a k), window opens,
  first frame 80x24, no panics; test session killed after.

## Left for #5 / #6

- Function keys (f1-f12), keypad keys, and bracketed-paste mode
  (`unidentified` text chunks arrive as if typed; programs that behave
  differently on paste — e.g. multiline guard in shells — need the
  bracketed-paste escape, a `block.event` escape hatch).
- Mouse events ride block input per the issue; not wired.
- Keymap-driven client routing: the app could fetch the keymap and honor
  init.lua overrides of app.* actions (today init.lua rebindings of
  ctrl-alt-* reach the server but the app still handles them locally
  first).
- `prefix` (key sequences) is plumbed through ClientCmd but the UI never
  builds one; it exists for when the app starts waiting on sequences.

## Review fix — paste chunks sized by JSON-encoded length

Review found a defect in `Actor::paste`: chunks were sized by **raw UTF-8
byte count** (`max_message_bytes - 1 KiB`), but the request is JSON and
serde_json escapes `"`, `\` and control characters — `\n` doubles to 2
bytes, other control bytes become 6-byte `\u00XX`. A newline-heavy paste
expanded past `limits.max_message_bytes` (600 KiB of newlines → ~1.2 MB
payload), and the server closed the control connection with 1009
(docs/API-rev19.md "Message Size"), disconnecting the whole app. On a
64 KiB-floor server it failed even sooner. The earlier live test (85 KB
plain ASCII) passed only because ASCII needs no escaping.

### Fix

- `rex_conn::chunk_utf8` replaced by `rex_conn::chunk_json_safe(text,
  max_json_bytes)` (crates/rex-conn/src/lib.rs): greedy accumulation per
  character, counting each at its JSON string-literal cost (2 for `"`,
  `\`, `\n`, `\t`, `\r`, `\b`, `\f`; 6 for other control chars < 0x20;
  UTF-8 length otherwise, plus the surrounding quotes). A single
  character larger than the whole budget still goes out alone (no
  infinite loop). Nothing else used `chunk_utf8`, so it's gone.
- `Actor::paste` keeps the 1 KiB envelope margin but now budgets by
  encoded length; `examples/client_key.rs` chunking follows, with a new
  `REX_PASTE_NEWLINES` mode that pastes a newline-heavy blob (the
  regression case) and polls for the end marker (a very large echo
  takes cat more than one fixed 400 ms sleep).

### Evidence

- Unit: new `chunk_json_safe_*` tests in rex-conn (5) and rex-app (4),
  including `chunk_json_safe_paste_stays_within_budget` — a 600 KiB
  newline + control-char blob chunks so every chunk's
  `serde_json::to_string` stays under the 1 MiB-minus-margin budget.
- Live (local Rex server, 1 MiB limit): plain run passes; then
  `REX_PASTE_NEWLINES=614400` — 614421 raw bytes (~1.23 MB JSON) sent
  as 2 chunks, all accepted, no 1009, connection stayed up through the
  later ctrl+d / arrow-key checks, end marker present on screen.
  Cleanup: `rex kill keytest-700926 keytest-701514`; no
  probe-stream-*.bin created.
