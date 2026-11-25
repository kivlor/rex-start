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

/// Splits `text` into chunks of at most `max_bytes` without breaking a
/// character: paste text is UTF-8, and a chunk boundary inside a code
/// point would send invalid bytes to the terminal. Used to keep each
/// `client.key` request under the welcome's `limits.max_message_bytes`.
pub fn chunk_utf8<'a>(text: &'a str, max_bytes: usize) -> impl Iterator<Item = &'a str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len().min(max_bytes);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        // A single character wider than max_bytes still goes out whole:
        // the caller guarantees max_bytes fits a request, and one code
        // point never breaks that by much. Cutting it would loop forever.
        if end == 0 {
            end = rest.chars().next().map(char::len_utf8).unwrap_or(0);
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
}
