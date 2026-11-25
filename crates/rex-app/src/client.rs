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
    /// A new view of the current session (also the first view).
    View(SessionView),
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime");
        // Replicas hold libghostty iterators that are not Send, so stream
        // tasks run on a LocalSet on this thread instead of the pool.
        let local = tokio::task::LocalSet::new();
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

async fn run(
    socket: String,
    preferred: Option<String>,
    new_session: bool,
    cmd_rx: async_channel::Receiver<ClientCmd>,
    event_tx: async_channel::Sender<ClientEvent>,
) -> Result<()> {
    let ws = connect(&socket, "/api/control", "rex.control.v1").await?;
    let (ws, _welcome) = hello(
        ws,
        json!({"kind": "app", "name": "rex-app", "version": env!("CARGO_PKG_VERSION")}),
    )
    .await?;

    // Events that arrive while a call is waiting land in this queue; the
    // main loop drains it alongside the socket.
    let (queued_tx, mut queued_rx) = mpsc::unbounded_channel::<Value>();
    let rpc = Rpc::new(ws, move |event: &Value| {
        let _ = queued_tx.send(event.clone());
    });

    let mut actor = Actor {
        socket: socket.clone(),
        rpc,
        events: event_tx.clone(),
        sessions: Vec::new(),
        current: None,
    };

    // ---- bootstrap: events, session list, and one session attached ----
    let mut payload = json!({"event_scope": "session"});
    if let Some(id) = actor.resolve_preferred(preferred.as_deref()).await? {
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
    let _ = event_tx
        .send(ClientEvent::Connected {
            sessions: actor.sessions.clone(),
            current: first.as_ref().map(attached_to_pair),
        })
        .await;
    if let Some(attached) = first {
        actor.set_current(attached).await;
    }
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

    async fn emit_sessions(&self) {
        let _ = self
            .events
            .send(ClientEvent::Sessions(self.sessions.clone()))
            .await;
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
        let _ = events.send(ClientEvent::View(view.clone())).await;
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
        if view.revision <= current.view.revision {
            debug!(
                "view revision {} not newer than {}",
                view.revision, current.view.revision
            );
            return Ok(());
        }
        debug!("refreshed view to revision {}", view.revision);
        let _ = self.events.send(ClientEvent::View(view.clone())).await;
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
                    self.current = None;
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
                    if known.is_none_or(|known| revision > known) {
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
