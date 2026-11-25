//! The client actor: everything that talks to the Rex server, run on its
//! own tokio runtime thread. The UI sends [`ClientCmd`]s and receives
//! [`ClientEvent`]s over async-channel, which both the tokio and GPUI
//! executors can await. Layout and focus methods are serialized by the
//! server ("Order of Requests"), and by this actor too: commands are
//! handled one at a time, each call waiting for its response.
//!
//! Closing the window drops the command channel, which ends the actor and
//! closes the sockets. Nothing here ever destroys a session: sessions and
//! their programs keep running server-side.

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use rex_conn::{connect, hello, Rpc};
use rex_terminal::Replica;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::view::{Attached, Bootstrap, SessionBrief, SessionView};

/// What the UI asks the server to do.
#[derive(Clone, Debug)]
pub enum ClientCmd {
    /// `session.new_split`: a shell block beside the focused block.
    NewSplit { direction: Direction },
    /// `session.zoom_block`: zoom the focused block, or undo the zoom.
    ToggleZoom,
    /// `session.focus_block`: focus one block of the active window.
    FocusBlock(String),
    /// Attach to a session and make it the one the window shows.
    SwitchSession(String),
    /// `session.create`, then attach to the new session.
    NewSession(String),
    /// `client.key`: one key press, giving the server's Lua bindings
    /// first chance at it. Ordered server-side, serialized here too.
    SendKey {
        key: KeyEvent,
        /// Earlier keys when `key` ends a sequence, e.g. `Some(["ctrl+b"])`.
        /// UNUSED so far: the UI never waits on sequences (the client's
        /// job per the keymap notes), so this is always None — kept
        /// plumbed for sequence support, which issue #5/#6 picks up.
        prefix: Option<Vec<String>>,
    },
    /// `client.key` in chunks: paste as text, split to stay under the
    /// server's `limits.max_message_bytes`. One call per chunk.
    Paste(String),
    /// `session.own_block` (mode take) + `com.superlogical.terminal.resize`
    /// with a grid that fits the pane: claim the block, then size it for
    /// this window. Coalesced actor-side: a repeat of the block's current
    /// grid sends nothing.
    OwnBlock {
        block_id: String,
        cols: u32,
        rows: u32,
    },
    /// `session.disown_block`: give up every claim in the current
    /// session, handing sizing back without killing the block.
    DisownAll,
}

/// One keyboard event for `client.key`: physical key name (W3C code in
/// snake_case, `unidentified` for synthesized text), mods joined with `+`,
/// the text it typed, the mods the layout consumed to produce that text,
/// and its unshifted code point for layout-dependent bindings such as
/// `ctrl+n`.
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub key: String,
    pub mods: String,
    pub text: String,
    /// The modifiers the keyboard layout used to produce `text` (the
    /// schema's `consumed_mods`), e.g. "alt" for an AltGr composition.
    /// Empty for chords that type nothing and for plain text.
    pub consumed_mods: String,
    pub unshifted_codepoint: i32,
}

impl KeyEvent {
    /// A key press as `client.key`'s `key` object. `composing` is false
    /// (GPUI gives us finished compositions).
    fn as_json(&self) -> Value {
        json!({
            "key": self.key,
            "mods": self.mods,
            "consumed_mods": self.consumed_mods,
            "text": self.text,
            "unshifted_codepoint": self.unshifted_codepoint,
            "composing": false,
            "action": "press",
        })
    }
}

/// How a split divides the anchor's space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Horizontal,
    Vertical,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Horizontal => "horizontal",
            Direction::Vertical => "vertical",
        }
    }
}

/// What the actor tells the UI.
#[derive(Clone, Debug)]
pub enum ClientEvent {
    /// The control connection is up; here's the session list and, if the
    /// server attached one, the session the window should show.
    Connected {
        sessions: Vec<SessionBrief>,
        current: Option<(SessionBrief, SessionView)>,
    },
    /// The session list changed.
    Sessions(Vec<SessionBrief>),
    /// The session the window is showing, and its live data streams.
    View(SessionBrief, SessionView),
    /// The session the window shows is gone from the server.
    CurrentGone,
    /// A block's latest rendered screen.
    Frame(String, Arc<rex_terminal::Frame>),
    /// Something worth a line in the status bar.
    Notice(String),
    /// The keys the server's resolved keymap still binds to the app's own
    /// actions, in binding spelling ("ctrl+alt+z" — the same form the UI's
    /// `binding_key` produces). The UI handles only these locally; every
    /// other key flows to `client.key`, so a Lua binding gets its chance.
    /// Sent after the `client.keymap` declaration and after every
    /// `keymap_changed` that changes the config hash.
    Keymap(HashSet<String>),
    /// The control connection ended; the UI can say so.
    Closed(String),
}

/// A running client: UI side of the two channels.
pub struct Client {
    pub cmd: async_channel::Sender<ClientCmd>,
    pub events: async_channel::Receiver<ClientEvent>,
}

/// Starts the actor on its own thread and runtime. `preferred` is a
/// session label or ID to attach first, or none to let the server pick.
pub fn start(socket: String, preferred: Option<String>, new_session: bool) -> Result<Client> {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<ClientCmd>();
    let (event_tx, event_rx) = async_channel::unbounded::<ClientEvent>();
    let builder = std::thread::Builder::new().name("rex-client".into());
    builder.spawn(move || {
        // Built inside the thread, but a failure is reported instead of
        // panicking: without this, the window would hang on "connecting"
        // forever with nothing to say why.
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = event_tx.try_send(ClientEvent::Closed(format!(
                    "client runtime failed: {error}"
                )));
                return;
            }
        };
        // Replicas hold libghostty iterators that are not Send, so stream
        // tasks run on a LocalSet on this thread instead of the pool.
        let local = tokio::task::LocalSet::new();
        // The runtime is dropped here, after block_on returns — never from
        // within the async context it drives.
        let _ = runtime.block_on(local.run_until(run(
            socket,
            preferred,
            new_session,
            cmd_rx,
            event_tx,
        )));
    })?;
    Ok(Client {
        cmd: cmd_tx,
        events: event_rx,
    })
}

/// The app's own actions, exactly as `declare_keymap` declares them.
const OUR_ACTIONS: [&str; 4] = ["app.split", "app.zoom", "app.picker", "app.new_session"];

/// The default layer's key bindings, in the spelling `normalize_key`
/// produces and the UI's `binding_key` matches.
const DEFAULT_LOCAL_KEYS: [&str; 5] = [
    "ctrl+alt+right",
    "ctrl+alt+down",
    "ctrl+alt+z",
    "ctrl+alt+p",
    "ctrl+alt+n",
];

/// The keys the UI handles locally when the keymap is unknown or the
/// `client.keymap` call failed: every declared default.
pub fn default_local_keys() -> HashSet<String> {
    DEFAULT_LOCAL_KEYS.iter().map(|k| k.to_string()).collect()
}

/// Canonical spelling for one key of a keymap binding: mods in the
/// protocol's order (shift, ctrl, alt, super), then the key, all
/// lowercase — so `Alt+Ctrl+Z` and `ctrl+alt+z` agree, and `cmd` maps
/// to `super`. None for anything that isn't one plain key.
pub fn normalize_key(key: &str) -> Option<String> {
    let mut shift = false;
    let mut ctrl = false;
    let mut alt = false;
    let mut super_ = false;
    let mut name: Option<String> = None;
    for token in key.split('+') {
        match token.trim().to_lowercase().as_str() {
            // A trailing separator ("ctrl+") or an empty token is not a
            // key spelling.
            "" => return None,
            "shift" => shift = true,
            "ctrl" | "control" => ctrl = true,
            "alt" | "altgr" => alt = true,
            "super" | "cmd" | "meta" => super_ = true,
            other => {
                if name.replace(other.to_owned()).is_some() {
                    // More than one key token: not one plain key.
                    return None;
                }
            }
        }
    }
    let name = name?;
    let mut parts: Vec<&str> = Vec::new();
    if shift {
        parts.push("shift");
    }
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if super_ {
        parts.push("super");
    }
    parts.push(&name);
    Some(parts.join("+"))
}

/// Which of the declared default keys the resolved keymap still binds to
/// our own actions. A binding the config or a Lua script took over comes
/// back as `client.key.forward` — always available, the bindings below
/// it on that key never run — so such a key leaves the set, as does a
/// key the config removed or bound elsewhere. Bindings in a mode or
/// key sequences (more than one element) wait on the server instead.
fn resolved_local_keys(bindings: &Value) -> HashSet<String> {
    let list: &[serde_json::Value] = bindings.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    DEFAULT_LOCAL_KEYS
        .iter()
        .filter(|default| key_is_ours(list, default))
        .map(|default| default.to_string())
        .collect()
}

/// Is one default key still bound to our actions in the resolved list?
fn key_is_ours(bindings: &[Value], default: &str) -> bool {
    let mut first_action: Option<&str> = None;
    for binding in bindings {
        let Some(keys) = binding["keys"].as_array() else {
            continue;
        };
        // A sequence's first key is not a binding of its own: the client
        // waits on the server, so don't claim it.
        if keys.len() != 1 {
            continue;
        }
        let Some(key) = keys[0].as_str().and_then(normalize_key) else {
            continue;
        };
        if key != default {
            continue;
        }
        // Mode bindings apply only while that mode is active, which the
        // app doesn't track; ignore them and keep looking.
        if binding.get("mode").is_some_and(|m| !m.is_null()) {
            continue;
        }
        let action = binding["action"].as_str();
        // A Lua binding wins regardless of order: always available.
        if action == Some("client.key.forward") {
            return false;
        }
        if first_action.is_none() {
            first_action = action;
        }
    }
    first_action.is_some_and(|action| OUR_ACTIONS.contains(&action))
}

/// The session the window is showing, plus its live data streams.
struct Current {
    session: SessionBrief,
    view: SessionView,
    /// One task per streaming block, keyed by block ID.
    streams: HashMap<String, tokio::task::JoinHandle<()>>,
}

struct Actor {
    socket: String,
    rpc: Rpc,
    events: async_channel::Sender<ClientEvent>,
    sessions: Vec<SessionBrief>,
    current: Option<Current>,
    /// The welcome's `limits.max_message_bytes`: any larger request would
    /// close the connection (status 1009), so paste chunks under it.
    max_message_bytes: u64,
    /// The config hash of the last `client.keymap` result; a
    /// `keymap_changed` event with a different hash re-fetches it.
    keymap_hash: Option<String>,
    /// This client's ID, from `session.bootstrap`. Ownership is exclusive:
    /// it decides which blocks in `owners` are ours.
    my_client_id: String,
    /// Block ID -> owner in the current session, as own_block/disown_block
    /// results and `block_owner_changed` events report it. `None` = no
    /// owner. The view doesn't carry owners, so this map is the truth.
    owners: HashMap<String, Option<String>>,
    /// The last grid each block was resized to, so repeats are coalesced.
    /// Cleared when a block's owner changes away from us.
    last_grid: HashMap<String, (u32, u32)>,
}

/// Set `REX_APP_DEBUG=1` to see the actor's protocol progress on stderr:
/// useful for smoke tests, since the UI is the only other witness.
macro_rules! debug {
    ($($arg:tt)*) => {
        if std::env::var_os("REX_APP_DEBUG").is_some() {
            eprintln!("[rex-app] {}", format_args!($($arg)*));
        }
    };
}

/// Connects, handshakes, bootstraps, and attaches the preferred session:
/// everything before the main loop. The caller turns any failure into
/// `ClientEvent::Closed`, so the window never hangs on "connecting".
async fn start_actor(
    socket: &str,
    preferred: Option<&str>,
    events: async_channel::Sender<ClientEvent>,
) -> Result<(Actor, mpsc::UnboundedReceiver<Value>)> {
    let ws = connect(socket, "/api/control", "rex.control.v1").await?;
    let (ws, _welcome) = hello(
        ws,
        json!({"kind": "app", "name": "rex-app", "version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;
    // Requests larger than this close the connection, so keep every
    // paste chunk's whole JSON message under it. 1 MiB here; every
    // server guarantees at least 64 KiB.
    let max_message_bytes = _welcome["limits"]["max_message_bytes"]
        .as_u64()
        .unwrap_or(64 * 1024);
    debug!("max_message_bytes: {max_message_bytes}");

    // Events that arrive while a call is waiting land in this queue; the
    // main loop drains it alongside the socket.
    let (queued_tx, queued_rx) = mpsc::unbounded_channel::<Value>();
    let rpc = Rpc::new(ws, move |event: &Value| {
        let _ = queued_tx.send(event.clone());
    });

    let mut actor = Actor {
        socket: socket.to_owned(),
        rpc,
        events,
        sessions: Vec::new(),
        current: None,
        max_message_bytes,
        keymap_hash: None,
        my_client_id: String::new(),
        owners: HashMap::new(),
        last_grid: HashMap::new(),
    };

    // Declare the app's own actions and their default bindings, so the
    // server's keymap (and init.lua) knows what ctrl-alt-* does here and
    // can rebind it. The app still handles the ctrl-alt-* keys locally
    // first; this is a declaration, not a routing table.
    actor.declare_keymap().await;

    // ---- bootstrap: events, session list, and one session attached ----
    let mut payload = json!({"event_scope": "session"});
    if let Some(id) = actor.resolve_preferred(preferred).await? {
        payload["attach_session_ids"] = Value::from(vec![id]);
    }
    let bootstrap: Bootstrap =
        serde_json::from_value(actor.rpc.call("session.bootstrap", payload).await?)?;
    debug!(
        "bootstrap: {} sessions, {} attached",
        bootstrap.attach_all.sessions.len(),
        bootstrap.attached_sessions.len(),
    );
    actor.sessions = bootstrap.attach_all.sessions.clone();
    actor.my_client_id = bootstrap.client_id.clone().unwrap_or_default();
    let first = bootstrap.attached_sessions.first().cloned();
    let _ = actor
        .events
        .send(ClientEvent::Connected {
            sessions: actor.sessions.clone(),
            current: first.as_ref().map(attached_to_pair),
        })
        .await;
    if let Some(attached) = first {
        actor.set_current(attached).await;
    }
    Ok((actor, queued_rx))
}

async fn run(
    socket: String,
    preferred: Option<String>,
    new_session: bool,
    cmd_rx: async_channel::Receiver<ClientCmd>,
    event_tx: async_channel::Sender<ClientEvent>,
) -> Result<()> {
    // Every way the startup can fail funnels through here: the window
    // gets a Closed reason instead of staying on "connecting".
    let (mut actor, mut queued_rx) =
        match start_actor(&socket, preferred.as_deref(), event_tx.clone()).await {
            Ok(started) => started,
            Err(error) => {
                let reason = format!("connect failed: {error}");
                debug!("{reason}");
                let _ = event_tx.send(ClientEvent::Closed(reason)).await;
                return Ok(());
            }
        };
    if new_session {
        if let Err(error) = actor.new_session("rex-app").await {
            debug!("new session failed: {error}");
            actor.notice(format!("new session failed: {error}")).await;
        }
    }

    // ---- main loop: commands, events read directly, queued events ----
    let reason = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Ok(cmd) => actor.command(cmd).await,
                // Every sender dropped: the window closed. Leave the server
                // alone and end the actor.
                Err(_) => break "window closed".to_owned(),
            },
            queued = queued_rx.recv() => {
                if let Some(event) = queued {
                    actor.event(&event).await;
                }
            }
            message = actor.rpc.ws.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value["type"] == "event" {
                            actor.event(&value["payload"]).await;
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => break format!("control connection error: {error}"),
                None => break "control connection closed".to_owned(),
            },
        }
    };
    let _ = event_tx.send(ClientEvent::Closed(reason)).await;
    // The window closed while the connection is still good: hand every
    // owned block back before the socket drops, so sizing passes on
    // without the blocks dying. Best effort — on a connection error the
    // call simply fails and the server's `lost` rule covers us.
    let _ = actor.disown_all().await;
    // Dropping the actor aborts its stream tasks and closes every socket.
    Ok(())
}

/// Revision gating: a view is worth adopting only when the server's
/// revision is strictly newer than the one we are already showing.
fn revision_is_newer(new: u64, known: Option<u64>) -> bool {
    known.is_none_or(|known| new > known)
}

fn attached_to_pair(attached: &Attached) -> (SessionBrief, SessionView) {
    (attached.session.clone(), attached.view.clone())
}

impl Actor {
    /// A label or short ID becomes a full session ID, via `session.list`.
    async fn resolve_preferred(&mut self, preferred: Option<&str>) -> Result<Option<String>> {
        let Some(target) = preferred else {
            return Ok(None);
        };
        if target.starts_with("session:") {
            return Ok(Some(target.to_owned()));
        }
        let listed: Value = self.rpc.call("session.list", json!({})).await?;
        let sessions = listed["sessions"]
            .as_array()
            .ok_or_else(|| anyhow!("session.list returned no sessions array"))?;
        Ok(sessions
            .iter()
            .find(|session| {
                session["label"].as_str() == Some(target)
                    || session["session_id"]
                        .as_str()
                        .is_some_and(|id| id.ends_with(target))
            })
            .and_then(|session| session["session_id"].as_str())
            .map(str::to_owned))
    }

    /// Handles one UI command. Ordered layout methods are serialized here
    /// naturally: the actor handles one command at a time and every call
    /// waits for its response.
    async fn command(&mut self, cmd: ClientCmd) {
        let outcome = match cmd {
            ClientCmd::NewSplit { direction } => self.new_split(direction).await.map(|_| ()),
            ClientCmd::ToggleZoom => self.toggle_zoom().await.map(|_| ()),
            ClientCmd::FocusBlock(block_id) => self.focus_block(&block_id).await.map(|_| ()),
            ClientCmd::SwitchSession(session_id) => {
                self.switch_session(&session_id).await.map(|_| ())
            }
            ClientCmd::NewSession(label) => self.new_session(&label).await.map(|_| ()),
            ClientCmd::SendKey { key, prefix } => {
                self.send_key(key, prefix.as_deref()).await.map(|_| ())
            }
            ClientCmd::Paste(text) => self.paste(&text).await,
            ClientCmd::OwnBlock {
                block_id,
                cols,
                rows,
            } => self.own_block(&block_id, cols, rows).await.map(|_| ()),
            ClientCmd::DisownAll => self.disown_all().await.map(|_| ()),
        };
        if let Err(error) = outcome {
            self.notice(format!("{error}")).await;
        }
    }

    fn session_id(&self) -> Option<&str> {
        self.current.as_ref().map(|c| c.session.session_id.as_str())
    }

    fn active_window_id(&self) -> Option<&str> {
        self.current
            .as_ref()
            .and_then(|c| c.view.active_window())
            .and_then(|w| w.window_id.as_deref())
    }

    async fn notice(&self, text: String) {
        let _ = self.events.send(ClientEvent::Notice(text)).await;
    }

    /// Stops watching the current session, aborting its stream tasks so
    /// their stale frames stop arriving.
    fn drop_current(&mut self) {
        if let Some(current) = self.current.take() {
            for task in current.streams.values() {
                task.abort();
            }
        }
    }

    async fn emit_sessions(&self) {
        let _ = self
            .events
            .send(ClientEvent::Sessions(self.sessions.clone()))
            .await;
    }

    /// `client.keymap`: declare the app's own actions (kind "client") and
    /// the default layer binding the app's ctrl-alt-* keys to them, then
    /// publish which of those keys the RESOLVED keymap still binds to our
    /// actions. init.lua can rebind these names; a Lua-bound key comes back
    /// as `client.key.forward`, so it leaves our set and flows to
    /// `client.key` — where the binding actually fires. If the call fails,
    /// the UI keeps handling every declared default locally (the
    /// `ClientEvent::Keymap` it starts with) and we debug-log.
    async fn declare_keymap(&mut self) {
        let payload = json!({
            "actions": [
                {
                    "name": "app.split",
                    "kind": "client",
                    "title": "Split Pane",
                    "category": "Layouts",
                    "description": "Split the focused pane horizontally or vertically.",
                },
                {
                    "name": "app.zoom",
                    "kind": "client",
                    "title": "Zoom Pane",
                    "category": "Layouts",
                    "description": "Zoom the focused pane to fill the window, or undo the zoom.",
                },
                {
                    "name": "app.picker",
                    "kind": "client",
                    "title": "Session Picker",
                    "category": "Sessions",
                    "description": "Open the session picker.",
                },
                {
                    "name": "app.new_session",
                    "kind": "client",
                    "title": "New Session",
                    "category": "Sessions",
                    "description": "Create a session and open it.",
                }
            ],
            "defaults": [
                {
                    "name": "default",
                    "bindings": [
                        {"keys": ["ctrl+alt+right"], "action": "app.split", "args": {"direction": "horizontal"}},
                        {"keys": ["ctrl+alt+down"], "action": "app.split", "args": {"direction": "vertical"}},
                        {"keys": ["ctrl+alt+z"], "action": "app.zoom"},
                        {"keys": ["ctrl+alt+p"], "action": "app.picker"},
                        {"keys": ["ctrl+alt+n"], "action": "app.new_session"}
                    ]
                }
            ]
        });
        match self.rpc.call("client.keymap", payload).await {
            Ok(keymap) => {
                self.keymap_hash = keymap["config"]["hash"].as_str().map(str::to_owned);
                let ours = resolved_local_keys(&keymap["bindings"]);
                debug!(
                    "keymap: {} resolved bindings; still ours: {:?}",
                    keymap["bindings"].as_array().map(Vec::len).unwrap_or(0),
                    ours
                );
                let _ = self.events.send(ClientEvent::Keymap(ours)).await;
            }
            Err(error) => {
                // A failed declaration must not stop the app: the UI's
                // fallback is handling all declared defaults locally.
                debug!("client.keymap declaration failed: {error}");
                let _ = self
                    .events
                    .send(ClientEvent::Keymap(default_local_keys()))
                    .await;
            }
        }
    }

    /// `client.key`: one key to the focused block, Lua bindings first.
    ///
    /// Design: one round trip per keystroke. `client.key` runs ordered
    /// with layout methods and this actor handles one command at a time,
    /// so keys arrive in typed order by construction; a response per key
    /// is imperceptible at human typing speed, and coalescing would only
    /// help programmatic floods, which the UI never generates.
    async fn send_key(&mut self, key: KeyEvent, prefix: Option<&[String]>) -> Result<Value> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        let mut payload = json!({"session_id": session_id, "key": key.as_json()});
        if let Some(prefix) = prefix {
            payload["prefix"] = Value::from(prefix);
        }
        let result = self.rpc.call("client.key", payload).await?;
        debug!(
            "client.key {} {}: handled={}",
            if key.mods.is_empty() { "-" } else { &key.mods },
            key.key,
            result["handled"].as_bool().unwrap_or(false)
        );
        Ok(result)
    }

    /// Paste: `client.key` with a chunk of text in each call (the server
    /// accepts multi-character `text` — verified live), chunks sized by
    /// their **JSON-encoded** length so each request stays under the
    /// welcome's `limits.max_message_bytes`: serde_json escapes `"`,
    /// `\\` and control characters, so a newline-heavy paste expands
    /// past the limit if chunks are sized by raw bytes and the server
    /// closes the connection (1009). 1 KiB of margin for the JSON
    /// envelope, floored at 1 KiB in case a server advertises the bare
    /// 64 KiB minimum.
    async fn paste(&mut self, text: &str) -> Result<()> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        let margin = 1024;
        let chunk_bytes = (self.max_message_bytes.saturating_sub(margin) as usize).max(1024);
        let mut sent = 0;
        for chunk in rex_conn::chunk_json_safe(text, chunk_bytes) {
            let key = KeyEvent {
                key: "unidentified".into(),
                mods: String::new(),
                text: chunk.to_owned(),
                consumed_mods: String::new(),
                unshifted_codepoint: 0,
            };
            let payload = json!({"session_id": session_id, "key": key.as_json()});
            let result = self.rpc.call("client.key", payload).await?;
            sent += chunk.len();
            debug!(
                "paste chunk {} bytes (total {}): handled={}",
                chunk.len(),
                sent,
                result["handled"].as_bool().unwrap_or(false)
            );
        }
        Ok(())
    }

    /// `session.new_split`: one shell block beside the focused block of
    /// the active window.
    async fn new_split(&mut self, direction: Direction) -> Result<Value> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session to split"))?
            .to_owned();
        let mut payload = json!({
            "session_id": session_id,
            "direction": direction.as_str(),
            "side": "after",
            "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}},
            "focus": true,
        });
        if let Some(window_id) = self.active_window_id() {
            payload["window_id"] = Value::from(window_id);
        }
        self.rpc.call("session.new_split", payload).await
    }

    /// `session.zoom_block`: zoom the focused block, or undo the zoom.
    async fn toggle_zoom(&mut self) -> Result<Value> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session to zoom"))?
            .to_owned();
        self.rpc
            .call("session.zoom_block", json!({"session_id": session_id}))
            .await
    }

    /// `session.own_block` (mode `take`) followed, when the grid actually
    /// changed, by `com.superlogical.terminal.resize` with `cols` x `rows`
    /// and this client's cell metrics: the resize applies at once because
    /// we just became the owner. Coalesced: if we already own the block
    /// at this grid, nothing is sent. Ownership is exclusive but not
    /// erroring: another client's claim doesn't fail this call — it wins
    /// and moves the block to our grid; the `block_owner_changed` event is
    /// how a losing client finds out and backs off.
    async fn own_block(&mut self, block_id: &str, cols: u32, rows: u32) -> Result<()> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        let ours = self
            .owners
            .get(block_id)
            .and_then(|owner| owner.as_deref())
            .is_some_and(|owner| owner == self.my_client_id.as_str());
        if ours && self.last_grid.get(block_id) == Some(&(cols, rows)) {
            return Ok(());
        }
        let result = self
            .rpc
            .call(
                "session.own_block",
                json!({
                    "session_id": session_id,
                    "block_ids": [block_id],
                    "mode": "take",
                }),
            )
            .await?;
        self.record_owners(&result, &[block_id]);
        debug!(
            "own_block {block_id} at {cols}x{rows}: owner={}",
            self.owners
                .get(block_id)
                .and_then(|o| o.clone())
                .unwrap_or_default(),
        );
        // Only resize when the grid differs from the last one applied.
        if self.last_grid.get(block_id) == Some(&(cols, rows)) {
            return Ok(());
        }
        self.rpc
            .call(
                "com.superlogical.terminal.resize",
                json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "args": {
                        "columns": cols,
                        "rows": rows,
                        "cell_width_px": crate::terminal::CELL_W_PX as u32,
                        "cell_height_px": crate::terminal::CELL_H_PX as u32,
                    },
                }),
            )
            .await?;
        self.last_grid.insert(block_id.to_owned(), (cols, rows));
        debug!("resize {block_id} to {cols}x{rows}");
        Ok(())
    }

    /// `session.disown_block` with no `block_ids`: give up every claim in
    /// the current session. The blocks keep running; sizing passes to the
    /// next client in line for each ("the block follows its owner").
    /// Nothing to do when we own nothing.
    async fn disown_all(&mut self) -> Result<()> {
        if self.current.is_none() {
            return Ok(());
        }
        let ours = self
            .owners
            .values()
            .any(|owner| owner.as_deref() == Some(self.my_client_id.as_str()));
        if !ours && self.last_grid.is_empty() {
            return Ok(());
        }
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        let result = self
            .rpc
            .call("session.disown_block", json!({"session_id": session_id}))
            .await?;
        self.record_owners(&result, &[]);
        self.last_grid.clear();
        debug!("disowned every block in {session_id}");
        Ok(())
    }

    /// Records the owners an own_block/disown_block result reports for
    /// `block_ids` (or every block in the result when empty).
    fn record_owners(&mut self, result: &Value, block_ids: &[&str]) {
        for entry in result["blocks"].as_array().into_iter().flatten() {
            let Some(block_id) = entry["block_id"].as_str() else {
                continue;
            };
            if !block_ids.is_empty() && !block_ids.contains(&block_id) {
                continue;
            }
            let owner = entry["owner"].as_str().map(str::to_owned);
            self.owners.insert(block_id.to_owned(), owner);
        }
    }

    /// `session.focus_block`: focus one block.
    async fn focus_block(&mut self, block_id: &str) -> Result<Value> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        self.rpc
            .call(
                "session.focus_block",
                json!({"session_id": session_id, "block_id": block_id}),
            )
            .await
    }

    /// `session.create`, then attach and show the new session.
    async fn new_session(&mut self, label: &str) -> Result<()> {
        let created = self
            .rpc
            .call(
                "session.create",
                json!({
                    "label": label,
                    "initial_windows": [
                        {"layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}}
                    ]
                }),
            )
            .await?;
        let session_id = created["session_id"]
            .as_str()
            .ok_or_else(|| anyhow!("no session_id in session.create result"))?
            .to_owned();
        let brief = SessionBrief {
            label: label.to_owned(),
            session_id: session_id.clone(),
        };
        if !self.sessions.iter().any(|s| s.session_id == session_id) {
            self.sessions.push(brief.clone());
            self.emit_sessions().await;
        }
        self.attach(&brief).await
    }

    /// Attach to a session and make it the one the window shows, detaching
    /// the previous one. One view at a time.
    async fn switch_session(&mut self, session_id: &str) -> Result<()> {
        let brief = self
            .sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .cloned()
            .unwrap_or(SessionBrief {
                session_id: session_id.to_owned(),
                label: String::new(),
            });
        self.attach(&brief).await
    }

    async fn attach(&mut self, brief: &SessionBrief) -> Result<()> {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.session.session_id == brief.session_id)
        {
            return Ok(());
        }
        // session.attach answers with the client and the session's
        // current view, wrapped in a `view` field.
        let attached = self
            .rpc
            .call("session.attach", json!({"session_id": brief.session_id}))
            .await?;
        let view: SessionView = serde_json::from_value(attached["view"].clone())?;
        let previous = self.current.take();
        if let Some(previous) = &previous {
            // The old session keeps running; we just stop watching it.
            // Abort its stream tasks so no stale frames keep arriving.
            for task in previous.streams.values() {
                task.abort();
            }
            let _ = self
                .rpc
                .call(
                    "session.detach",
                    json!({"session_id": previous.session.session_id}),
                )
                .await;
        }
        self.set_current(Attached {
            session: brief.clone(),
            view,
        })
        .await;
        Ok(())
    }

    /// Adopts the session the bootstrap/attach returned: publishes the
    /// view and opens data streams for the active window's blocks.
    async fn set_current(&mut self, attached: Attached) {
        let Attached { session, view } = attached;
        debug!(
            "showing session {} (revision {})",
            session.session_id, view.revision,
        );
        let events = self.events.clone();
        let _ = events
            .send(ClientEvent::View(session.clone(), view.clone()))
            .await;
        self.current = Some(Current {
            session,
            view,
            streams: HashMap::new(),
        });
        // Ownership belongs to the session: a new current session starts
        // with a clean slate. (The view doesn't report owners; own/disown
        // results and `block_owner_changed` events keep the map current.)
        self.owners.clear();
        self.last_grid.clear();
        self.sync_streams().await;
    }

    /// Refreshes the current session's view when the server says it
    /// changed. A revision at or below ours describes a change we have.
    async fn refresh_view(&mut self) -> Result<()> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        let view: SessionView = serde_json::from_value(
            self.rpc
                .call("session.view", json!({"session_id": session_id}))
                .await?,
        )?;
        let Some(current) = &mut self.current else {
            return Ok(());
        };
        if !revision_is_newer(view.revision, Some(current.view.revision)) {
            debug!(
                "view revision {} not newer than {}",
                view.revision, current.view.revision
            );
            return Ok(());
        }
        debug!("refreshed view to revision {}", view.revision);
        let _ = self
            .events
            .send(ClientEvent::View(current.session.clone(), view.clone()))
            .await;
        current.view = view;
        self.sync_streams().await;
        Ok(())
    }

    /// Opens a data stream for every visible terminal block of the active
    /// window and drops the ones no longer visible. Blocks outside the
    /// active window keep running server-side; we just don't watch them.
    async fn sync_streams(&mut self) {
        let Some(current) = &self.current else {
            return;
        };
        let visible: Vec<String> = current
            .view
            .active_window()
            .map(|w| w.placements().into_iter().map(|(id, _)| id).collect())
            .unwrap_or_default();
        let stale: Vec<String> = current
            .streams
            .keys()
            .filter(|id| !visible.contains(id))
            .cloned()
            .collect();
        let mut tasks_to_drop = Vec::new();
        {
            let current = self.current.as_mut().expect("checked above");
            for id in stale {
                if let Some(task) = current.streams.remove(&id) {
                    tasks_to_drop.push(task);
                }
            }
        }
        for task in tasks_to_drop {
            task.abort();
        }
        for block_id in visible {
            let already = self
                .current
                .as_ref()
                .is_some_and(|c| c.streams.contains_key(&block_id));
            if already {
                continue;
            }
            let session_id = self.session_id().expect("current exists").to_owned();
            let Ok(ticket) = rex_conn::data_ticket(&mut self.rpc, &session_id, &block_id).await
            else {
                continue;
            };
            let socket = self.socket.clone();
            let events = self.events.clone();
            let block_id_task = block_id.clone();
            let task = tokio::task::spawn_local(async move {
                let _ = stream_block(&socket, &ticket, &block_id_task, events).await;
            });
            if let Some(current) = self.current.as_mut() {
                current.streams.insert(block_id, task);
            } else {
                task.abort();
            }
        }
    }

    /// Routes one control event.
    async fn event(&mut self, payload: &Value) {
        let kind = payload["type"].as_str().unwrap_or_default();
        match kind {
            "session_created" => {
                if let Ok(brief) =
                    serde_json::from_value::<SessionBrief>(payload["session"].clone())
                {
                    if !self
                        .sessions
                        .iter()
                        .any(|s| s.session_id == brief.session_id)
                    {
                        self.sessions.push(brief);
                        self.emit_sessions().await;
                    }
                }
            }
            "session_destroyed" => {
                let gone = payload["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.sessions.retain(|s| s.session_id != gone);
                if self
                    .current
                    .as_ref()
                    .is_some_and(|current| current.session.session_id == gone)
                {
                    self.drop_current();
                    let _ = self.events.send(ClientEvent::CurrentGone).await;
                }
                self.emit_sessions().await;
            }
            // A session moved in the list; the order it changed to is not
            // worth a round trip right now.
            "session_moved" => {}
            // The configuration file was reloaded; our keymap may have
            // changed under us. Docs: compare the hash and re-fetch only
            // when it differs (a declaration that failed left None, so a
            // reload is also the retry path).
            "keymap_changed" => {
                // The fields sit one payload level down, like a session
                // event's.
                let hash = payload["payload"]["hash"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if self.keymap_hash.as_deref() != Some(hash.as_str()) {
                    debug!("keymap changed (hash {hash}), re-fetching");
                    self.declare_keymap().await;
                }
            }
            "session_event" => {
                // A session's event arrives wrapped: the session's ID and
                // the event itself sit one payload level down.
                let inner = &payload["payload"];
                let session_id = inner["session_id"].as_str().unwrap_or_default();
                let event = &inner["event"];
                let matches = self
                    .current
                    .as_ref()
                    .is_some_and(|current| current.session.session_id == session_id);
                if !matches {
                    return;
                }
                if event["type"] == "session_view_changed" {
                    let revision = event["payload"]["revision"].as_u64().unwrap_or(0);
                    let known = self.current.as_ref().map(|c| c.view.revision);
                    if revision_is_newer(revision, known) {
                        debug!("view changed to revision {revision}, refreshing");
                        if let Err(error) = self.refresh_view().await {
                            debug!("view refresh failed: {error}");
                            self.notice(format!("view refresh failed: {error}")).await;
                        }
                    }
                }
                if event["type"] == "block_owner_changed" {
                    let payload = &event["payload"];
                    let block_id = payload["block_id"].as_str().unwrap_or_default().to_owned();
                    let owner = payload["owner"].as_str().map(str::to_owned);
                    let previous = payload["previous"].as_str().map(str::to_owned);
                    let reason = payload["reason"].as_str().unwrap_or_default().to_owned();
                    self.owners.insert(block_id.clone(), owner.clone());
                    debug!(
                            "block_owner_changed: {block_id} owner={owner:?} previous={previous:?} reason={reason}"
                    );
                    // Exclusivity: someone else took a block we owned. Back
                    // off — forget our grid so a later claim re-resizes —
                    // and surface it rather than fighting over the block.
                    if previous.as_deref() == Some(self.my_client_id.as_str())
                        && owner.as_deref() != Some(self.my_client_id.as_str())
                    {
                        self.last_grid.remove(&block_id);
                        let who = owner.as_deref().unwrap_or("no one");
                        self.notice(format!("block {block_id} taken by {who}"))
                            .await;
                    }
                }
                // block_created, window_created, focus changes, and the
                // rest all show up in the next view.
            }
            _ => {}
        }
    }
}

/// Feeds one block's data stream into a replica and publishes its frames.
async fn stream_block(
    socket: &str,
    ticket: &[u8],
    block_id: &str,
    events: async_channel::Sender<ClientEvent>,
) -> Result<()> {
    let (mut ws, nonce) = rex_conn::open_data(socket, ticket).await?;
    let mut replica = Replica::new(nonce)?;
    let mut first_frame = true;
    while let Some(message) = ws.next().await {
        let bytes = match message? {
            Message::Binary(bytes) => bytes,
            // The server only sends binary on a data stream.
            _ => continue,
        };
        let fed = replica.feed(&bytes)?;
        if let Some((cols, rows, cell_w, cell_h)) = fed.resize {
            // Cross-check, not an assert: the server tells us the cell size
            // it sized the grid for (0 when unknown). When it's nonzero it
            // should match what we asked for — a mismatch means our own
            // request didn't land, or someone else owns the block.
            if cell_w > 0 && cell_h > 0 {
                let (want_w, want_h) = (
                    crate::terminal::CELL_W_PX as u32,
                    crate::terminal::CELL_H_PX as u32,
                );
                if cell_w == want_w && cell_h == want_h {
                    debug!("block {block_id}: resize {cols}x{rows} at {cell_w}x{cell_h}px matches our cell metrics");
                } else {
                    debug!(
                        "block {block_id}: resize {cols}x{rows} at {cell_w}x{cell_h}px differs from our {want_w}x{want_h}px (someone else owns the grid?)"
                    );
                }
            } else {
                debug!("block {block_id}: resize {cols}x{rows}, cell px unknown");
            }
        }
        if fed.changed {
            if let Some(frame) = replica.frame()? {
                if first_frame {
                    first_frame = false;
                    debug!(
                        "block {block_id}: first frame {}x{}",
                        frame.cols, frame.rows,
                    );
                }
                let _ = events
                    .send(ClientEvent::Frame(block_id.to_owned(), Arc::new(frame)))
                    .await;
            }
        }
        if let Some(code) = fed.exited {
            let _ = events
                .send(ClientEvent::Notice(format!("program exited with {code}")))
                .await;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::{default_local_keys, key_is_ours, normalize_key, revision_is_newer};
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn revision_must_be_strictly_newer() {
        assert!(revision_is_newer(2, None));
        assert!(revision_is_newer(3, Some(2)));
        assert!(!revision_is_newer(2, Some(2)));
        assert!(!revision_is_newer(1, Some(2)));
    }

    #[test]
    fn normalize_key_orders_mods_and_lowercases() {
        assert_eq!(normalize_key("ctrl+alt+z"), Some("ctrl+alt+z".into()));
        assert_eq!(normalize_key("Alt+Ctrl+Z"), Some("ctrl+alt+z".into()));
        assert_eq!(
            normalize_key("shift+ctrl+alt+down"),
            Some("shift+ctrl+alt+down".into())
        );
        assert_eq!(normalize_key("cmd+k"), Some("super+k".into()));
        assert_eq!(normalize_key("v"), Some("v".into()));
        // Not one plain key: no key token, or two of them.
        assert_eq!(normalize_key("ctrl+"), None);
        assert_eq!(normalize_key("a+b"), None);
    }

    #[test]
    fn resolved_local_keys_honor_rebinds_and_removals() {
        // Everything still ours: the resolved list of a fresh server.
        let fresh = json!([
            {"keys": ["ctrl+alt+right"], "action": "app.split", "args": {"direction": "horizontal"}},
            {"keys": ["ctrl+alt+down"], "action": "app.split", "args": {"direction": "vertical"}},
            {"keys": ["ctrl+alt+z"], "action": "app.zoom"},
            {"keys": ["ctrl+alt+p"], "action": "app.picker"},
            {"keys": ["ctrl+alt+n"], "action": "app.new_session"}
        ]);
        assert_eq!(super::resolved_local_keys(&fresh), default_local_keys());

        // init.lua took over ctrl+alt+p (a Lua binding reports as
        // client.key.forward, which always wins), ctrl+alt+n was rebound
        // to another action, ctrl+alt+right starts a sequence, and
        // ctrl+alt+down was removed: only zoom stays ours. A mode
        // binding for ctrl+alt+z must not hide the modeless one.
        let rebound = json!([
            {"keys": ["ctrl+alt+p"], "action": "client.key.forward", "source": "init.lua:3"},
            {"keys": ["ctrl+alt+z"], "action": "app.zoom"},
            {"keys": ["ctrl+alt+z"], "action": "client.key.forward", "mode": "resize"},
            {"keys": ["ctrl+alt+n"], "action": "dev_layout"},
            {"keys": ["ctrl+alt+right", "x"], "action": "app.split"}
        ]);
        let rebound: &[serde_json::Value] = rebound.as_array().unwrap();
        assert!(key_is_ours(rebound, "ctrl+alt+z"));
        assert!(!key_is_ours(rebound, "ctrl+alt+p"));
        assert!(!key_is_ours(rebound, "ctrl+alt+n"));
        assert!(!key_is_ours(rebound, "ctrl+alt+right"));
        assert!(!key_is_ours(rebound, "ctrl+alt+down"));
        let only_zoom: HashSet<String> = ["ctrl+alt+z"].iter().map(|s| s.to_string()).collect();
        assert_eq!(super::resolved_local_keys(&json!(rebound)), only_zoom);

        // Nothing bound at all: nothing ours.
        assert_eq!(
            super::resolved_local_keys(&json!([])),
            HashSet::<String>::new()
        );
    }

    #[test]
    fn default_local_keys_are_the_declared_defaults() {
        assert!(default_local_keys().contains("ctrl+alt+z"));
        assert_eq!(default_local_keys().len(), 5);
    }

    #[test]
    fn chunk_json_safe_paste_stays_within_budget() {
        // The regression from review: sizing paste chunks by raw UTF-8
        // bytes let a newline-heavy payload expand ~2x in JSON and
        // blow past limits.max_message_bytes, closing the control
        // connection (1009). Every chunk's JSON encoding must stay
        // within the budget — proven for a control-char-heavy input.
        let chunk_budget = (1024 * 1024usize).saturating_sub(1024); // 1 MiB server, 1 KiB envelope margin
        let mut text = String::new();
        for _ in 0..600 * 1024 {
            text.push('\n'); // ~600 KiB of newlines: 2x expansion in JSON
        }
        for i in 0..20_000 {
            text.push(if i % 2 == 0 { '\n' } else { 'x' });
            if i % 7 == 3 {
                text.push('\u{1}'); // 6x expansion as \u0001
            }
        }
        let mut chunks = 0;
        for chunk in rex_conn::chunk_json_safe(&text, chunk_budget) {
            chunks += 1;
            let payload = json!(chunk);
            assert!(
                payload.to_string().len() <= chunk_budget,
                "chunk JSON encoded to {} > budget {}",
                payload.to_string().len(),
                chunk_budget
            );
        }
        assert!(chunks >= 2, "expected multiple chunks");
        assert_eq!(text.len(), 600 * 1024 + 20_000 + 2857); // newlines + xs/controls
    }
}
