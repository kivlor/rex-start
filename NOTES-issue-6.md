# Issue 6 — reconnect and resume (working notes)

Milestone 6 of 6. Stacked on `issue-5/own-block`.

## Step 0 — recon

### Spec (issue #6 + docs/API-rev19.md)

Control connection:
- `hello` may carry `reconnect_token` from the last welcome; the new welcome
  has `resumed: true` and a NEW token (keep the newest, never log it). Bad
  token => new client, `resumed: false` — client id only survives if accepted.
- Server keeps a dropped client for 60s. Events while away are lost; `seq`
  restarts at 1 on the new connection. After reconnect: `session.attach` again,
  refresh view, re-claim owned blocks ("restore": an owner whose connection
  drops gets its blocks back on reconnect unless the owner changed since —
  line ~4485). Seq rule ("Combining Results With Events"): a response's `seq`
  is the last queued event's seq; skip events with seq <= response seq.
- Server pings every 120s; 3 unanswered pings => close 1001 `keepalive
  timeout`. tungstenite answers pings itself, but a dropped socket shows up
  as `ws.next()` => None or Err.

Data connection:
- Stream outlives its connection: server holds it ~60s (the WELCOME carries
  the retention ms) with output retained.
- RESUME = 25 bytes: `0x02` + 16-byte latest reconnect token + u64 LE count
  of stream bytes received. No ticket, no control connection needed.
- WELCOME = 32 bytes: `0x81`, 16-byte NEW token, 3 zero bytes, u32 LE
  retention ms, u64 LE channel nonce (same nonce on resume).
- REJECT = 2 bytes: `0x82` + reason:
  - 0x01 unknown/expired/used/mis-issued token
  - 0x02 client already has a connection to this data source — if the old one
    dropped, send RESUME, which replaces it
  - 0x03 stream gone (block closed) OR RESUME counted FEWER bytes than sent
  - 0x05 malformed/late handshake OR RESUME counted MORE bytes
  - 0x06 server failed; 0x07 wrong subprotocol; 0x04 limit (unused)
  - unknown reason = failure. After a refused RESUME, start over: new
    `data.connect` ticket + HELLO (which ends the waiting stream).
- Close statuses: 1000 = stream ended (done); 1001 keepalive = resume;
  1001 data provider canceled = server stopped it (shut down or opener left)
  — resume-worthy but may reject; 1008/1011 = provider gone/failed;
  **1013 `data reconnect required`** = start over, new ticket + HELLO (also
  when retained output overflowed); **4000 `block closed`** = stop, never
  reconnect. 1002/1008/1011/1013 with reason `data handshake rejected` = a
  REJECT was sent (parse the reason).
- Counting rule: count every stream byte USED across ALL connections; not the
  WELCOME; not read-ahead. With tungstenite each `ws.next()` Binary payload
  we process is exactly the bytes we used (no read-ahead is exposed), so
  counting at consumption is correct — but it must live in ONE authoritative
  place.

### Current code (what I'm extending)

- `rex-conn`: `connect`, `hello(ws, client) -> (Ws, welcome)`, `Rpc::call`
  (events to a callback), `data_ticket`, `open_data(path, ticket) ->
  (Ws, nonce)` — WELCOME parsed only for the nonce; the reconnect token
  (bytes 1..17) is dropped today. `hello` has no reconnect_token arm.
- `rex-app/client.rs`: `stream_block(socket, ticket, block_id, events)` is a
  single-connection loop: open_data, feed Replica, publish frames; any close
  just ends the task (no reconnect). The actor's `run` breaks the main loop
  on control errors and sends `ClientEvent::Closed(reason)`; app.rs shows
  "disconnected: …" and drops the client. `sync_streams` fetches tickets
  inline in the actor and hands each stream task its ticket — a stream task
  has no way to ask for a NEW ticket (start-over path needs one).
- Retention window: WELCOME bytes 20..24 give the ms (60s default).

### Design decisions

- **Counter**: one struct per stream, `rex_conn::StreamResume { bytes: u64,
  token: [u8;16] }`, owned by the stream task; `consume(len)` called exactly
  once per processed Binary payload. Token kept in a `ReconnectToken` wrapper
  whose `Debug` prints `[redacted 16 bytes]` so no accidental log can leak it.
- **Stream loop** (client.rs): consume → on close/drop classify: 4000 or
  clean-1000 → stop; everything else → RESUME with latest token+count (0x02
  reject → RESUME again; 0x01/0x03/0x05 → new ticket+HELLO, fresh Replica;
  1013 close → same start-over). New tickets requested from the actor over an
  mpsc so `Rpc` stays single-owner.
- **Control reconnect**: `run` keeps the newest welcome's
  `client.reconnect_token`; on drop, retry hello-with-token with backoff
  (0.5s→5s cap, ~bounded 3 min), emit `Reconnecting`/`Reconnected` events;
  after success: re-attach current session, refresh view, re-own blocks we
  still hold (own_block is coalesced so repeats are cheap). Give-up →
  `Closed`.
- Example `resume.rs` proves: 3 clean resumes with exact counts; wrong count
  +1 → REJECT 0x05, −1 → REJECT 0x03; control drop → hello with token →
  same `client.id`, `resumed: true`.

Probe plan: live server at $XDG_RUNTIME_DIR/rex/server.sock; probes during
the example run.

## Step 1 — rex-conn plumbing (commit)

- `hello_with_token(ws, client, Option<&str>)`: hello carrying the last
  welcome's `reconnect_token`; `hello` wraps it with `None`. Live-probed
  (temp example, deleted): token is 22-char base64url of 16 bytes; with the
  token the new welcome has `resumed: true` and the SAME `client.id` plus a
  NEW token; a bogus token silently starts a new client (`resumed: false`,
  different id).
- `ReconnectToken([u8;16])`: manual `Debug` printing
  `ReconnectToken([redacted; 16 bytes])`, so no derive can ever leak it.
- `DataWelcome { token, retention_ms, nonce }` parsed from the 32-byte
  WELCOME; `open_data` now returns it (callers updated: probe, own_block,
  app).
- `resume_data(path, token, bytes)` → `DataHandshake::{Welcome(Box<Ws>,
  DataWelcome), Rejected(u8)}`; shared `handshake` for HELLO/RESUME.
- `StreamResume`: the authoritative per-stream state — `welcomed` adopts the
  newest token, `consume(len)` counts exactly once per processed payload,
  `bytes()`, `resume_message()` → 25-byte RESUME. Unit tests: 25-byte
  layout with LE u64, counting across a simulated reconnect (chunked
  messages incl. a 0 and a 64 KiB one), newest-token-wins, Debug never
  prints token bytes.

## Step 2 — integration example `rex-conn/examples/resume.rs` (commit)

Live run (2025-…, server = user's /run/user/1000/rex/server.sock process):

```
warm-up: 1895 bytes consumed, max line number on screen 62
PASS resume 1: dropped at byte 1895, resumed at byte 1895, now at 2193; line numbers 62 -> 129
PASS resume 2: dropped at byte 2193, resumed at byte 2193, now at 2528; line numbers 129 -> 196
PASS resume 3: dropped at byte 2528, resumed at byte 2528, now at 2863; line numbers 196 -> 263
PASS RESUME count-1 rejected with 0x03 (fewer bytes than sent)
stream ended by the -1 refusal (0x03); starting over with a new ticket + HELLO
PASS RESUME count+1 rejected with 0x05 (more bytes than sent)
PASS control reconnect: client id client:…b9680b7a survives the drop, resumed: true
all checks passed; session destroyed
```

Findings worth keeping:
- **The count must be exact is literal**: ±1 byte → REJECT 0x03 (fewer) /
  0x05 (more), and the refusal ENDS the stream (a later exact-count
  RESUME gets 0x03). The example proves both reasons on live streams,
  doing -1 first and starting over (new ticket + HELLO, new channel
  nonce by design) before the +1 probe.
- **A bare control client does not survive its drop**: a client not
  attached to anything and with no scripts "leaves at once" (doc,
  Clients in a session list), so its reconnect_token dies immediately —
  an unclean drop + token hello resumed=false. An ATTACHED client
  survives the unclean drop and resumes: true with the same client id.
  The example attaches its probe connection before dropping. (The app's
  control connection is always attached, so the app path is unaffected.)
- A new stream (start-over) has a NEW channel nonce; a resumed stream
  keeps the same one.
- rex.service (user unit) is inactive here; the live server is the
  user's own `rex server run` process (PID since 12:13, no restarts).
  No service restarts were performed.

## Step 3 — wired into the app (commit)

### rex-conn
- `recv_json` now SKIPS websocket control frames (Ping/Pong): the server
  pings control connections on a schedule and a ping between request and
  response used to fail an RPC in flight — caught live (see smoke).

### rex-app/client.rs
- `ClientEvent::Reconnecting` / `Reconnected` (status bar: app.rs shows
  "reconnecting…" / "reconnected"; Closed stays the final give-up).
- Control path: `run` keeps the newest welcome's `client.reconnect_token`
  (never logged); on a drop the outer loop calls `reconnect_control`
  (hello with token, backoff 0.5s→5s, ~3 min bound), then `catch_up`:
  `session.list`, re-`session.attach` (updates `my_client_id` from the
  result — a refused token means a NEW client), re-publishes the view,
  and re-claims blocks still marked ours (clearing `last_grid` so the
  grid resize re-applies; the server's `restore` rule is the backstop).
- Data path: `stream_block` is now a connection loop around the ONE
  authoritative `rex_conn::StreamResume` (consume → classify →
  reconnect). Close table (`classify_close`, unit-tested): 1000/4000
  stop, 1013 restarts the stream, everything else (incl. unclean drops)
  resumes. REJECT table in `next_connection`: 0x02 → RESUME again;
  0x01/0x03/0x05 → start over (fresh ticket via the actor's new
  `TicketRequest` channel + HELLO, fresh Replica, `restarted` resets
  the count); unknown → stop. Retry window 120s.
- The stream task asks the actor (sole Rpc owner) for tickets through an
  mpsc; the main select serves them, so a stream start-over works even
  while the control connection is itself reconnecting (the request just
  waits, bounded 30s in `fresh_ticket`).
- `main.rs`: `REX_APP_HEADLESS=SECONDS` runs the actor without a window,
  logging events to stderr — the panic message already promised this.

### Live smoke (headless app through a killable unix-socket relay)

Created a session running an endless `echo $i` loop, ran the app
headless via /tmp/rex-relay.sock → server.sock, killed the relay (unclean
drop of BOTH control and data connections), restarted it:

```
[rex-app] control connection lost: … Connection reset without closing handshake; reconnecting
[rex-app] event: Reconnecting
[rex-app] block …b26e8d4: RESUME attempt failed: connecting to unix socket /tmp/rex-relay.sock   (x6, retry loop)
[rex-app] block …b26e8d4: resumed at byte 5234
[rex-app] event: Reconnected
[rex-app] control reconnected (resumed: true)
[rex-app] re-attached: view revision 1 (was 1)
```

Earlier run (same setup) caught the ping bug: a resumed connection's
ping arrived between `session.list` and its response and killed the call,
leaving an id-mismatched response for the next call — fixed by the
`recv_json` skip above.

Notes:
- No display in this environment, so the GUI itself wasn't launched; the
  headless run drives the same actor the window does, and app.rs's
  status handling for the new events is a two-line match arm. rex.service
  was NOT restarted (it's inactive; the live server is the user's own
  process). `ss -K` has no unix-socket equivalent, so the relay is the
  forced-drop mechanism.
- The in-app re-own path (catch_up's own_block loop) wasn't live-fired
  because headless never owns a block; own_block itself was live-proven
  in issue 5's example, and the restore rule covers the reconnect case.
- Smoke sessions destroyed (resume-test-*, reconnect-smoke, toktest-tmp).

## Step 4 — final checks (commit)

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean
  (exit 0; the future-incompat note about proc-macro-error2 is
  pre-existing and unrelated).
- `cargo test --workspace`: green — rex-app 20, rex-conn 9, rex-terminal
  13, replay 1. New this issue: RESUME encoding (25 bytes, LE u64),
  counting across a simulated reconnect, newest-token-wins,
  restarted-resets-count, token Debug redaction, close-status table.
- README: all status boxes ticked (key input, own_block, reconnect/resume
  — the final box closes the milestone); gotchas gained a
  "Reconnect and resume" entry (exact-count rule, ±1 REJECT ends the
  stream, bare clients don't survive drops, ping mid-RPC fix).
- Cleanup: all test sessions destroyed (resume-test-*, reconnect-smoke,
  toktest-tmp); generated probe-stream-reconnect-smoke.bin deleted (the
  pi-demo/probe fixtures untouched). No pushes, no PRs, no service
  restarts.

## Review-fix round (commit)

Two findings from the review:

1. **Headless log flooding** — the headless driver printed every event
   with `{event:?}`, and `ClientEvent::Frame`'s Debug dump is the whole
   `Frame` struct (rows of cells), easily 50KB+ per line. Fixed: frames
   now log one compact line — `[rex-app] frame block:<id> 80x24 cursor
   (17,0) rows: <first two non-empty rows, trimmed>` (via
   `rex_terminal::row_text`, mirroring app.rs's `debug_frames` logic);
   every other event keeps its Debug line. The event enum is unchanged —
   `Frame` doesn't carry `Fed.changed`, so no change-only filtering.

2. **Cleanup discipline** — earlier runs left test sessions behind
   (resume-test-*, toktest-tmp, reconnect-smoke; the orchestrator killed
   them). Rule for any run in this repo: `rex kill` every session the
   run created, before reporting done — leave `rex ls` empty.

### Ping verification (empirical)

A 420-second idle headless run (`REX_APP_HEADLESS=420`,
`REX_APP_DEBUG=1`, `--new-session`) produced ZERO "control connection
lost"/reconnect lines and exited cleanly. Proof the tungstenite
auto-answers the server's 120s pings through the `recv_json` skip-pings
path: no keepalive timeout within 420s (the server's 3-strike window is
~360s).

### Checks

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace`: all green.
- Functional: `REX_APP_HEADLESS=8 REX_APP_DEBUG=1 cargo run -p rex-app --
  --new-session` — frames logged compactly (see below); the session it
  created was `rex kill`ed; `rex ls` left empty.

## Review-fix round 2 (PR #11 external review)

### BLOCKING: no re-claim after a >60s lapse (new client id)

**Bug.** After a token lapse the server forgets the old client and the
re-attach hands us a NEW client id. `catch_up` updated `my_client_id`,
but `owners` still mapped our blocks to the OLD id (the
`block_owner_changed` events fired during the outage are lost), and the
re-claim filter was `owner == my_client_id` — matching nothing. No
blocks were re-claimed or re-gridded; the panes rendered at whatever
grid the server last had until window reactivation. The old comment
claiming the `client_id` re-read covers this was wrong: the re-read is
exactly what makes the filter miss.

**Fix.** The actor now tracks `former_client_ids` (every id we held
before the current one; `set_client_id` records the previous id on each
change, used by bootstrap and `catch_up`'s re-attach). The re-claim set
is computed by a pure associated fn, `Actor::reclaims(owners,
last_grid, my_client_id, former_client_ids)`: blocks the owners map
attributes to ANY of our ids (current or former) that have a grid to
re-apply. `catch_up` re-claims that set with a fresh `own_block` take —
legitimate, since the old client is dead server-side so its blocks are
unowned — and `own_block`'s `record_owners` re-records the entries
under the new id (verified: `record_owners` inserts unconditionally).
`disown_all`'s "do we own anything" guard now uses the same
current-or-former test (`is_our_id`), so a failed re-claim (old-id entry
left in the map) still disowns properly.

**Evidence.**
- Unit: `reclaims_covers_blocks_owned_under_a_former_client_id`
  (old-id + new-id entries both reclaimed; foreign/unowned/no-grid
  excluded) and `reclaims_without_the_old_id_misses_the_old_blocks`
  (the pre-fix filter, shown to miss — the bug, reproduced).
- Live (bounded, ~75 s): `resume.rs --lapse` — a control drop held
  past the ~60 s token window returns `resumed: false` with a NEW
  `client.id` (`PASS lapse reconnect: old id … forgotten, NEW client id
  … issued`). That validates the premise end-to-end: the id does
  change; the map-vs-id mismatch it creates is exactly what
  `reclaims`'s unit tests cover. A full GUI lapse (kill the GUI's
  control path for 60 s+ and watch it re-grid) was judged impractical
  to script; the premise is live-verified, the logic unit-verified.

### Non-blocking

1. **Server restart → stale view** — DONE. `catch_up` now adopts the
   re-attach's view unconditionally: after a restart the revision
   sequence starts over, so the fresh view can be LOWER than the stale
   one and the old `revision >=` gate dropped it. The re-attach's view
   is the truth by construction; revision gating stays on the
   connected paths (`refresh_view`, event handlers).
2. **`replica.feed` error ends the stream silently** — DONE. A decode
   failure now surfaces as `ClientEvent::Notice("block <id>: stream
   ended: <error>")` before the task returns the error.
3. **Stale ticket requests during long reconnects** — ASSESSED, cheap
   guard added. A queued request whose stream task died gets its
   oneshot reply dropped; `fresh_ticket` treats a dropped oneshot as a
   bail and `reply.send` failing is not an error worth acting on. The
   ticket RPC is wasted, not lost work; nothing hangs (the 30 s
   timeout in `fresh_ticket` bounds the wait while the actor is
   unreachable). Added a debug log when the reply is dropped so the
   waste is visible under `REX_APP_DEBUG`.

### Checks (this round)

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace` (60 tests): all green.
- `resume` example: all PASS (3 resumes, 0x03/0x05 rejections, control
  reconnect `resumed: true`); `own_block` example: all PASS.
- `resume --lapse`: PASS (see above), session destroyed.
- GUI smoke (`REX_APP_DEBUG=1 … timeout 12 … --new-session`): bootstrap,
  own_block at 121x38, resize matching cell metrics, view refresh —
  clean; session `rex kill`ed.
- Cleanup: `rex ls` empty, checked twice. No pushes, no PRs.
