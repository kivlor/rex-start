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

/// Read the next text message as JSON, skipping websocket control
/// frames: the server pings every 120s (tungstenite answers them as it
/// reads them), and a ping arriving between request and response must
/// not fail an RPC in flight.
pub async fn recv_json(ws: &mut Ws) -> Result<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(s))) => return Ok(serde_json::from_str(&s)?),
            Some(Ok(Message::Close(frame))) => bail!("connection closed: {frame:?}"),
            Some(Ok(Message::Binary(b))) => {
                bail!("unexpected binary message ({} bytes)", b.len())
            }
            // Ping/Pong and raw frames: control traffic, not JSON.
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => bail!("connection closed"),
        }
    }
}

/// The control connection's opening handshake: hello, then welcome.
///
/// `client` becomes the hello's `client` field, such as
/// `{"kind": "cli", "name": "probe", "version": "0.1.0"}`.
/// Returns the welcome message.
pub async fn hello(ws: Ws, client: Value) -> Result<(Ws, Value)> {
    hello_with_token(ws, client, None).await
}

/// The hello handshake, optionally carrying the `reconnect_token` from
/// the last welcome to continue that client ("Reconnecting" in the API
/// doc). A good token keeps the client id and sets `resumed: true` in
/// the new welcome; an unknown or expired one silently starts a new
/// client. Never log the token or the welcome's `client` object.
pub async fn hello_with_token(
    mut ws: Ws,
    client: Value,
    reconnect_token: Option<&str>,
) -> Result<(Ws, Value)> {
    let mut hello = json!({
        "type": "hello",
        "api_revision": 19,
        "client": client
    });
    if let Some(token) = reconnect_token {
        hello["reconnect_token"] = Value::from(token);
    }
    send_json(&mut ws, &hello).await?;
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

/// A 16-byte data-connection reconnect token. The WELCOME carries a
/// new one each time; only the latest is good for a RESUME. It is a
/// bearer secret, so `Debug` never shows it ("Never log it").
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReconnectToken([u8; 16]);

impl std::fmt::Debug for ReconnectToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReconnectToken([redacted; 16 bytes])")
    }
}

impl ReconnectToken {
    /// The token from a WELCOME's bytes 1..=16 (the `0x81` prefix at 0).
    fn from_welcome(welcome: &[u8]) -> Result<Self> {
        Ok(Self(
            welcome
                .get(1..17)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| anyhow!("WELCOME too short for a reconnect token"))?,
        ))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// What a data connection's handshake answered: the WELCOME's new
/// reconnect token, retention window, and channel nonce — or the
/// REJECT's one-byte reason.
#[derive(Debug)]
pub enum DataHandshake {
    Welcome(Box<Ws>, DataWelcome),
    Rejected(u8),
}

/// The parsed 32-byte data WELCOME. Its `Debug` is safe: the token
/// redacts itself.
#[derive(Clone, Copy, Debug)]
pub struct DataWelcome {
    pub token: ReconnectToken,
    /// How long the server keeps the stream for a resume, in ms.
    pub retention_ms: u32,
    /// The channel nonce rex1 messages must carry; never zero, and the
    /// same across resumes of one stream.
    pub nonce: u64,
}

/// Sends `msg` (HELLO or RESUME) on a fresh `/api/data` socket and reads
/// the one-binary answer: WELCOME (with a NEW token — keep the newest)
/// or REJECT (the server then closes with `data handshake rejected`).
async fn handshake(path: &str, msg: Vec<u8>) -> Result<DataHandshake> {
    let mut ws = connect(path, "/api/data", "rex.data.terminal.v1").await?;
    ws.send(Message::Binary(msg.into())).await?;
    match ws.next().await {
        Some(Ok(Message::Binary(b))) if b.len() == 32 && b[0] == 0x81 => {
            Ok(DataHandshake::Welcome(
                Box::new(ws),
                DataWelcome {
                    token: ReconnectToken::from_welcome(&b)?,
                    retention_ms: u32::from_le_bytes(b[20..24].try_into().unwrap()),
                    nonce: u64::from_le_bytes(b[24..32].try_into().unwrap()),
                },
            ))
        }
        Some(Ok(Message::Binary(b))) if b.len() == 2 && b[0] == 0x82 => {
            Ok(DataHandshake::Rejected(b[1]))
        }
        Some(other) => bail!("bad data handshake answer: {other:?}"),
        None => bail!("data connection closed during handshake"),
    }
}

/// Opens a terminal data connection: WS at /api/data, binary HELLO with
/// the ticket, WELCOME back. The WELCOME's reconnect token starts the
/// stream's resume state; every later WELCOME replaces it.
pub async fn open_data(path: &str, ticket: &[u8]) -> Result<(Ws, DataWelcome)> {
    let mut msg = vec![0x01u8];
    msg.extend_from_slice(ticket);
    match handshake(path, msg).await? {
        DataHandshake::Welcome(ws, welcome) => Ok((*ws, welcome)),
        DataHandshake::Rejected(reason) => {
            bail!("data HELLO rejected: reason 0x{reason:02x}")
        }
    }
}

/// A 25-byte RESUME: `0x02`, the 16 bytes of the latest reconnect
/// token, then the exact number of stream bytes received so far as a
/// u64 little-endian. A resume needs no ticket and no control
/// connection.
fn encode_resume(token: ReconnectToken, bytes: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(25);
    msg.push(0x02);
    msg.extend_from_slice(token.as_bytes());
    msg.extend_from_slice(&bytes.to_le_bytes());
    msg
}

/// Opens a new `/api/data` connection and tries to resume one stream:
/// RESUME with the latest token and the exact byte count. On a WELCOME
/// the stream continues from the first byte not received; on a REJECT
/// the caller decides per the reason table (0x01/0x03/0x05 → start over
/// with a new ticket; 0x02 → RESUME again; anything else → failure).
pub async fn resume_data(path: &str, token: ReconnectToken, bytes: u64) -> Result<DataHandshake> {
    handshake(path, encode_resume(token, bytes)).await
}

/// The authoritative per-stream resume state (issue #6: "keep a single
/// authoritative counter where frames are consumed"). `bytes` counts
/// every stream byte used, across every connection of the stream, NOT
/// counting handshakes (the WELCOME is a handshake message, and
/// tungstenite hands over each payload only when `next()` returns it, so
/// what the consumer processes is exactly what was used — no
/// read-ahead). `token` is always the newest one — each WELCOME
/// replaces it, and only the newest resumes.
#[derive(Debug, Default)]
pub struct StreamResume {
    token: Option<ReconnectToken>,
    bytes: u64,
}

impl StreamResume {
    /// Records a fresh WELCOME: adopts its NEW token (and returns the
    /// nonce/retention the caller also needs from it).
    pub fn welcomed(&mut self, welcome: DataWelcome) -> DataWelcome {
        self.token = Some(welcome.token);
        welcome
    }

    /// Records a fresh WELCOME from a NEW stream (the start-over path:
    /// new ticket + HELLO): adopts its token and starts the count over,
    /// because the new stream begins with a new snapshot.
    pub fn restarted(&mut self, welcome: DataWelcome) -> DataWelcome {
        self.token = Some(welcome.token);
        self.bytes = 0;
        welcome
    }

    /// Counts one consumed stream payload — call exactly once per
    /// processed binary message, with its byte length.
    pub fn consume(&mut self, len: usize) {
        self.bytes += len as u64;
    }

    /// The exact byte count for a RESUME.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The latest token, if any WELCOME has been seen.
    pub fn token(&self) -> Option<ReconnectToken> {
        self.token
    }

    /// Builds this stream's RESUME message; `None` before any WELCOME.
    pub fn resume_message(&self) -> Option<Vec<u8>> {
        Some(encode_resume(self.token?, self.bytes))
    }
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

    /// A DataWelcome straight from a synthetic 32-byte WELCOME.
    fn welcome_with(token_byte: u8) -> DataWelcome {
        let mut b = vec![0x81u8];
        b.extend(std::iter::repeat_n(token_byte, 16));
        b.extend([0, 0, 0]);
        b.extend(60_000u32.to_le_bytes());
        b.extend(0xdead_beefu64.to_le_bytes());
        DataWelcome {
            token: ReconnectToken::from_welcome(&b).unwrap(),
            retention_ms: u32::from_le_bytes(b[20..24].try_into().unwrap()),
            nonce: u64::from_le_bytes(b[24..32].try_into().unwrap()),
        }
    }

    #[test]
    fn resume_message_is_25_bytes_le() {
        let mut state = StreamResume::default();
        assert!(state.resume_message().is_none());
        state.welcomed(welcome_with(0x7f));
        state.consume(256);
        state.consume(3);
        assert_eq!(state.bytes(), 259);
        let msg = state.resume_message().unwrap();
        assert_eq!(msg.len(), 25);
        assert_eq!(msg[0], 0x02);
        assert_eq!(&msg[1..17], &[0x7fu8; 16]);
        // The count is a little-endian u64.
        assert_eq!(u64::from_le_bytes(msg[17..25].try_into().unwrap()), 259);
        assert_eq!(&msg[17..25], &259u64.to_le_bytes());
    }

    #[test]
    fn stream_resume_counts_across_reconnects_and_new_tokens_win() {
        let mut state = StreamResume::default();
        state.welcomed(welcome_with(1));
        let chunks = [7usize, 0, 1, 65536, 3];
        for c in chunks {
            state.consume(c);
        }
        let total: u64 = chunks.iter().map(|&c| c as u64).sum();
        assert_eq!(state.bytes(), total);
        // A resume's WELCOME carries a NEW token: the newest must win.
        state.welcomed(welcome_with(2));
        let msg = state.resume_message().unwrap();
        assert_eq!(&msg[1..17], &[2u8; 16]);
        // ...and the count carries over: every consumed byte, every
        // connection.
        assert_eq!(
            u64::from_le_bytes(msg[17..25].try_into().unwrap()),
            state.bytes()
        );
        state.consume(5);
        assert_eq!(state.bytes(), total + 5);
    }

    #[test]
    fn reconnect_token_debug_never_leaks() {
        let token = welcome_with(0x41).token;
        let printed = format!("{token:?}");
        assert_eq!(printed, "ReconnectToken([redacted; 16 bytes])");
        assert!(!printed.contains('A'));
        // Deriving Debug on StreamResume must not leak either.
        let state = StreamResume::default();
        assert!(!format!("{state:?}").contains('A'));
    }

    #[test]
    fn restarted_resets_the_count_for_a_new_stream() {
        let mut state = StreamResume::default();
        state.welcomed(welcome_with(1));
        state.consume(1234);
        // The start-over path: a new ticket + HELLO gives a new stream
        // whose bytes begin at zero, with the newest token.
        state.restarted(welcome_with(9));
        assert_eq!(state.bytes(), 0);
        let msg = state.resume_message().unwrap();
        assert_eq!(&msg[1..17], &[9u8; 16]);
        assert_eq!(u64::from_le_bytes(msg[17..25].try_into().unwrap()), 0);
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
