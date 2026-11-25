//! rex-app: a GPUI window onto a Rex server. One session's active window,
//! its panes laid out the way the server says, a session picker, and clean
//! disconnection on close: sessions keep running server-side.
//!
//! Set `REX_APP_HEADLESS=SECONDS` to run the client actor without a
//! window: events and statuses go to stderr, for smoke tests of the
//! reconnect paths (see NOTES-issue-6.md).

mod app;
mod client;
mod terminal;
mod view;

use gpui::{prelude::*, px, size, App, Application, Bounds, WindowBounds, WindowOptions};

use app::RexApp;

const USAGE: &str = "\
Usage: rex-app [options]

Options:
  -S, --socket PATH     the server's Unix socket
                        (default: $REX_SERVER, then $XDG_RUNTIME_DIR/rex/server.sock)
  -s, --session TARGET  open the session with this label or ID first
      --new-session     create a session and open it
  -h, --help            show this help
";

fn default_socket() -> String {
    std::env::var("REX_SERVER")
        .ok()
        .filter(|value| !value.starts_with("http"))
        .unwrap_or_else(|| {
            // The runtime dir is per-user; derive it instead of assuming
            // uid 1000.
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
            format!("{runtime_dir}/rex/server.sock")
        })
}

struct Args {
    socket: String,
    session: Option<String>,
    new_session: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        socket: default_socket(),
        session: None,
        new_session: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "-S" | "--socket" => args.socket = value(&flag)?,
            "-s" | "--session" => args.session = Some(value(&flag)?),
            "--new-session" => args.new_session = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("rex-app: {error}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let Args {
        socket,
        session,
        new_session,
    } = args;
    let client = match client::start(socket, session, new_session) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("rex-app: {error}");
            std::process::exit(1);
        }
    };

    // Headless mode: run the actor without a window, logging events —
    // for smoke tests where no display is available (CI, relay tests).
    if let Ok(secs) = std::env::var("REX_APP_HEADLESS") {
        let secs: u64 = secs.parse().unwrap_or(30);
        return headless(client, secs);
    }

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("rex-app".into()),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Rex");
                cx.new(|cx| RexApp::new(client, window, cx))
            },
        )
        .expect("open the Rex window");
        cx.activate(true);
    });
}

/// One compact line per event; `Frame` — the only event big enough to
/// flood the log with a 50KB-plus `Debug` dump — is summarized instead
/// of pretty-printed: dimensions, cursor, and the first non-empty rows
/// (trimmed), matching the `debug_frames` logic in `app.rs`.
fn headless_log(event: client::ClientEvent) {
    match event {
        client::ClientEvent::Frame(block_id, frame) => {
            let rows: Vec<String> = frame
                .cells
                .iter()
                .map(|row| rex_terminal::row_text(row).trim().to_string())
                .filter(|row| !row.is_empty())
                .take(2)
                .collect();
            let rows = rows.join(" / ");
            let cursor = frame
                .cursor
                .as_ref()
                .map(|c| format!(" cursor ({},{})", c.x, c.y))
                .unwrap_or_default();
            eprintln!(
                "[rex-app] frame block:{block_id} {}x{}{cursor} rows: {rows}",
                frame.cols, frame.rows
            );
        }
        other => eprintln!("[rex-app] event: {other:?}"),
    }
}

/// Drives the client actor without a GPUI window, logging the events a
/// window would show (with `REX_APP_DEBUG=1`, the actor's own lines add
/// the protocol detail). Exits after `secs` seconds, or when the actor
/// gives up and closes.
fn headless(client: client::Client, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            eprintln!("[rex-app] headless run finished");
            return;
        }
        // The actor lives on its own thread; poll its events (no
        // blocking recv with a deadline in async-channel, so poll).
        match client.events.try_recv() {
            Ok(event) => headless_log(event),
            Err(async_channel::TryRecvError::Empty) => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Err(_) => {
                eprintln!("[rex-app] actor closed");
                return;
            }
        }
    }
}
