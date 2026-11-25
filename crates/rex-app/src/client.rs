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
use std::collections::HashMap;
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
        prefix: Option<Vec<String>>,
    },
    /// `client.key` in chunks: paste as text, split to stay under the
    /// server's `limits.max_message_bytes`. One call per chunk.
    Paste(String),
}

/// One keyboard event for `client.key`: physical key name (W3C code in
/// snake_case, `unidentified` for synthesized text), mods joined with `+`,
/// the text it typed, and its unshifted code point for layout-dependent
/// bindings such as `ctrl+n`.
#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub key: String,
    pub mods: String,
    pub text: String,
    pub unshifted_codepoint: i32,
}

impl KeyEvent {
    /// A key press as `client.key`'s `key` object. `consumed_mods` is
    /// empty (we don't track layout-consumed shift) and `composing` is
    /// false (GPUI gives us finished compositions).
    fn as_json(&self) -> Value {
        json!({
            "key": self.key,
            "mods": self.mods,
            "consumed_mods": "",
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
    /// the default layer binding the app's ctrl-alt-* keys to them.
    /// init.lua can rebind these names; a Lua-bound key then comes back
    /// in the keymap as `client.key.forward`, which is exactly the path
    /// SendKey uses, so Lua bindings keep first chance at every key.
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
                let lua_bindings = keymap["bindings"].as_array().map(|b| b.len()).unwrap_or(0);
                debug!("keymap declared: {lua_bindings} resolved bindings");
            }
            Err(error) => {
                // A failed declaration must not stop the app: the app's
                // own keys still work locally.
                debug!("client.keymap declaration failed: {error}");
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
    /// accepts multi-character `text` — verified live), chunks sized so
    /// each request's JSON stays under `limits.max_message_bytes`; an
    /// oversized request would close the connection. 1 KiB of margin for
    /// the JSON envelope, floored at 1 KiB in case a server advertises
    /// the bare 64 KiB minimum.
    async fn paste(&mut self, text: &str) -> Result<()> {
        let session_id = self
            .session_id()
            .ok_or_else(|| anyhow!("no session"))?
            .to_owned();
        let margin = 1024;
        let chunk_bytes = (self.max_message_bytes.saturating_sub(margin) as usize).max(1024);
        let mut sent = 0;
        for chunk in rex_conn::chunk_utf8(text, chunk_bytes) {
            let key = KeyEvent {
                key: "unidentified".into(),
                mods: String::new(),
                text: chunk.to_owned(),
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
    #[test]
    fn chunk_utf8_splits_on_char_boundaries() {
        let text = "\u{e9}abc\u{1f600}def";
        let chunks: Vec<&str> = rex_conn::chunk_utf8(text, 3).collect();
        assert_eq!(chunks, vec!["\u{e9}a", "bc", "\u{1f600}", "def"]);
    }

    #[test]
    fn chunk_utf8_empty_and_huge() {
        assert_eq!(rex_conn::chunk_utf8("", 100).count(), 0);
        let huge = "x".repeat(10_000);
        assert_eq!(
            rex_conn::chunk_utf8(&huge, 1024)
                .map(str::len)
                .sum::<usize>(),
            10_000
        );
        assert!(rex_conn::chunk_utf8(&huge, 1024).all(|c| c.len() <= 1024));
    }

    #[test]
    fn chunk_utf8_wide_char_in_narrow_chunk() {
        // A single character wider than the chunk still goes out whole
        // rather than looping on empty chunks.
        let chunks: Vec<&str> = rex_conn::chunk_utf8("a\u{1f600}b", 2).collect();
        assert_eq!(chunks, vec!["a", "\u{1f600}", "b"]);
    }
}

#[cfg(test)]
mod tests {
    use super::revision_is_newer;

    #[test]
    fn revision_must_be_strictly_newer() {
        assert!(revision_is_newer(2, None));
        assert!(revision_is_newer(3, Some(2)));
        assert!(!revision_is_newer(2, Some(2)));
        assert!(!revision_is_newer(1, Some(2)));
    }
}
