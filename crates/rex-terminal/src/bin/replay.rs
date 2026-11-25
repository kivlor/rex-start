//! Replays a recorded Rex terminal stream and prints the final screen.
//!
//! Usage: replay <stream file> [--nonce N]
//!
//! The nonce defaults to the one in the stream's first rex1 APC, which the
//! server sends (a resize) right after the snapshot, so a capture from
//! `probe --label X` replays with no arguments:
//!
//!     cargo run -p rex-terminal --bin replay -- probe-stream-X.bin

use std::process::ExitCode;

use rex_terminal::{detect_nonce, row_text, Replica};

fn main() -> ExitCode {
    let mut path = None;
    let mut nonce = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--nonce" => match args.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(n) => nonce = Some(n),
                None => {
                    eprintln!("replay: --nonce needs a decimal channel nonce");
                    return ExitCode::FAILURE;
                }
            },
            "--help" | "-h" => {
                println!("usage: replay <stream file> [--nonce N]");
                return ExitCode::SUCCESS;
            }
            _ if path.is_none() => path = Some(arg),
            _ => {
                eprintln!("replay: unexpected argument {arg}");
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: replay <stream file> [--nonce N]");
        return ExitCode::FAILURE;
    };

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("replay: reading {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let nonce = nonce.or_else(|| detect_nonce(&bytes));
    let Some(nonce) = nonce else {
        eprintln!("replay: no --nonce given and {path} holds no rex1 APC to take one from");
        return ExitCode::FAILURE;
    };

    let mut replica = match Replica::new(nonce) {
        Ok(replica) => replica,
        Err(e) => {
            eprintln!("replay: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut exited = None;
    for chunk in bytes.chunks(4096) {
        match replica.feed(chunk) {
            Ok(fed) => exited = exited.or(fed.exited),
            Err(e) => {
                eprintln!("replay: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let frame = match replica.frame() {
        Ok(Some(frame)) => frame,
        Ok(None) => {
            eprintln!("replay: the snapshot never decoded (truncated file?)");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("replay: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("{}x{}", frame.cols, frame.rows);
    for row in &frame.cells {
        println!("|{}", row_text(row));
    }
    if let Some(code) = exited {
        println!("[child exited with {code}]");
    }
    ExitCode::SUCCESS
}
