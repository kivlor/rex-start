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

## Review feedback round 2 — AltGr, keymap honesty, paste shift

### 1. AltGr: how characters reach the block on this layout

Reviewer's question: "how do AltGr characters reach the block on your
layout?" — investigated at three levels, no system settings touched:

**gpui 0.2.2 source** (X11 and Wayland share `Keystroke::from_xkb` in
`src/platform/linux/platform.rs`): `key_char` is xkbcommon's
layout-resolved UTF-8 for the current modifier state (`key_get_utf8`),
and `modifiers.alt` is Mod1-effective. gpui's own editors filter chorded
`key_char` out downstream ("only allow shift modifier when inserting
text"), which confirms `key_char` is `Some` even for plain alt chords.

**libxkbcommon probe** (small C program, /tmp, no settings changed):
- `de` default: Alt_R is `ISO_Level3_Shift` → Mod5=1, **Mod1=0**;
  AltGr+q → utf8 `@`, AltGr+e → `€`. gpui reports `alt: false`,
  `key_char: Some("@")` → the keystroke is NOT chorded and our existing
  plain-text path delivers it. **On standard Linux layouts (the common
  case) AltGr characters already reached the block; the chorded-strip
  concern doesn't apply.**
- `de` + `lv3:ralt_alt`: Alt_R is a plain Alt (Mod1=1) and the layout
  composes nothing (utf8 is just `q`) — this option intentionally turns
  AltGr off; nothing is lost.
- `de` + `lv3:lalt_switch` (the reviewer's scenario made real: level 3
  on Mod1): alt=1 AND utf8 = the composed char. Only this opt-in family
  of XKB options trips the chorded path.

**Live protocol probe** (throwaway `altgr_probe` example, deleted after;
sessions destroyed): with a `cat -v` reader,
- `mods:"alt", text:""` → `^[q` (the alt chord's ESC prefix);
- `mods:"alt", text:"q"` → `^[q` — the block applies the alt chord even
  when text is present, so naively keeping text would still swallow the
  character;
- `mods:"alt", consumed_mods:"alt", text:"@"` → `@` typed — the schema's
  `consumed_mods` ("modifiers the keyboard layout used to produce
  text") is the designed channel for exactly this.

**Fix** (`keystroke_to_key_event`): with alt held and ctrl/super not,
the keystroke is text-producing when the layout composed a character —
the composed char either differs from the keystroke's key name (gpui
falls back to the base key's letter for non-ASCII: key `"q"`,
key_char `"€"`) or is itself not an ASCII alphanumeric (German AltGr+q:
keysym IS `@`, so key == key_char == `"@"`). Then we send
`text = key_char` with `consumed_mods: "alt"` (mods kept, so
server-side bindings still see the chord — live-verified to type the
char). A plain alt chord (US alt+q: key `"q"`, key_char `"q"`;
alt+shift+s: key `"s"`, key_char `"S"`) stays chorded and keeps typing
nothing — the reviewer's literal "key_char is Some while alt is held"
rule would have converted every plain alt+letter into text, and the
live probe shows the block would then drop the ESC-q chord. Known
residual: a plain-alt+punctuation chord (Alt+.) is indistinguishable
from an AltGr composition at the Keystroke level and will type its
character; accepted because Mod1-mapped level 3 is an opt-in config and
Alt+punct chords are rare (both facts in code comments).

Couldn't verify live: a real AltGr keyboard press end-to-end (this
machine is US layout); the path was verified piecewise instead —
xkbcommon state → gpui keystroke fields (source + probe), keystroke →
key event (unit tests), key event → typed char (live probe).

### 2. ctrl+v paste

`main_key` now requires shift NOT held: ctrl+shift+v flows to the
server like any other chord.

### 3. init.lua rebinds of app.* are now honored

Design: after the `client.keymap` declaration, the RESOLVED bindings
(`bindings` in the result, in try order) decide which of our five
declared defaults are still OURS:
- a binding for the key with action `client.key.forward` (what a Lua
  binding reports as) always wins per the docs — key leaves our set;
- a key bound to another action, or removed, leaves the set;
- sequences (keys length > 1) and mode-scoped bindings wait on the
  server — not claimed locally.
Pure helpers in `client.rs`: `normalize_key` (canonical binding
spelling: mods in protocol order, lowercase, cmd→super — unit-tested)
and `key_is_ours`/`resolved_local_keys` (unit-tested against fresh,
rebound, sequence, mode, and empty binding lists). The actor sends the
set to the UI as `ClientEvent::Keymap(HashSet<String>)`; on a failed
`client.keymap` call it sends the full default set (the UI also STARTS
with the full set, so keys work before the event arrives) and
debug-logs. The UI keeps `ours` and its `main_key` handles a ctrl-alt
key locally only when `binding_key(keystroke)` (same spelling, from
GPUI modifiers + lowercase key) is in the set — everything else flows
to `client.key`, where a Lua-bound key hits `client.key.forward`.
`keymap_changed` is handled in the actor: the event's fields sit one
payload level down (`payload.payload.hash` — found by logging the raw
event live); a hash different from the last `client.keymap` result's
`config.hash` re-declares and re-publishes the set, the same hash is a
no-op.

Live evidence: GUI smoke logged `keymap: 6 resolved bindings; still
ours: {ctrl+alt+down, ctrl+alt+n, ctrl+alt+p, ctrl+alt+right,
ctrl+alt+z}` (init.lua's ctrl+a k is the 6th, a sequence, correctly
not ours); `rex config reload` with an unchanged file → no refetch
(hash equal); with a changed file → "keymap changed (hash …),
re-fetching" and a fresh set. init.lua was restored byte-identical
after the test (created by the earlier acceptance run, unchanged
content).

Known limits (honest, documented): a Lua rebind of app.* to a key the
UI doesn't know locally still won't perform the action (the server
would need `client.perform` plumbing, out of scope here) — but it now
at least stops the UI from swallowing the key, and the default key
becomes a server-side binding.

### 4. `prefix`

Kept the plumbing (`ClientCmd::SendKey { prefix }` → `client.key`'s
`prefix`), with a doc-comment saying it is unused pending sequence
support (#5/#6). It is not used anywhere else, so removal was the only
alternative; kept because the protocol field is the designed hook for
sequences and the client-side "wait for the next key" work lands next.

### 5. duplicate chunker tests

Removed from rex-app's test module (pure duplicates of rex-conn's own
suite, calling `rex_conn::chunk_json_safe` directly):
`chunk_json_safe_splits_on_char_boundaries`,
`chunk_json_safe_empty_and_huge`,
`chunk_json_safe_wide_char_in_narrow_chunk`. Kept the rex-app-only
budget/property test `chunk_json_safe_paste_stays_within_budget`.
Replaced by the new `normalize_key` / `resolved_local_keys_honor_…` /
`default_local_keys_…` tests (client.rs) and the AltGr + `binding_key`
tests (app.rs). rex-app: 19 tests, workspace all green.
