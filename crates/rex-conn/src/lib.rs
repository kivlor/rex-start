//! Shared Rex protocol plumbing: WebSockets over Unix sockets, the
//! control connection's hello/welcome handshake, one-at-a-time RPC, and
//! the data connection's ticket handshake.
//!
//! Everything here is lifted from `crates/probe`, which proved the paths
//! against the real server; the probe now uses this crate so the app and
//! the probe share one implementation (see docs/API-rev19.md).

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Per-character encoded length of `c` inside a serde_json string
/// literal: `"`, `\\` and the two short escapes (`\n`, `\t`, `\r`,
/// `\b`, `\f`) cost 2 bytes, other control chars (< 0x20) become a
/// 6-byte `\u00XX`, and everything else costs its UTF-8 byte length.
const fn json_escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\t' | '\r' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Splits `text` into chunks whose JSON string-literal encoding stays
/// within `max_json_bytes`, never breaking a character. Paste goes out
/// through `client.key` as JSON, and serde_json escapes `"`, `\\` and
/// control characters — sizing chunks by raw UTF-8 bytes would let a
/// newline-heavy payload expand past the welcome's
/// `limits.max_message_bytes` and close the control connection (1009).
/// This counts each character at its escaped length instead. A single
/// character wider than the whole budget still goes out alone: cutting
/// it would loop forever, and one code point never breaks a budget
/// that fits a request by much.
pub fn chunk_json_safe(text: &str, max_json_bytes: usize) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        // Greedy: keep appending characters while the chunk's encoded
        // length (plus the surrounding quotes) stays within budget.
        let mut used = 2; // opening + closing quote of the JSON string
        let mut end = 0;
        for c in rest.chars() {
            let cost = json_escaped_len(c);
            if used + cost > max_json_bytes && end > 0 {
                break;
            }
            used += cost;
            end += c.len_utf8();
        }
        // A single character larger than the whole budget goes out
        // alone rather than looping forever on empty chunks.
        if end == 0 {
            end = first.len_utf8();
        }
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        Some(chunk)
    })
}

pub type Ws = WebSocketStream<UnixStream>;

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
pub async fn connect(path: &str, uri_path: &str, subprotocol: &str) -> Result<Ws> {
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

/// One JSON message on a connection.
pub async fn send_json(ws: &mut Ws, v: &Value) -> Result<()> {
    Ok(ws.send(Message::Text(v.to_string().into())).await?)
}

/// Read the next text message as JSON.
pub async fn recv_json(ws: &mut Ws) -> Result<Value> {
    match ws.next().await {
        Some(Ok(Message::Text(s))) => Ok(serde_json::from_str(&s)?),
        Some(Ok(m)) => bail!("unexpected control message: {m:?}"),
        Some(Err(e)) => Err(e.into()),
        None => bail!("connection closed"),
    }
}

/// The control connection's opening handshake: hello, then welcome.
///
/// `client` becomes the hello's `client` field, such as
/// `{"kind": "cli", "name": "probe", "version": "0.1.0"}`.
/// Returns the welcome message.
pub async fn hello(mut ws: Ws, client: Value) -> Result<(Ws, Value)> {
    send_json(
        &mut ws,
        &json!({
            "type": "hello",
            "api_revision": 19,
            "client": client
        }),
    )
    .await?;
    let welcome = recv_json(&mut ws).await?;
    if welcome["type"] != "welcome" {
        bail!("expected welcome, got: {welcome}");
    }
    Ok((ws, welcome))
}

/// A control-connection RPC: one request at a time, each waiting for its
/// response, with events that arrive in between handed to `on_event`.
/// The layout/focus methods are serialized server-side ("Order of
/// Requests" in the API doc), so one Rpc at a time is also the right
/// shape for calling them.
pub struct Rpc {
    pub ws: Ws,
    next_id: u64,
    on_event: Box<dyn FnMut(&Value) + Send>,
}

impl Rpc {
    pub fn new(ws: Ws, on_event: impl FnMut(&Value) + Send + 'static) -> Self {
        Self {
            ws,
            next_id: 1,
            on_event: Box::new(on_event),
        }
    }

    /// Send a request and wait for its response, passing any events that
    /// arrive first to the event callback. Never used for two calls at
    /// once, so ids stay unique.
    pub async fn call(&mut self, method: &str, payload: Value) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        send_json(
            &mut self.ws,
            &json!({"type": "request", "id": id, "method": method, "payload": payload}),
        )
        .await?;
        let mut msg = tokio::time::timeout(Duration::from_secs(10), recv_json(&mut self.ws))
            .await
            .map_err(|_| anyhow!("timeout waiting for response to {method} (id {id})"))??;
        // Events can arrive between request and response.
        while msg["type"] != "response" || msg["id"] != json!(id) {
            if msg["type"] == "event" {
                (self.on_event)(&msg["payload"]);
            } else if msg["type"] == "response" {
                bail!("response with wrong id for {method}: {}", msg);
            }
            msg = recv_json(&mut self.ws).await?;
        }
        if let Some(err) = response_error(&msg) {
            bail!("{method} failed: {}", err);
        }
        Ok(msg["payload"].clone())
    }
}

/// The `error` field of a response, if the server reported one. JSON
/// `null` means success: only a present, non-null error fails the call.
fn response_error(msg: &Value) -> Option<&Value> {
    msg.get("error").filter(|err| !err.is_null())
}

/// A one-use `data.connect` ticket, decoded to its 16 bytes.
pub async fn data_ticket(rpc: &mut Rpc, session_id: &str, block_id: &str) -> Result<Vec<u8>> {
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
    check_ticket(ticket)
}

/// A `data.connect` ticket is 16 raw bytes. The server controls the
/// string, so a wrong length is an error the caller can show, not a
/// panic.
fn check_ticket(bytes: Vec<u8>) -> Result<Vec<u8>> {
    if bytes.len() != 16 {
        bail!("ticket must be 16 bytes, got {}", bytes.len());
    }
    Ok(bytes)
}

/// Opens a terminal data connection: WS at /api/data, binary HELLO with the
/// ticket, WELCOME back. Returns the stream and its channel nonce, which
/// rex1 APC messages must carry.
pub async fn open_data(path: &str, ticket: &[u8]) -> Result<(Ws, u64)> {
    let mut ws = connect(path, "/api/data", "rex.data.terminal.v1").await?;
    let mut hello = vec![0x01u8];
    hello.extend_from_slice(ticket);
    ws.send(Message::Binary(hello.into())).await?;
    let welcome = match ws.next().await {
        Some(Ok(Message::Binary(b))) if b.len() == 32 && b[0] == 0x81 => b,
        Some(other) => bail!("bad data WELCOME: {other:?}"),
        None => bail!("data connection closed before WELCOME"),
    };
    let nonce = u64::from_le_bytes(welcome[24..32].try_into().unwrap());
    Ok((ws, nonce))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_error_is_success() {
        assert!(response_error(&json!({"type": "response", "error": null})).is_none());
        assert!(response_error(&json!({"type": "response"})).is_none());
    }

    #[test]
    fn present_error_fails_the_call() {
        let msg = json!({"type": "response", "error": {"code": 4}});
        assert!(response_error(&msg).is_some());
    }

    #[test]
    fn ticket_length_is_an_error_not_a_panic() {
        assert_eq!(check_ticket(vec![0u8; 16]).unwrap().len(), 16);
        assert!(check_ticket(Vec::new()).is_err());
        assert!(check_ticket(vec![0u8; 17]).is_err());
    }

    #[test]
    fn chunk_json_safe_splits_on_char_boundaries() {
        let text = "\u{e9}abc\u{1f600}def";
        let chunks: Vec<&str> = chunk_json_safe(text, 5).collect();
        assert_eq!(chunks, vec!["\u{e9}a", "bc", "\u{1f600}", "def"]);
    }

    #[test]
    fn chunk_json_safe_empty_and_huge() {
        assert_eq!(chunk_json_safe("", 100).count(), 0);
        let huge = "x".repeat(10_000);
        assert_eq!(
            chunk_json_safe(&huge, 1024).map(str::len).sum::<usize>(),
            10_000
        );
        assert!(chunk_json_safe(&huge, 1024).all(|c| c.len() <= 1024));
    }

    #[test]
    fn chunk_json_safe_wide_char_in_narrow_chunk() {
        // A single character wider than the chunk still goes out whole
        // rather than looping on empty chunks.
        let chunks: Vec<&str> = chunk_json_safe("a\u{1f600}b", 4).collect();
        assert_eq!(chunks, vec!["a", "\u{1f600}", "b"]);
    }

    #[test]
    fn chunk_json_safe_counts_escapes_not_raw_bytes() {
        // Newlines cost 2 bytes each in JSON, so a 4-byte "\n\n" chunk
        // with a 6-byte budget is full, while 6 plain 'a's fit in 8.
        let chunks: Vec<&str> = chunk_json_safe("\n\n\n", 6).collect();
        assert_eq!(chunks, vec!["\n\n", "\n"]);
        let chunks: Vec<&str> = chunk_json_safe("aaaaaaaa", 8).collect();
        assert_eq!(chunks, vec!["aaaaaa", "aa"]);
    }

    #[test]
    fn chunk_json_safe_encoded_length_within_budget() {
        // The property the paste path depends on: every chunk's JSON
        // string encoding (quotes included) stays within the budget,
        // even for control-char-heavy input that raw-byte chunking
        // would oversell by up to 6x.
        let mut text = String::new();
        for i in 0..20_000 {
            text.push(if i % 2 == 0 { '\n' } else { 'x' });
            if i % 7 == 3 {
                text.push('\u{1}');
            }
        }
        assert!(text.len() > 10_000);
        for chunk in chunk_json_safe(&text, 4096) {
            let encoded = serde_json::to_string(chunk).unwrap();
            assert!(
                encoded.len() <= 4096,
                "chunk encoded to {} bytes > 4096",
                encoded.len()
            );
        }
    }
}
