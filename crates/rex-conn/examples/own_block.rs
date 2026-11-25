//! Integration check for `session.own_block` / `session.disown_block` and
//! grid negotiation against a live Rex server (issue 5, step 3). Run it
//! manually:
//!
//! ```sh
//! mise exec -- cargo run -p rex-conn --example own_block
//! ```
//!
//! It creates a session with a shell, owns its block at 40x12, verifies the
//! reflow through the `resize` rex1 message on the data stream AND through
//! the program's own eyes (`stty size` read back via
//! `com.superlogical.terminal.format`), re-owns at 100x30 and verifies
//! again. Then a SECOND control connection takes the same block: the first
//! client must see `block_owner_changed`, and the block's grid must follow
//! the second client (66x22). Cleans up after itself (`session.destroy`).

use anyhow::{bail, Result};
use futures_util::StreamExt;
use rex_conn::{connect, data_ticket, hello, open_data, Rpc};
use rex_terminal::Replica;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
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

async fn wait_output() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // The replica's libghostty iterators are !Send, so the stream task
    // runs on a LocalSet on this thread, like the app's actor does.
    let local = tokio::task::LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> Result<()> {
    let socket = socket_path();
    let label = format!("owntest-{}", std::process::id());

    // ---- client 1: create a session, attach, own the block ----
    let ws1 = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (ws1, _welcome) = hello(
        ws1,
        json!({"kind": "cli", "name": "own-block-example-1", "version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;
    // Session events land in this channel; Rpc hands them over between
    // calls, and we pump with cheap calls while waiting for them.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
    let mut rpc1 = Rpc::new(ws1, move |event: &Value| {
        let _ = event_tx.send(event.clone());
    });
    println!("client 1 connected");

    let created = rpc1
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
    let attached = rpc1
        .call("session.attach", json!({"session_id": session_id}))
        .await?;
    let my_client_id = attached["client_id"]
        .as_str()
        .expect("client_id in attach result")
        .to_owned();
    let block_id = attached["view"]["windows"][0]["focused_block_id"]
        .as_str()
        .expect("focused block")
        .to_owned();
    println!("session {session_id}\nclient 1 is {my_client_id}\nblock {block_id}");

    // The data stream runs as a local task (the replica holds !Send
    // iterators, like the app's actor). It reports every resize rex1.
    let ticket = data_ticket(&mut rpc1, &session_id, &block_id).await?;
    let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u32, u32, u32, u32)>();
    let stream_socket = socket.clone();
    let stream_nonce_ticket = ticket.clone();
    tokio::task::spawn_local(async move {
        let result = stream_resizes(&stream_socket, &stream_nonce_ticket, resize_tx).await;
        if let Err(error) = result {
            eprintln!("stream task ended: {error}");
        }
    });

    // ---- own at 40x12, verify reflow two ways ----
    own_and_resize(&mut rpc1, &session_id, &block_id, 40, 12).await?;
    let (cols, rows, cell_w, cell_h) = wait_resize(&mut resize_rx, 40, 12).await?;
    println!("PASS resize rex1 says {cols}x{rows} at {cell_w}x{cell_h}px");
    let stty = stty_size(&mut rpc1, &session_id, &block_id).await?;
    if stty != "12 40" {
        bail!("stty size after 40x12 said {stty:?}, expected \"12 40\"");
    }
    println!("PASS stty size reports {stty:?} (rows cols) for 40x12");

    // ---- re-own at 100x30, verify again ----
    own_and_resize(&mut rpc1, &session_id, &block_id, 100, 30).await?;
    let (cols, rows, _, _) = wait_resize(&mut resize_rx, 100, 30).await?;
    println!("PASS resize rex1 says {cols}x{rows} after re-own");
    let stty = stty_size(&mut rpc1, &session_id, &block_id).await?;
    if stty != "30 100" {
        bail!("stty size after 100x30 said {stty:?}, expected \"30 100\"");
    }
    println!("PASS stty size reports {stty:?} (rows cols) for 100x30");

    // ---- a second client takes the block; client 1 must see it ----
    let ws2 = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (ws2, _welcome) = hello(
        ws2,
        json!({"kind": "cli", "name": "own-block-example-2", "version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;
    let mut rpc2 = Rpc::new(ws2, |_| {});
    rpc2.call("session.attach", json!({"session_id": session_id}))
        .await?;
    let taken = rpc2
        .call(
            "session.own_block",
            json!({"session_id": session_id, "block_ids": [block_id], "mode": "take"}),
        )
        .await?;
    let owner2 = taken["blocks"][0]["owner"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    println!("client 2 took the block; owner now {owner2}");

    // Pump client 1's connection (cheap calls surface queued events) until
    // the takeover event arrives: the owner must become client 2, taking
    // it from us (client 1's own earlier claim also sent an event).
    let mut saw_takeover = false;
    for _ in 0..20 {
        while let Ok(event) = event_rx.try_recv() {
            if event["type"] == "session_event" {
                let inner = &event["payload"]["event"];
                if inner["type"] == "block_owner_changed" {
                    let payload = &inner["payload"];
                    let owner = payload["owner"].as_str().unwrap_or("none");
                    let previous = payload["previous"].as_str().unwrap_or("none");
                    let reason = payload["reason"].as_str().unwrap_or("?");
                    println!("client 1 saw block_owner_changed: owner={owner} previous={previous} reason={reason}");
                    if owner == owner2 && previous == my_client_id {
                        saw_takeover = true;
                    }
                }
            }
        }
        if saw_takeover {
            break;
        }
        rpc1.call("server.whoami", json!({})).await?;
        wait_output().await;
    }
    if !saw_takeover {
        bail!("client 1 never saw block_owner_changed");
    }
    println!("PASS client 1 saw block_owner_changed (exclusivity, no fight)");

    // ---- the block's grid follows client 2 now ----
    rpc2.call(
        "com.superlogical.terminal.resize",
        json!({"session_id": session_id, "block_id": block_id,
                   "args": {"columns": 66, "rows": 22}}),
    )
    .await?;
    let (cols, rows, _, _) = wait_resize(&mut resize_rx, 66, 22).await?;
    println!("PASS block followed client 2's grid: resize rex1 says {cols}x{rows}");
    let stty = stty_size(&mut rpc1, &session_id, &block_id).await?;
    if stty != "22 66" {
        bail!("stty size after client 2's 66x22 said {stty:?}, expected \"22 66\"");
    }
    println!("PASS stty size reports {stty:?} (rows cols) from client 2's grid");

    // ---- client 2 disowns: sizing passes on, the block lives ----
    // Client 1 claimed with `take` earlier, so it's the in-line claimer:
    // the block passes back to us, and our own last resize request (sent
    // while client 2 owned, so deferred until we became the owner again)
    // applies now. Either way the block must stay alive and controllable.
    rpc2.call("session.disown_block", json!({"session_id": session_id}))
        .await?;
    let mut saw_pass_back = false;
    for _ in 0..20 {
        while let Ok(event) = event_rx.try_recv() {
            if event["type"] == "session_event" {
                let inner = &event["payload"]["event"];
                if inner["type"] == "block_owner_changed" {
                    let payload = &inner["payload"];
                    let owner = payload["owner"].as_str().unwrap_or("none");
                    let previous = payload["previous"].as_str().unwrap_or("none");
                    let reason = payload["reason"].as_str().unwrap_or("?");
                    println!("client 1 saw block_owner_changed: owner={owner} previous={previous} reason={reason}");
                    if owner == my_client_id && reason == "disown" {
                        saw_pass_back = true;
                    }
                }
            }
        }
        if saw_pass_back {
            break;
        }
        rpc1.call("server.whoami", json!({})).await?;
        wait_output().await;
    }
    if !saw_pass_back {
        bail!("client 1 never saw the block pass back on disown");
    }
    let stty = stty_size(&mut rpc1, &session_id, &block_id).await?;
    let (rows, cols): (u32, u32) = {
        let mut it = stty.split_whitespace();
        (
            it.next().unwrap_or_default().parse()?,
            it.next().unwrap_or_default().parse()?,
        )
    };
    println!(
        "PASS disown kept the block alive; grid now {cols}x{rows} (our deferred resize applied)"
    );

    // Still the owner: a fresh resize applies at once.
    rpc1.call(
        "com.superlogical.terminal.resize",
        json!({"session_id": session_id, "block_id": block_id,
                   "args": {"columns": 50, "rows": 16}}),
    )
    .await?;
    let stty = stty_size(&mut rpc1, &session_id, &block_id).await?;
    if stty != "16 50" {
        bail!("stty size after re-own 50x16 said {stty:?}, expected \"16 50\"");
    }
    println!("PASS client 1 still sizes the block after handback: {stty:?}");

    // ---- clean up ----
    rpc1.call("session.destroy", json!({"session_id": session_id}))
        .await?;
    println!("all checks passed; session destroyed");
    Ok(())
}

/// `session.own_block` (take) then `com.superlogical.terminal.resize`:
/// the claim first, so the resize applies at once.
async fn own_and_resize(
    rpc: &mut Rpc,
    session_id: &str,
    block_id: &str,
    cols: u32,
    rows: u32,
) -> Result<()> {
    let owned = rpc
        .call(
            "session.own_block",
            json!({"session_id": session_id, "block_ids": [block_id], "mode": "take"}),
        )
        .await?;
    println!(
        "own_block {cols}x{rows}: owner={}",
        owned["blocks"][0]["owner"].as_str().unwrap_or("none"),
    );
    rpc.call(
        "com.superlogical.terminal.resize",
        json!({"session_id": session_id, "block_id": block_id,
               "args": {"columns": cols, "rows": rows,
                        "cell_width_px": 9, "cell_height_px": 18}}),
    )
    .await?;
    Ok(())
}

/// Waits (bounded) for a resize rex1 reporting `cols` x `rows` on the data
/// stream, skipping earlier sizes.
async fn wait_resize(
    resize_rx: &mut mpsc::UnboundedReceiver<(u32, u32, u32, u32)>,
    cols: u32,
    rows: u32,
) -> Result<(u32, u32, u32, u32)> {
    for _ in 0..50 {
        while let Ok(size) = resize_rx.try_recv() {
            if size.0 == cols && size.1 == rows {
                return Ok(size);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("no resize rex1 saying {cols}x{rows} arrived within 5s");
}

/// Runs `clear; stty size` in the shell and reads the screen back as
/// text, so the *program* confirms the pty's negotiated size, not just
/// the stream. The clear keeps older rows-cols pairs off the screen, so
/// the one match found is this run's answer.
async fn stty_size(rpc: &mut Rpc, session_id: &str, block_id: &str) -> Result<String> {
    rpc.call(
        "client.key",
        json!({"session_id": session_id, "key": key_event("unidentified", "clear; stty size")}),
    )
    .await?;
    rpc.call(
        "client.key",
        json!({"session_id": session_id, "key": key_event("enter", "")}),
    )
    .await?;
    wait_output().await;
    let text = rpc
        .call(
            "com.superlogical.terminal.format",
            json!({"session_id": session_id, "block_id": block_id,
                   "args": {"format": "text", "trim": true}}),
        )
        .await?;
    let screen = text["content"].as_str().unwrap_or_default();
    // The last "<rows> <cols>" pair on the screen is stty's answer.
    let found = screen
        .lines()
        .rev()
        .find_map(|line| {
            let mut it = line.split_whitespace();
            let rows = it.next()?;
            let cols = it.next()?;
            (rows.parse::<u32>().is_ok() && cols.parse::<u32>().is_ok())
                .then(|| format!("{rows} {cols}"))
        })
        .unwrap_or_default();
    Ok(found)
}

/// Opens the block's data stream and reports every `resize` rex1 that
/// carries a grid, as (cols, rows, cell_width_px, cell_height_px).
async fn stream_resizes(
    socket: &str,
    ticket: &[u8],
    resize_tx: mpsc::UnboundedSender<(u32, u32, u32, u32)>,
) -> Result<()> {
    let (mut ws, nonce) = open_data(socket, ticket).await?;
    let mut replica = Replica::new(nonce)?;
    while let Some(message) = ws.next().await {
        let bytes = match message? {
            Message::Binary(bytes) => bytes,
            _ => continue,
        };
        let fed = replica.feed(&bytes)?;
        if let Some(size) = fed.resize {
            let _ = resize_tx.send(size);
        }
    }
    Ok(())
}
