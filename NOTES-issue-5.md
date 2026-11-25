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

Example: `crates/rex-conn/examples/own_block.rs` (pattern of client_key.rs).
Run: `mise exec -- cargo run -p rex-conn --example own_block` (full output in
the commit message and reproduced here). Live run 2026-11-25, server pid
revision 19:

```
client 1 connected
session session:01a210ed-5af0-744e-96c6-a6a417d69721
client 1 is client:01a210ed-5af0-7036-80d0-9019558a6e6c
block block:01a210ed-5af0-7545-ad6f-49bceab60f20
own_block 40x12: owner=client:01a210ed-5af0-7036-80d0-9019558a6e6c
PASS resize rex1 says 40x12 at 9x18px
PASS stty size reports "12 40" (rows cols) for 40x12
own_block 100x30: owner=client:01a210ed-5af0-7036-80d0-9019558a6e6c
PASS resize rex1 says 100x30 after re-own
PASS stty size reports "30 100" (rows cols) for 100x30
client 2 took the block; owner now client:01a210ed-5e7e-7bd1-a0f4-ca8754fa6f04
client 1 saw block_owner_changed: owner=client:01a210ed-5af0-7036-80d0-9019558a6e6c previous=none reason=take
client 1 saw block_owner_changed: owner=client:01a210ed-5e7e-7bd1-a0f4-ca8754fa6f04 previous=client:01a210ed-5af0-7036-80d0-9019558a6e6c reason=take
PASS client 1 saw block_owner_changed (exclusivity, no fight)
PASS block followed client 2's grid: resize rex1 says 66x22
PASS stty size reports "22 66" (rows cols) from client 2's grid
client 1 saw block_owner_changed: owner=client:01a210ed-5af0-7036-80d0-9019558a6e6c previous=client:01a210ed-5e7e-7bd1-a0f4-ca8754fa6f04 reason=disown
PASS disown kept the block alive; grid now 100x30 (our deferred resize applied)
PASS client 1 still sizes the block after handback: "16 50"
all checks passed; session destroyed
```

Findings recorded along the way:

- Grid reflow verified twice (40x12 then 100x30), each two ways: the
  `resize` rex1 on the data stream (cols/rows + 9x18 cell px echo) and the
  program's own `stty size` via `com.superlogical.terminal.format`.
- `block_owner_changed` seen by client 1 when client 2 took the block
  (owner=client2, previous=client1, reason=take), and the block's grid
  followed client 2's 66x22, verified both ways again.
- `session.view` does NOT report block owners (verified live earlier with
  ownprobe/ownprobe2), despite the doc's "session.view shows owners" note —
  owners are tracked from own/disown results and events.
- Deferred-resize semantics confirmed: a `terminal.resize` sent while
  another client owns applies "when the caller becomes the owner" — our
  100x30 request applied the moment client 2 disowned and the block passed
  back to us (the in-line `take` claimer). The first draft of the example
  mistook this for a bug; it's documented behavior (API doc, resize notes).
- The example runs the data-stream replica on a LocalSet (libghostty
  iterators are !Send), like the app's actor.
- Sessions destroyed by the example itself (`session.destroy`); the two
  manual probe sessions (ownprobe, ownprobe2) killed with `rex kill`.
- Cell-px cross-check number from the live server: resize rex1 reported
  9x18px, exactly the `CELL_W_PX`/`CELL_H_PX` metrics we sent.

## Step 4 — GUI smoke (live run)

```
WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 REX_APP_DEBUG=1 \
  mise exec -- cargo run -p rex-app -- -s gui-own   # under `timeout 15`
```

Debug log (verbatim, /tmp/gui-own.log):

```
[rex-app] bootstrap: 1 sessions, 1 attached
[rex-app] showing session session:01a210ed-b7e9-7ecf-81b9-846f1cd44f04 (revision 1)
[rex-app] block block:01a210ed-b7ea-70a8-ad60-f053507483e1: resize 80x24 at 8x16px differs from our 9x18px (someone else owns the grid?)
[rex-app] block block:01a210ed-b7ea-70a8-ad60-f053507483e1: first frame 80x24
[rex-app] block:01a210ed-b7ea-70a8-ad60-f053507483e1 renders: kivlor@kyrios:~/Code/rex-start$
[rex-app] own_block block:01a210ed-b7ea-70a8-ad60-f053507483e1 at 121x38: owner=client:01a210ed-cdd7-7ddb-9122-7da7701d7d6f
[rex-app] block block:01a210ed-b7ea-70a8-ad60-f053507483e1: resize 121x38 at 9x18px matches our cell metrics
[rex-app] resize block:01a210ed-b7ea-70a8-ad60-f053507483e1 to 121x38
[rex-app] block_owner_changed: ... owner=Some("client:01a210ed-cdd7-...") previous=None reason=take
[rex-app] view changed to revision 2, refreshing
[rex-app] refreshed view to revision 2
```

What this shows:

- **own_block fired on window activation** with the fitting grid 121x38:
  the 1100x720 window minus the 26px header and 3px gutter is
  1097x691 px → floor(1097/9)=121, floor(691/18)=38. Exactly grid_for's
  math (and its split-pane unit test).
- **The resize rex1 arrived** at 9x18px — the cell-px cross-check branch
  logged "matches our cell metrics". The pre-claim snapshot's resize rex1
  (80x24 at 8x16px, the server default) exercised the mismatch branch.
- **block_owner_changed** handled (reason=take, view refreshed).
- **No panics**; the run ended at `timeout`'s SIGTERM (exit 124).

Ownership after the window closed (timeout kill is not a graceful close,
  so the server's `lost` rule — not our disown — released the claim):

- `rex capture --session gui-own` still showed the live prompt: the block
  survived, at the 121x38 grid we negotiated.
- From a second control connection, `session.own_block` with mode
  `request` — which only claims blocks that have **no owner** — succeeded
  and reported the CLI client as the new owner: proof the dead app client
  no longer owned it. `client.list` showed the app client with
  `connection_state: lost`. A `session.disown_block` handed it back to
  no-owner and the block stayed alive.
- The graceful-close path (window X → command channel drops → actor's
  end-of-loop `disown_all` before the socket closes) is wired in code but
  was not exercised live: SIGTERM kills the whole process, and I have no
  tool on this Wayland session to click the window's close button or
  drive the WM to resize. Noted as a leftover for #6 (add a close key /
  integration hook so the graceful path can be driven headlessly).

Could not do the visual htop reflow check: no way to drive the WM resize
  here. The resize path is wired — every render recomputes each pane's
  grid via grid_for and re-owns only on change — and the same
  own/resize/reflow sequence was verified end-to-end live by the step-3
  example (resize rex1 + stty size at two grids).

Test sessions cleaned up: `rex kill gui-own` (plus ownprobe/ownprobe2 and
the two owntest-* labels from the example's earlier failed runs). No
probe-stream-*.bin files were generated; the two committed fixture .bin
files untouched.

## Step 5 — final checks

- `mise exec -- cargo fmt --all -- --check` — clean.
- `mise exec -- cargo clippy --workspace --all-targets -- -D warnings` —
  clean (only the pre-existing proc-macro-error2 future-incompat note
  from the dependency tree, unrelated to this branch).
- `mise exec -- cargo test --workspace` — green: 19 (rex-app) + 13
  (rex-terminal) + 5 (rex-conn) + 1 (probe) = 38 passed, 0 failed.
- Cleanup: no test sessions remain (`rex ls` empty); no probe-stream-*.bin
  generated; the two committed fixture .bin files untouched.

## Review feedback (PR #10) — designs and decisions

Four non-blocking review items, addressed in one commit.

### 1. A failed claim is retried, not stuck

The bug: `claim_panes` cached its grid in `own_requests` *before* the
actor ran the claim, so a failed `session.own_block`/resize RPC left the
pane rendering at the server's old grid until the grid changed or the
window reactivated. Fix: new `ClientEvent::OwnFailed { block_id, reason }`.
When the actor's `own_block` fails, `command`'s OwnBlock arm sends it
(plus the usual `Notice`, which puts the reason in the status bar);
app-side, `on_client_event` removes the `own_requests` entry for that
block, so the next render — which recomputes the fitting grid every
frame — retries. One event carries both the reason and the retry
trigger; no new state, no timers.

### 2. One pane-pixel helper

The pane pixel math lived twice, in `claim_panes` and `render` — drift
would have mis-sized the claimed grid against the drawn pane. Now one
free function, `app::pane_pixels(viewport, rect)`: rect fractions of the
viewport minus the 26px header, minus the 3px gutter, floored at 1px.
Both call sites use it; a unit test pins the header/gutter arithmetic
and the 1px floor.

### 3. `disown_all` guard tightened

Old guard: `!ours && last_grid.is_empty()`. A client owning nothing but
holding a stale `last_grid` entry still sent a pointless disown round
trip. Every path that grants us a block (own_block results,
`block_owner_changed`) records it in `owners`, and `set_current` clears
both maps on a session switch — so "we own nothing" is a complete skip
condition: `if !ours { return }`. The comment in the code says why that's
safe.

### 4. A block handed back to us re-claims immediately

The gap: a foreign taker's disconnect (`lost`) or detach (`disconnect`)
passed the block back to us (we were in line), but nothing re-claimed
our grid — the pane rendered at the taker's grid until reactivation.
Decision: in the `block_owner_changed` handler, when `owner == us`,
`previous != us`, and the reason is NOT `take`/`request` (those are our
own claims, which `own_block` already handles — excluding them avoids a
spurious clear/re-claim churn on every own), the actor removes the
block's `last_grid` entry (so its coalescing can't swallow the retry)
and sends a new `ClientEvent::BlockReturned(block_id)`. App-side, the
handler removes the `own_requests` entry; the next render — window
active, frames arriving — re-runs `claim_panes`, which re-claims via
`OwnBlock` at our current fitting grid, and the actor resizes since its
`last_grid` is empty. Payload shape confirmed from `docs/API-rev19.md`
§`block_owner_changed`: `block_id`, `owner` (new, left out when none),
`previous`, `reason`. `owner == None` with `previous != us` is left
alone: we held no claim on it, so there is nothing of ours to restore.

### 5. New unit tests

- `record_owners` (was an Actor method, now an associated fn taking the
  owners map, so it's testable without an actor): owner set from an
  own_block result, owner cleared when left out (disown shape), the
  `block_ids` filter skipping unlisted blocks while keeping their
  entries, and malformed/missing `blocks` skipped without harm.
- `Fed.resize` parsing (`rex-terminal`): a crafted resize APC through a
  real `Replica` — cell pixels flow through, missing fields default to
  0, last resize of a feed wins, the terminal clamps to 1x1 while `Fed`
  reports the raw grid, and feeds without a resize report `None`.

## Left for #6

- The graceful-close path (window X → actor's end-of-loop `disown_all`)
  is wired but not headlessly drivable on this Wayland session: no tool
  here to click the title bar or drive the WM. A close keybinding or
  integration hook would let it be exercised end-to-end.
- Visual htop reflow check under a real WM resize — same blocker; the
  resize path is wired via render-driven `claim_panes` and reflow was
  verified programmatically (step 3).
- Multiple windows of ours on one session: the model tracks one current
  session and claims its active window's panes; a second window of ours
  showing the same session would re-own blocks between the two — the API
  allows only one owner, so a future #6 could coordinate (e.g. only the
  active window claims).
- Colors: while a terminal has an owner it takes colors too
  (`com.superlogical.terminal.set_theme`); ownership plumbing now in place
  makes that a small follow-up.
