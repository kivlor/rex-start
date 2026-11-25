//! Headless probe for a Rex server: proves the control and data paths
//! against the real protocol, with no UI.
//!
//! Flow (see docs/API-rev19.md, "A First Session"):
//!   1. control WS  -> hello -> welcome (api revision 19)
//!   2. session.create (one window, one shell block)
//!   3. session.attach
//!   4. com.superlogical.terminal.write  (type the command)
//!   5. data.connect -> ticket -> data WS -> binary HELLO -> WELCOME
//!   6. stream the block's output to a file, decoding rex1 APC control
//!      messages (resize / theme / child_exited) by channel nonce
//!   7. com.superlogical.terminal.format -> print the screen as text
//!
//! The connection plumbing lives in `crates/rex-conn`, shared with the
//! GPUI app. The session is left running on the server so other clients
//! can attach.

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use rex_conn::{connect, hello, open_data, Rpc};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

struct Args {
    socket: String,
    label: String,
    command: String,
    seconds: u64,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        socket: format!(
            "{}/rex/server.sock",
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into())
        ),
        label: "probe".into(),
        command: "echo probe-ok".into(),
        seconds: 15,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let val = it
            .next()
            .ok_or_else(|| anyhow!("missing value for {flag}"))?;
        match flag.as_str() {
            "--socket" => args.socket = val,
            "--label" => args.label = val,
            "--command" => args.command = val,
            "--seconds" => args.seconds = val.parse()?,
            _ => bail!("unknown flag {flag}"),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    println!("probe -> {}", args.socket);

    // ---- control connection ----
    let control = connect(&args.socket, "/api/control", "rex.control.v1").await?;
    let (control, welcome) = hello(
        control,
        json!({"kind": "cli", "name": "probe", "version": "0.1.0"}),
    )
    .await?;
    let revision = welcome["api_revision"].as_u64().unwrap_or(0);
    println!(
        "welcome: protocol={} api_revision={} client={}",
        welcome["protocol"], revision, welcome["client"]["id"]
    );

    let mut rpc = Rpc::new(control, |event: &Value| {
        // view_changed is noisy; everything else is worth a line.
        if event["type"] != "session_view_changed" {
            eprintln!("[event] {}", event["type"]);
        }
    });

    // ---- create a session with one shell block ----
    let created = rpc
        .call(
            "session.create",
            json!({
                "label": args.label,
                "initial_windows": [
                    {"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}
                ]
            }),
        )
        .await?;
    let session_id = created["session_id"]
        .as_str()
        .ok_or_else(|| anyhow!("no session_id in {created}"))?
        .to_string();
    let block_id = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .ok_or_else(|| anyhow!("no block_ids in {created}"))?
        .to_string();
    println!("session: {session_id}\nblock:   {block_id}");

    // ---- attach: required before data.connect ----
    rpc.call("session.attach", json!({"session_id": session_id}))
        .await?;

    // ---- type the command ----
    rpc.call(
        "com.superlogical.terminal.write",
        json!({"session_id": session_id, "block_id": block_id,
               "args": {"data": format!("{}\n", args.command)}}),
    )
    .await?;

    // ---- data connection ----
    let ticket = rex_conn::data_ticket(&mut rpc, &session_id, &block_id).await?;
    let (data, nonce) = open_data(&args.socket, &ticket).await?;
    println!("data stream open (channel nonce {nonce})");

    // ---- stream until child_exited or the deadline ----
    let (tx, mut rx) = mpsc::channel::<i64>(1);
    let stream_path = format!("probe-stream-{}.bin", args.label);
    let stream_path_task = stream_path.clone();
    let mut data = data;
    let data_task = tokio::spawn(async move {
        let mut out = std::fs::File::create(&stream_path_task).expect("create stream file");
        // rex1 APC messages: ESC _ rex1;<nonce>;<json> ESC \
        // rex-terminal's filter skips look-alikes whose nonce doesn't match.
        let mut filter = rex_terminal::StreamFilter::new(nonce);
        loop {
            match data.next().await {
                Some(Ok(Message::Binary(b))) => {
                    use std::io::Write;
                    out.write_all(&b).expect("write stream file");
                    let mut exits: Vec<i64> = Vec::new();
                    filter.feed(&b, |piece| {
                        if let rex_terminal::Piece::Message(v) = piece {
                            match v {
                                rex_terminal::RexMessage::Resize { cols, rows, .. } => {
                                    eprintln!("[rex1] resize {cols}x{rows}");
                                }
                                rex_terminal::RexMessage::Theme { scheme, .. } => {
                                    eprintln!(
                                        "[rex1] theme {}",
                                        scheme.as_deref().unwrap_or("(from background)")
                                    );
                                }
                                rex_terminal::RexMessage::ChildExited { exit_code } => {
                                    eprintln!("[rex1] child_exited {exit_code}");
                                    // i64 as-is: negative codes are signal
                                    // deaths, and `.max(0)` would hide them.
                                    exits.push(exit_code);
                                }
                                rex_terminal::RexMessage::Unknown => {
                                    eprintln!("[rex1] (unknown type)");
                                }
                            }
                        }
                    });
                    for code in exits {
                        let _ = tx.send(code).await;
                    }
                }
                Some(Ok(m)) => bail!("unexpected data message: {m:?}"),
                Some(Err(e)) => bail!("data stream error: {e}"),
                None => break,
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let deadline = tokio::time::sleep(Duration::from_secs(args.seconds));
    tokio::pin!(deadline);
    let exited = loop {
        select! {
            _ = &mut deadline => break None,
            code = rx.recv() => break Some(code.unwrap_or(0)),
            msg = rpc.ws.next() => {
                match msg {
                    Some(Ok(Message::Text(s))) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&s) {
                            if v["type"] == "event" && v["payload"]["type"] != "session_view_changed" {
                                eprintln!("[event] {}", v["payload"]["type"]);
                            }
                        }
                    }
                    Some(other) => bail!("control connection problem: {other:?}"),
                    None => bail!("control connection closed"),
                }
            }
        }
    };
    data_task.abort();

    // ---- read the screen as text ----
    tokio::time::sleep(Duration::from_secs(1)).await;
    let screen = rpc
        .call(
            "com.superlogical.terminal.format",
            json!({"session_id": session_id, "block_id": block_id}),
        )
        .await?["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    println!("---- screen (terminal.format) ----");
    println!("{}", screen.trim_end());
    println!("----------------------------------");

    match exited {
        Some(code) => println!("child exited with {code}"),
        None => println!(
            "still running after {}s — session '{label}' stays alive on the server.\n  inspect: rex capture -s {label}\n  end it:  rex kill -s {label}",
            args.seconds,
            label = args.label
        ),
    }
    println!("stream saved to {stream_path} (Ghostty snapshot + raw output)");
    Ok(())
}
