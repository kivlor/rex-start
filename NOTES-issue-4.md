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
