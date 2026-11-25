# Issue 5 — Resize ownership: `session.own_block` and grid negotiation

Branch `issue-5/own-block`, stacked on `issue-4/client-key` (PR #9 unmerged).

## Step 0 — recon

### Issue #5 spec (gh issue view 5)

Claim ownership of visible blocks and negotiate their grid size so the
server sizes programs for *this* window. While a terminal has an owner, it
takes size (and colors) from that client. `session.own_block` is on the
ordered methods list. Ownership is exclusive; expect `block_owner_changed`
plus a followup `resize` rex1 on the data stream. `resize` rex1 carries
`cell_width_px`/`cell_height_px` (0 when unknown) for cross-checking.
Acceptance: window resize → matching `resize` rex1 + programs reflow;
another client watching sees `block_owner_changed` not a garbled grid;
releasing hands sizing back without killing the block.

### Live schema: `rex api describe session.own_block` (verbatim)

```
Claim blocks for the calling client. The result lists each block's owner.

Method: session.own_block

Input schema:
{
  "type": "object",
  "properties": {
    "block_ids": {
      "type": [ "null", "array" ],
      "items": { "type": [ "string", "null" ] },
      "description": "The blocks to claim. Leave it out or empty to claim every block in the session."
    },
    "mode": {
      "type": "string",
      "description": "How to claim them. 'take' makes the calling client the owner right away. 'request' makes it the owner only of blocks that have none, and it may get the rest when their owner gives them up or leaves. Defaults to take."
    },
    "session_id": { "type": "string", "description": "The session to act on." }
  },
  "required": ["session_id"],
  "additionalProperties": false
}

Output schema:
{
  "type": "object",
  "properties": {
    "blocks": {
      "type": [ "null", "array" ],
      "items": {
        "type": "object",
        "properties": {
          "block_id": { "type": ["string","null"], "description": "The block's ID." },
          "owner": { "type": ["string","null"], "description": "The client that owns the block. Left out when the block has no owner." }
        },
        "required": ["block_id"],
        "additionalProperties": false
      },
      "description": "Each claimed block and its owner after this call."
    }
  },
  "required": ["blocks"],
  "additionalProperties": false
}

Request example:  { "session_id": "session:<uuid>", "block_ids": ["block:<uuid>"], "mode": "request" }
Response example: { "blocks": [ { "block_id": "block:<uuid>", "owner": "client:<uuid>" } ] }
```

Validation notes (live, abridged): the caller must be attached
(`session.attach`/`session.bootstrap`, not just `attach_all`); either mode
puts the caller in line — when the owner gives up/detaches/drops, the block
passes to the in-line client that claimed most recently, `take` ahead of
every `request`; an owner whose connection drops gets blocks back on
reconnect (`restore`) unless the owner changed again; every change of owner
sends `block_owner_changed` (and `session_view_changed`, because
`session.view` shows owners).

### Release form: `rex api describe session.disown_block` (verbatim, key parts)

```
Give up the calling client's claim on blocks. The result lists each block's owner.

Input schema properties:
  block_ids (array of strings or nulls, or null): The blocks to give up.
      Leave it out or empty to give up every block in the session.
  session_id (string, required): The session to act on.
```

So release = `session.disown_block` with `session_id` (+ optional
`block_ids`, empty/all when omitted). Both are **ordered** methods (API doc
"Order of Requests" list includes `session.own_block` and
`session.disown_block`), so they serialize with layout/focus calls — a
natural fit for the actor's one-at-a-time command loop.

### `docs/API-rev19.md` findings

- "How Rex Is Organized" (line 37): a client claims a block with
  `session.own_block` and becomes its **owner**; while a terminal has an
  owner, it takes its size and colors from that client.
- `com.superlogical.terminal.resize` (line 5551): `args.columns`/`args.rows`
  (1–65535, required), optional `cell_width_px`/`cell_height_px` (1–65535,
  sent together or both left out). "It applies at once if the caller owns
  the block or no client does, and otherwise when the caller becomes the
  owner" — i.e. we own first, then resize.
- `resize` rex1 message (line 643): cols/rows + `cell_width_px` /
  `cell_height_px` (0 when unknown), `owner` = block's owner when the size
  was set, or null. One right after the snapshot, another each time the
  size changes; an owner change alone sends none.
- `block_owner_changed` (line 1078): fields `block_id`, `owner` (new,
  left out when none), `previous`, `reason` ∈ {create, take, request,
  disown, lost, disconnect, restore}. Session event — arrives nested under
  `session_event` (per PR #8 finding; client.rs already unwraps
  `payload["payload"]["event"]`).
- `session.view` shows owners (validation note above) — so our `BlockRef`
  should parse an `owner` field to know who owns what without extra calls.

### Code layout

- `crates/rex-app/src/client.rs` — actor: `ClientCmd` enum, one-at-a-time
  command loop, `session_event` unwrapping already present.
- `app.rs` — GPUI window; panes get pixel rects from server fractions ×
  viewport; no window-activation hook yet (GPUI: `window.on_window_activation`
  / app `cx.activate`).
- `terminal.rs` — `TerminalElement` stretches a server-given frame to the
  pane bounds; **no cell metrics today** (font size derived from cell_h at
  paint time). Natural cell size to derive/store: the non-stretch path uses
  9.0 × 18.0 px per cell — that's the design cell.
- `view.rs` — parses `session.view`; `BlockRef` has no `owner` field yet.
- `rex-conn` — `Rpc` usable from examples (`Rpc::call`), plus
  `data_ticket`/`open_data` for streams; `client_key.rs` is the example
  pattern to follow for step 3.

### Prior art `willothy/rex-linux` (read from /tmp/rex-linux)

README policy: "Resize ownership: the client takes the blocks it shows when
its window is activated or typed into, and asks for grids that fit each
pane." Implementation (`crates/rex-app/src/app.rs` ~1110): `claim()` calls
`session.own_block` mode take, skipped when the view says we already own;
`claim_visible()` on session switch/activation/typing; per-block resize
requests coalesced by remembering the last requested
`(cols, rows, cell_w, cell_h)` tuple and skipping repeats.

### Design for this repo (steps 1–2)

- Ownership taken: window activated (GPUI activation hook in app.rs), focus
  lands on a pane (mouse click path already sends FocusBlock), and when
  typing (SendKey) — matching the prior-art policy. Grid: cols/rows from
  pane pixels ÷ cell metrics, re-requested via
  `com.superlogical.terminal.resize` only when the tuple changes
  (coalescing, like rex-linux's `pane.requested`).
- Ownership released: `session.disown_block` on window deactivation, and on
  actor end (channel closed = window closed) before the sockets drop —
  the API's `disconnect`/`lost` reasons would also cover us, but an explicit
  disown is the documented "hand sizing back without killing the block".
- Exclusivity: track per-block owner from `session.view`'s new `owner` field
  and `block_owner_changed` events. Don't fight: if `block_owner_changed`
  says another client took a block while we thought we owned it, mark it
  foreign, stop resizing it, surface a notice. Our own claim confirmations
  update the map. (own_block take doesn't error when someone else owns —
  it *wins* — so "back off" is driven by the event/`previous` field, not an
  RPC error; but we still handle an RPC error gracefully with a notice.)

## Step 1 — ownership plumbing (notes)

ClientCmd::OwnBlock { block_id, cols, rows, cell_w, cell_h } — take
ownership, then `com.superlogical.terminal.resize` with the grid (the
resize applies at once since we now own). ClientCmd::DisownAll — release
every claim in the current session. Actor tracks per-block
`owner: Option<String>` (updated from view + `block_owner_changed`), a
`last_resize: HashMap<block_id, (u32,u32)>` for coalescing, and skips
owning blocks already owned by *another* client unless we're told to take
(Take back? No: default is take; but after a foreign takeover event we
mark foreign and don't re-take until our own activation/focus says the user
is here). On actor end: best-effort `session.disown_block` before the
socket drops.

app.rs: `window.on_window_activation` (GPUI 0.2 has
`WindowActivation::Active/Inactive`) → on Active send OwnBlock for the
focused pane, on Inactive send DisownAll. Focus (mouse click → FocusBlock)
also sends OwnBlock. Typing already goes through SendKey; owning on every
keystroke would be chatty — we own on activation/focus only (covers
"typed into" because you must focus to type), like the prior art's
claim-on-activation plus focus click.

Cell metrics: terminal.rs's natural (non-stretch) size is 9×18 px per cell
— stored as consts CELL_W_PX/CELL_H_PX there and exported for grid math.

## Step 2 — grid computation (notes)

`terminal::grid_for(pane_w, pane_h)`: floor(w/9), floor(h/18), clamped to
[1, 65535]. Tests cover exact division, flooring remainders, sub-cell /
zero / negative panes → 1x1, the protocol max, and an end-to-end split-pane
share of a 1100x694 window (547x665 px → 60x36).

Cross-check: `rex_terminal::Fed` now reports the last `resize` rex1 of a
feed as `(cols, rows, cell_w_px, cell_h_px)`; `stream_block` debug-logs a
comparison against our 9x18 metrics when the server's cell px is nonzero
— a match logs "matches our cell metrics", a mismatch logs "someone else
owns the grid?". Deliberately not a hard assert: cell px can be 0/unknown
and the owner may be another client.

## Step 3 — evidence (filled in after live runs)

(to be recorded)

## Step 4 — GUI smoke (filled in after live runs)

(to be recorded)

## Step 5 — final checks

(to be recorded)
