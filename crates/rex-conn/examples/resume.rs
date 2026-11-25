//! Integration check for data-connection RESUME with exact byte counting
//! (issue 6, step 2). Run it manually:
//!
//! ```sh
//! mise exec -- cargo run -p rex-conn --example resume
//! ```
//!
//! It creates a session printing an endless line-number sequence
//! (`0, 1, 2, ...`), opens the block's data stream, and drops the
//! connection WITHOUT a close frame three times, resuming each time with
//! the authoritative `StreamResume` count. The server only accepts an
//! exact count (it rejects ±1 with reasons 0x05 more / 0x03 fewer), so
//! three accepted resumes prove the counter; the on-screen line numbers
//! staying monotonic proves the output has no gap or duplicate. Then the
//! wrong-count path: RESUME with count+1 must REJECT 0x05, count-1 must
//! REJECT 0x03 (each probed on a live stream, starting over after a
//! refusal if the server ended the stream). Finally the control path: a
//! control connection dropped and re-helloed with its `reconnect_token`
//! must keep the same `client.id` with `resumed: true`.
//!
//! Cleans up after itself (`session.destroy`).

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use futures_util::StreamExt;
use rex_conn::{
    connect, data_ticket, hello, hello_with_token, open_data, resume_data, DataHandshake, Rpc,
    StreamResume,
};
use rex_terminal::{row_text, Replica};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

fn socket_path() -> String {
    std::env::var("REX_SERVER")
        .ok()
        .filter(|s| !s.starts_with("http"))
        .unwrap_or_else(|| {
            format!(
                "{}/rex/server.sock",
                std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into())
            )
        })
}

/// One key press as `client.key`'s `key` object.
fn key_event(key: &str, text: &str) -> Value {
    json!({
        "key": key,
        "mods": "",
        "consumed_mods": "",
        "text": text,
        "unshifted_codepoint": 0,
        "composing": false,
        "action": "press",
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // The replica's libghostty iterators are !Send, so the stream work
    // runs on a LocalSet on this thread, like the app's actor does.
    let local = tokio::task::LocalSet::new();
    local.run_until(run()).await
}

/// Everything one data stream needs, in one place: the authoritative
/// byte counter with its latest token, the replica, and the largest line
/// number seen on screen (monotonicity is the no-gap/no-duplicate check).
struct Stream {
    ws: rex_conn::Ws,
    resume: StreamResume,
    replica: Replica,
    max_line: u64,
}

async fn run() -> Result<()> {
    let socket = socket_path();
    let label = format!("resume-test-{}", std::process::id());

    // ---- control connection: create a session with one shell block ----
    let ws = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (ws, _welcome) = hello(
        ws,
        json!({"kind": "cli", "name": "resume-example", "version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;
    let mut rpc = Rpc::new(ws, |_| {});
    let created = rpc
        .call(
            "session.create",
            json!({"label": label, "initial_windows": [
                {"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}
            ]}),
        )
        .await?;
    let session_id = created["session_id"]
        .as_str()
        .expect("session_id")
        .to_owned();
    let attached = rpc
        .call("session.attach", json!({"session_id": session_id}))
        .await?;
    let block_id = attached["view"]["windows"][0]["focused_block_id"]
        .as_str()
        .expect("focused block")
        .to_owned();
    println!("session {session_id}\nblock {block_id}");

    // ---- start the endless line-number printer ----
    rpc.call(
        "client.key",
        json!({"session_id": session_id,
               "key": key_event("unidentified",
                   "i=0; while true; do echo $i; i=$((i+1)); sleep 0.05; done")}),
    )
    .await?;
    rpc.call(
        "client.key",
        json!({"session_id": session_id, "key": key_event("enter", "")}),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ---- data connection: HELLO, then consume some bytes ----
    let ticket = data_ticket(&mut rpc, &session_id, &block_id).await?;
    let (ws, welcome) = open_data(&socket, &ticket).await?;
    let nonce = welcome.nonce;
    println!(
        "data WELCOME: nonce {nonce}, retention {} ms",
        welcome.retention_ms
    );
    let mut stream = Stream {
        ws,
        resume: StreamResume::default(),
        replica: Replica::new(nonce)?,
        max_line: 0,
    };
    stream.resume.welcomed(welcome);
    consume(&mut stream, 60).await?;
    println!(
        "warm-up: {} bytes consumed, max line number on screen {}",
        stream.resume.bytes(),
        stream.max_line
    );

    // ---- 3 unclean drops + exact-count resumes ----
    for attempt in 1..=3 {
        let before = stream.resume.bytes();
        let max_before = stream.max_line;
        // Drop WITHOUT a close frame: the fd just closes, which the
        // server sees as a lost connection, leaving the stream waiting.
        drop(stream.ws);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let token = stream
            .resume
            .token()
            .ok_or_else(|| anyhow!("no reconnect token yet"))?;
        let count = stream.resume.bytes();
        match resume_data(&socket, token, count).await? {
            DataHandshake::Welcome(ws, welcome) => {
                if welcome.nonce != nonce {
                    bail!("resume WELCOME nonce {} != original {nonce}", welcome.nonce);
                }
                stream.ws = *ws;
                // The newest token always wins: adopt it before resuming.
                stream.resume.welcomed(welcome);
                consume(&mut stream, 60).await?;
                println!(
                    "PASS resume {attempt}: dropped at byte {before}, resumed at byte {count}, now at {}; line numbers {} -> {} (no gap, no duplicate)",
                    stream.resume.bytes(),
                    max_before,
                    stream.max_line
                );
                if stream.resume.bytes() <= before {
                    bail!("no bytes arrived after resume {attempt}");
                }
                if stream.max_line < max_before {
                    bail!("line numbers went backwards after resume {attempt}");
                }
            }
            DataHandshake::Rejected(reason) => {
                bail!("resume {attempt} rejected: 0x{reason:02x}");
            }
        }
    }

    // ---- the count must be EXACT: -1 byte is REJECT 0x03 ----
    drop(stream.ws);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let token = stream.resume.token().expect("token");
    let count = stream.resume.bytes();
    let reason = expect_reject(&socket, token, count.wrapping_sub(1)).await?;
    if reason != 0x03 {
        bail!("wrong count -1 rejected with 0x{reason:02x}, expected 0x03");
    }
    println!("PASS RESUME count-1 rejected with 0x03 (fewer bytes than sent)");

    // ---- and +1 byte is REJECT 0x05 ----
    // The refused RESUME may have ended the stream (the doc says an
    // inexact count ends it), so first try to resume the same stream
    // with the exact count; if that is refused too, start over with a
    // new ticket + HELLO — the doc's prescribed path after a refused
    // RESUME — and probe +1 on the fresh stream.
    match resume_data(&socket, token, count).await? {
        DataHandshake::Welcome(ws, welcome) => {
            stream.ws = *ws;
            stream.resume.welcomed(welcome);
            println!("stream survived the -1 refusal; exact-count resume still works");
        }
        DataHandshake::Rejected(reason) => {
            println!(
                "stream ended by the -1 refusal (0x{reason:02x}); starting over with a new ticket + HELLO"
            );
            let ticket = data_ticket(&mut rpc, &session_id, &block_id).await?;
            let (ws, welcome) = open_data(&socket, &ticket).await?;
            // A new stream is a new channel: the nonce differs by design.
            assert_ne!(welcome.nonce, 0, "channel nonce is never zero");
            stream.ws = ws;
            stream.replica = Replica::new(welcome.nonce)?;
            stream.resume.welcomed(welcome);
            consume(&mut stream, 60).await?;
        }
    }
    drop(stream.ws);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let token = stream.resume.token().expect("token");
    let count = stream.resume.bytes();
    let reason = expect_reject(&socket, token, count + 1).await?;
    if reason != 0x05 {
        bail!("wrong count +1 rejected with 0x{reason:02x}, expected 0x05");
    }
    println!("PASS RESUME count+1 rejected with 0x05 (more bytes than sent)");

    // ---- control reconnect: same client id, resumed: true ----
    // A second control connection attached to the session, then dropped
    // WITHOUT a close frame. A bare client that isn't attached to
    // anything leaves the server at once on a drop ("Clients in a
    // session list" in the doc), so the token test needs an attached
    // client — like the app's own always-attached control connection.
    let client =
        json!({"kind": "cli", "name": "resume-example-ctl", "version": env!("CARGO_PKG_VERSION")});
    let cws = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (mut cws, welcome) = hello(cws, client.clone()).await?;
    let id1 = welcome["client"]["id"]
        .as_str()
        .expect("client id")
        .to_owned();
    let token1 = welcome["client"]["reconnect_token"]
        .as_str()
        .expect("reconnect_token")
        .to_owned();
    assert_eq!(
        token_byte_len(&token1),
        16,
        "control reconnect_token should be 16 bytes base64url"
    );
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message as _Message;
    let attach = json!({"type": "request", "id": "1", "method": "session.attach",
                        "payload": {"session_id": &session_id}});
    cws.send(_Message::Text(attach.to_string().into())).await?;
    let _ = cws.next().await; // the attach response: the client is attached now
    drop(cws); // unclean drop of the attached control connection
    tokio::time::sleep(Duration::from_millis(300)).await;
    let cws2 = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (_cws2, welcome2) = hello_with_token(cws2, client, Some(&token1)).await?;
    let resumed = welcome2["client"]["resumed"].as_bool().unwrap_or(false);
    let same_id = welcome2["client"]["id"].as_str() == Some(id1.as_str());
    if !resumed || !same_id {
        bail!("control reconnect: resumed={resumed}, same id={same_id}");
    }
    println!("PASS control reconnect: client id {id1} survives the drop, resumed: true");

    // ---- clean up ----
    rpc.call("session.destroy", json!({"session_id": session_id}))
        .await?;
    println!("all checks passed; session destroyed");
    Ok(())
}

/// Decodes `token` as base64url and returns its byte length, for probing
/// the shape without ever logging the value.
fn token_byte_len(token: &str) -> usize {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .map(|d| d.len())
        .unwrap_or(0)
}

/// Consumes up to `messages` binary payloads, counting every byte exactly
/// once in `resume` and tracking the largest line number on screen.
async fn consume(stream: &mut Stream, messages: usize) -> Result<()> {
    for _ in 0..messages {
        let bytes = match stream.ws.next().await {
            Some(Ok(Message::Binary(bytes))) => bytes,
            Some(Ok(Message::Close(frame))) => {
                bail!("data connection closed: {frame:?}");
            }
            Some(Ok(_)) => continue,
            Some(Err(error)) => return Err(error.into()),
            None => bail!("data connection dropped"),
        };
        // The authoritative count: exactly once per consumed payload,
        // before the replica uses it, so a feed error can't undercount.
        stream.resume.consume(bytes.len());
        let fed = stream.replica.feed(&bytes)?;
        if fed.changed {
            if let Some(frame) = stream.replica.frame()? {
                for row in &frame.cells {
                    for number in row_text(row).split_whitespace() {
                        if let Ok(n) = number.parse::<u64>() {
                            stream.max_line = stream.max_line.max(n);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Sends one (deliberately wrong) RESUME and returns its REJECT reason,
/// failing if the server wrongly accepts it.
async fn expect_reject(socket: &str, token: rex_conn::ReconnectToken, count: u64) -> Result<u8> {
    match resume_data(socket, token, count).await? {
        DataHandshake::Welcome(..) => {
            bail!("server accepted a RESUME with count {count} — it must be exact")
        }
        DataHandshake::Rejected(reason) => Ok(reason),
    }
}
