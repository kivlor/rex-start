//! rex-app: a GPUI window onto a Rex server. One session's active window,
//! its panes laid out the way the server says, a session picker, and clean
//! disconnection on close: sessions keep running server-side.

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
