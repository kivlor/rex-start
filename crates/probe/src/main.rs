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
//! The session is left running on the server so other clients can attach.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::select;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

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

type Ws = WebSocketStream<UnixStream>;

/// A Sec-WebSocket-Key: 16 random bytes, base64, from /dev/urandom.
fn ws_key() -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut bytes = [0u8; 16];
    f.read_exact(&mut bytes)?;
    use base64::engine::general_purpose::STANDARD;
    Ok(STANDARD.encode(bytes))
}

/// Open a WebSocket over a Unix socket at `path` with `subprotocol`.
async fn connect(path: &str, uri_path: &str, subprotocol: &str) -> Result<Ws> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to unix socket {path}"))?;
    let req = Request::builder()
        .method("GET")
        .uri(format!("ws://rex{uri_path}"))
        .header("Host", "rex")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", ws_key()?)
        .header("Sec-WebSocket-Protocol", subprotocol)
        .body(())?;
    let (ws, resp) = tokio_tungstenite::client_async(req, stream).await?;
    if resp.status() != 101 {
        bail!(
            "websocket handshake over {uri_path} failed: {}",
            resp.status()
        );
    }
    Ok(ws)
}

/// One JSON message on the control connection.
async fn send_json(ws: &mut Ws, v: &Value) -> Result<()> {
    Ok(ws.send(Message::Text(v.to_string().into())).await?)
}

/// Read the next text message as JSON (skips nothing; control is all text).
async fn recv_json(ws: &mut Ws) -> Result<Value> {
    match ws.next().await {
        Some(Ok(Message::Text(s))) => Ok(serde_json::from_str(&s)?),
        Some(Ok(m)) => bail!("unexpected control message: {m:?}"),
        Some(Err(e)) => Err(e.into()),
        None => bail!("control connection closed"),
    }
}

struct Rpc {
    ws: Ws,
    next_id: u64,
}

impl Rpc {
    /// Send a request and wait for its response, logging any events that
    /// arrive first. Never used for two calls at once, so ids stay unique.
    async fn call(&mut self, method: &str, payload: Value) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        send_json(
            &mut self.ws,
            &json!({"type": "request", "id": id, "method": method, "payload": payload}),
        )
        .await?;
        let fut = recv_json(&mut self.ws);
        let msg = tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .map_err(|_| anyhow!("timeout waiting for response to {method} (id {id})"))??;
        // Events can arrive between request and response.
        let mut msg = msg;
        while msg["type"] != "response" || msg["id"] != json!(id) {
            if msg["type"] == "event" {
                let inner = &msg["payload"]["type"];
                if inner != "session_view_changed" {
                    // view_changed is noisy; everything else is worth a line.
                    eprintln!("[event] {inner}");
                }
            } else if msg["type"] == "response" {
                bail!("response with wrong id for {method}: {}", msg);
            }
            msg = recv_json(&mut self.ws).await?;
        }
        if let Some(err) = msg.get("error") {
            bail!("{method} failed: {}", err);
        }
        Ok(msg["payload"].clone())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    println!("probe -> {}", args.socket);

    // ---- control connection ----
    let mut control = connect(&args.socket, "/api/control", "rex.control.v1").await?;
    send_json(
        &mut control,
        &json!({
            "type": "hello",
            "api_revision": 19,
            "client": {"kind": "cli", "name": "probe", "version": "0.1.0"}
        }),
    )
    .await?;
    let welcome = recv_json(&mut control).await?;
    if welcome["type"] != "welcome" {
        bail!("expected welcome, got: {welcome}");
    }
    let revision = welcome["api_revision"].as_u64().unwrap_or(0);
    println!(
        "welcome: protocol={} api_revision={} client={}",
        welcome["protocol"], revision, welcome["client"]["id"]
    );

    let mut rpc = Rpc {
        ws: control,
        next_id: 1,
    };

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
    let ticket = rpc
        .call(
            "data.connect",
            json!({
                "session_id": session_id,
                "block_id": block_id,
                "data_ref_id": "terminal",
                "subprotocol": "rex.data.terminal.v1"
            }),
        )
        .await?["ticket"]
        .as_str()
        .ok_or_else(|| anyhow!("no ticket in data.connect result"))?
        .to_string();
    let ticket = URL_SAFE_NO_PAD.decode(ticket).context("decoding ticket")?;
    assert_eq!(ticket.len(), 16, "ticket must be 16 bytes");

    let mut data = connect(&args.socket, "/api/data", "rex.data.terminal.v1").await?;
    let mut hello = vec![0x01u8];
    hello.extend_from_slice(&ticket);
    data.send(Message::Binary(hello.into())).await?;

    let welcome = match data.next().await {
        Some(Ok(Message::Binary(b))) if b.len() == 32 && b[0] == 0x81 => b,
        Some(other) => bail!("bad data WELCOME: {other:?}"),
        None => bail!("data connection closed before WELCOME"),
    };
    let nonce = u64::from_le_bytes(welcome[24..32].try_into().unwrap());
    println!("data stream open (channel nonce {nonce})");

    // ---- stream until child_exited or the deadline ----
    let (tx, mut rx) = mpsc::channel::<i64>(1);
    let stream_path = format!("probe-stream-{}.bin", args.label);
    let stream_path_task = stream_path.clone();
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
