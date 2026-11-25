//! Integration check for `client.key` against a live Rex server (issue 4,
//! step 2). Run it manually:
//!
//! ```sh
//! mise exec -- cargo run -p rex-conn --example client_key
//! # force small paste chunks to exercise the chunking path:
//! REX_KEY_CHUNK_BYTES=8192 mise exec -- cargo run -p rex-conn --example client_key
//! # newline-heavy paste: each byte escapes to 2 JSON bytes, the case
//! # raw-byte chunking sent oversized (1009):
//! REX_PASTE_NEWLINES=614400 mise exec -- cargo run -p rex-conn --example client_key
//! ```
//!
//! It creates a session, runs `cat` in its shell, and verifies each kind of
//! key — plain text, Enter, an arrow key, a ctrl combo, and a multi-KB
//! paste — arrives, by reading the terminal back with
//! `com.superlogical.terminal.format`. Leaves the session running (label
//! `keytest-<pid>`); clean up with `rex kill keytest-<pid>`.

use anyhow::{bail, Result};
use rex_conn::{connect, hello, Rpc};
use serde_json::{json, Value};
use std::time::Duration;

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
fn key_event(key: &str, mods: &str, text: &str, unshifted_codepoint: i32) -> Value {
    json!({
        "key": key,
        "mods": mods,
        "consumed_mods": "",
        "text": text,
        "unshifted_codepoint": unshifted_codepoint,
        "composing": false,
        "action": "press",
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let socket = socket_path();
    let label = format!("keytest-{}", std::process::id());
    let ws = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (ws, welcome) = hello(
        ws,
        json!({"kind": "cli", "name": "client-key-example", "version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;
    let max_message_bytes = welcome["limits"]["max_message_bytes"]
        .as_u64()
        .unwrap_or(64 * 1024);
    let mut rpc = Rpc::new(ws, |_| {});

    println!("connected; max_message_bytes = {max_message_bytes}");

    // A fresh session with one shell block.
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
    let view = rpc
        .call("session.attach", json!({"session_id": session_id}))
        .await?;
    let block_id = view["view"]["windows"][0]["focused_block_id"]
        .as_str()
        .expect("focused block")
        .to_owned();
    println!("session {session_id}, block {block_id}");

    // Send one key via client.key and return `handled`.
    async fn send_key(rpc: &mut Rpc, session_id: &str, key: Value) -> Result<bool> {
        let result = rpc
            .call("client.key", json!({"session_id": session_id, "key": key}))
            .await?;
        Ok(result["handled"].as_bool().unwrap_or(false))
    }

    // Read the terminal back as text.
    async fn screen(rpc: &mut Rpc, session_id: &str, block_id: &str) -> Result<String> {
        let text = rpc
            .call(
                "com.superlogical.terminal.format",
                json!({"session_id": session_id, "block_id": block_id, "args": {"format": "text", "trim": true}}),
            )
            .await?;
        Ok(text["content"]
            .as_str()
            .expect("content in format result")
            .to_owned())
    }

    async fn wait_output() {
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    // ---- run cat so typed keys echo back through the block ----
    send_key(&mut rpc, &session_id, key_event("key_c", "", "c", 99)).await?;
    send_key(&mut rpc, &session_id, key_event("key_a", "", "a", 97)).await?;
    send_key(&mut rpc, &session_id, key_event("key_t", "", "t", 116)).await?;
    send_key(&mut rpc, &session_id, key_event("enter", "", "", 13)).await?;
    wait_output().await;

    // ---- plain text in one multi-character call, then Enter ----
    send_key(
        &mut rpc,
        &session_id,
        key_event("key_h", "", "hello-from-client-key", 104),
    )
    .await?;
    send_key(&mut rpc, &session_id, key_event("enter", "", "", 13)).await?;
    wait_output().await;
    let text = screen(&mut rpc, &session_id, &block_id).await?;
    if !text.contains("hello-from-client-key") {
        bail!("plain text did not arrive; screen:\n{text}");
    }
    println!("PASS plain text (multi-char, one call)");

    // ---- multi-KB paste, chunked under max_message_bytes ----
    // Default chunking keeps one chunk's JSON under the server's limit;
    // a small REX_KEY_CHUNK_BYTES forces several chunks to exercise the
    // path. The budget is the chunk's *encoded* size: serde_json escapes
    // control characters, so newline-heavy paste expands in JSON.
    let chunk_bytes = std::env::var("REX_KEY_CHUNK_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| (max_message_bytes.saturating_sub(1024) as usize).max(1024));
    let paste_end_marker;
    let paste = match std::env::var("REX_PASTE_NEWLINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        // Newline-heavy paste: every byte escapes to 2 JSON bytes, the
        // regression case where raw-byte chunking oversized the request.
        Some(nl) => {
            paste_end_marker = "paste-newlines-done".to_string();
            format!("{}\n{}\n", "\n".repeat(nl), paste_end_marker)
        }
        None => {
            paste_end_marker = "paste-line-04999".to_string();
            let lines: Vec<String> = (0..5000).map(|i| format!("paste-line-{i:05}")).collect();
            lines.join("\n")
        }
    };
    println!(
        "pasting {} bytes in chunks of up to {} JSON bytes",
        paste.len(),
        chunk_bytes.min(paste.len())
    );
    let mut chunks = 0;
    for chunk in rex_conn::chunk_json_safe(&paste, chunk_bytes) {
        chunks += 1;
        let handled = send_key(
            &mut rpc,
            &session_id,
            key_event("unidentified", "", chunk, 0),
        )
        .await?;
        if handled {
            bail!("a Lua binding ate the paste chunk");
        }
    }
    println!("sent {chunks} paste chunks");
    send_key(&mut rpc, &session_id, key_event("enter", "", "", 13)).await?;
    // cat can take a while to echo a very large paste, so poll for the
    // end marker instead of one fixed sleep.
    let mut text = String::new();
    for _ in 0..120 {
        wait_output().await;
        text = screen(&mut rpc, &session_id, &block_id).await?;
        if text.contains(&paste_end_marker) {
            break;
        }
    }
    if !text.contains(&paste_end_marker) {
        bail!(
            "paste did not arrive whole; screen tail:\n{}",
            text.chars()
                .rev()
                .take(2000)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
    }
    println!("PASS multi-KB paste ({chunks} chunks, end marker present)");

    // ---- ctrl combo: ctrl+d ends cat, the shell returns ----
    send_key(&mut rpc, &session_id, key_event("key_d", "ctrl", "", 100)).await?;
    wait_output().await;
    send_key(
        &mut rpc,
        &session_id,
        key_event("key_e", "", "echo ctrl-d-worked", 101),
    )
    .await?;
    send_key(&mut rpc, &session_id, key_event("enter", "", "", 13)).await?;
    wait_output().await;
    let text = screen(&mut rpc, &session_id, &block_id).await?;
    if !text.contains("ctrl-d-worked") {
        bail!("ctrl+d did not reach the block; screen:\n{text}");
    }
    println!("PASS ctrl combo (ctrl+d ended cat, shell ran the echo)");

    // ---- arrow key, at a reader that interprets it: bash readline ----
    // `cat` reads in canonical mode, where an arrow is just literal
    // bytes (the same happens in a local terminal). `read -e` runs
    // readline in raw mode, which moves the cursor on arrow_left.
    send_key(
        &mut rpc,
        &session_id,
        key_event("key_b", "", "bash -c 'read -e x; echo GOT:$x'", 98),
    )
    .await?;
    send_key(&mut rpc, &session_id, key_event("enter", "", "", 13)).await?;
    wait_output().await;
    send_key(&mut rpc, &session_id, key_event("key_a", "", "abc", 97)).await?;
    send_key(&mut rpc, &session_id, key_event("arrow_left", "", "", 0)).await?;
    send_key(&mut rpc, &session_id, key_event("arrow_left", "", "", 0)).await?;
    send_key(&mut rpc, &session_id, key_event("key_x", "", "X", 120)).await?;
    send_key(&mut rpc, &session_id, key_event("enter", "", "", 13)).await?;
    wait_output().await;
    let text = screen(&mut rpc, &session_id, &block_id).await?;
    if !text.contains("GOT:aXbc") {
        bail!("arrow key did not move the cursor; screen:\n{text}");
    }
    println!("PASS arrow key (readline moved the cursor: 'GOT:aXbc')");

    // Leave the session for inspection; `rex kill {label}` cleans up.
    println!("all checks passed; session label: {label}");
    Ok(())
}
