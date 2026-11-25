//! Terminal replicas for Rex terminal streams, built on libghostty
//! (the `ghostty-vt` safe API).
//!
//! A Rex terminal stream starts with a snapshot of the server's terminal in
//! Ghostty's snapshot format, then everything the terminal's program writes,
//! with Rex's own control messages (`rex1` APCs) placed between them
//! (docs/API-rev19.md, "The Terminal Stream"). A [`Replica`] restores the
//! snapshot into a libghostty terminal, applies everything after it in order,
//! and renders [`Frame`]s. Replicas never answer terminal queries: the
//! server's own terminal already does.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use rex_terminal::{row_text, Replica};
//!
//! let mut replica = Replica::new(42)?; // 42: the WELCOME's channel nonce
//! replica.feed(b"...snapshot and stream bytes...")?;
//! let frame = replica.frame()?.ok_or("snapshot not restored yet")?;
//! for row in &frame.cells {
//!     println!("{}", row_text(row));
//! }
//! # Ok(())
//! # }
//! ```

mod stream;

pub use stream::{detect_nonce, Piece, RexMessage, StreamFilter};

use ghostty_vt::{
    render::{CellIterator, RenderState, RowIterator},
    snapshot,
    style::RgbColor,
    terminal::Terminal,
};
use thiserror::Error;

/// Failures from restoring or driving a replica. None of them are fatal to
/// the process: a caller can log and drop the stream.
#[derive(Debug, Error)]
pub enum Error {
    #[error("libghostty error: {0}")]
    Ghostty(#[from] ghostty_vt::Error),
    #[error("stream does not start with a Ghostty snapshot")]
    NotASnapshot,
    #[error("snapshot exceeded {0} bytes without decoding")]
    SnapshotTooLarge(usize),
}

/// An 8-bit RGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<RgbColor> for Color {
    fn from(c: RgbColor) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// One grid cell of a rendered frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub foreground: Color,
    pub background: Color,
}

/// The terminal's cursor in viewport coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

/// A rendered screen: rows of cells plus the defaults a renderer needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub background: Color,
    pub foreground: Color,
    pub cells: Vec<Vec<Cell>>,
    pub cursor: Option<Cursor>,
}

/// What a replica learned from the bytes it just consumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fed {
    /// The screen may have changed.
    pub changed: bool,
    /// The terminal's program exited with this code.
    pub exited: Option<i64>,
}

/// A libghostty terminal mirroring one Rex terminal stream.
///
/// Bytes are fed incrementally in stream order; the snapshot at the start is
/// restored automatically once enough of it has arrived. All stream bytes are
/// untrusted: malformed input is skipped or reported as an [`Error`], never
/// panicked on.
pub struct Replica {
    nonce: u64,
    state: State,
}

enum State {
    /// Still collecting the snapshot at the head of the stream.
    Restoring(Vec<u8>),
    Live(Box<Live>),
}

struct Live {
    terminal: Terminal,
    filter: StreamFilter,
    render_state: RenderState,
    rows: RowIterator,
    cells: CellIterator,
}

impl Replica {
    /// A replica for a stream whose WELCOME carried `nonce`.
    pub fn new(nonce: u64) -> Result<Self, Error> {
        Ok(Self {
            nonce,
            state: State::Restoring(Vec::new()),
        })
    }

    /// The channel nonce this replica accepts.
    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    /// True once the snapshot has been restored and the terminal is live.
    pub fn is_live(&self) -> bool {
        matches!(self.state, State::Live(_))
    }

    /// Consumes the next stream bytes. Returns [`Fed::default`] while the
    /// snapshot is still incomplete.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Fed, Error> {
        let rest: Vec<u8> = match &mut self.state {
            State::Live(live) => {
                let mut fed = Fed::default();
                live.feed(bytes, &mut fed);
                return Ok(fed);
            }
            State::Restoring(buffer) => {
                buffer.extend_from_slice(bytes);
                match try_restore(buffer)? {
                    Some((terminal, consumed)) => {
                        let rest = buffer.split_off(consumed);
                        self.state = State::Live(Box::new(Live::new(terminal, self.nonce)?));
                        rest
                    }
                    None => {
                        if buffer.len() > MAX_SNAPSHOT_BYTES {
                            return Err(Error::SnapshotTooLarge(MAX_SNAPSHOT_BYTES));
                        }
                        return Ok(Fed::default());
                    }
                }
            }
        };
        let State::Live(live) = &mut self.state else {
            // Set just above; the borrow checker can't see through the match.
            return Err(Error::NotASnapshot);
        };
        let mut fed = Fed {
            changed: true,
            ..Fed::default()
        };
        live.feed(&rest, &mut fed);
        Ok(fed)
    }

    /// The grid size, once the replica is live.
    pub fn size(&self) -> Option<(u16, u16)> {
        let State::Live(live) = &self.state else {
            return None;
        };
        Some((live.terminal.cols().ok()?, live.terminal.rows().ok()?))
    }

    /// Renders the current screen, once the replica is live.
    pub fn frame(&mut self) -> Result<Option<Frame>, Error> {
        let State::Live(live) = &mut self.state else {
            return Ok(None);
        };
        let snapshot = live.render_state.update(&live.terminal)?;
        let colors = snapshot.colors()?;
        let default_foreground: Color = colors.foreground.into();
        let default_background: Color = colors.background.into();
        let cols = snapshot.cols()?;
        let rows = snapshot.rows()?;

        let cursor = snapshot.cursor_viewport()?.map(|at| Cursor {
            x: at.x,
            y: at.y,
            visible: snapshot.cursor_visible().unwrap_or(true),
        });

        let mut frame_rows = Vec::with_capacity(usize::from(rows));
        let mut row_iteration = live.rows.update(&snapshot)?;
        while let Some(row) = row_iteration.next() {
            let mut frame_cells = Vec::with_capacity(usize::from(cols));
            let mut cell_iteration = live.cells.update(row)?;
            while let Some(cell) = cell_iteration.next() {
                let style = cell.style()?;
                let mut foreground = cell.fg_color()?.map_or(default_foreground, Into::into);
                let mut background = cell.bg_color()?.map_or(default_background, Into::into);
                if style.inverse {
                    std::mem::swap(&mut foreground, &mut background);
                }
                let mut text = String::new();
                if !style.invisible {
                    cell.graphemes_utf8(&mut text)?;
                }
                frame_cells.push(Cell {
                    text,
                    foreground,
                    background,
                });
            }
            frame_rows.push(frame_cells);
        }

        Ok(Some(Frame {
            cols,
            rows,
            background: default_background,
            foreground: default_foreground,
            cells: frame_rows,
            cursor,
        }))
    }

    /// The terminal title, once the replica is live and set.
    pub fn title(&self) -> Option<String> {
        let State::Live(live) = &self.state else {
            return None;
        };
        let title = live.terminal.title().ok()?;
        (!title.is_empty()).then(|| title.to_owned())
    }
}

impl Live {
    fn new(terminal: Terminal, nonce: u64) -> Result<Self, Error> {
        Ok(Self {
            terminal,
            filter: StreamFilter::new(nonce),
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
        })
    }

    fn feed(&mut self, bytes: &[u8], fed: &mut Fed) {
        let terminal = &mut self.terminal;
        self.filter.feed(bytes, |piece| match piece {
            Piece::Output(output) => {
                terminal.vt_write(output);
                fed.changed = true;
            }
            Piece::Message(message) => {
                fed.changed = true;
                apply_message(terminal, message, fed);
            }
        });
    }
}

/// The cap on a buffered snapshot before giving up: real snapshots are a few
/// kilobytes to a few megabytes.
const MAX_SNAPSHOT_BYTES: usize = 256 << 20;

/// Restores the snapshot at the start of `buffer`, returning the terminal and
/// how many bytes the snapshot occupies, or nothing while bytes are still
/// missing. A buffer that doesn't even start with the magic fails fast.
fn try_restore(buffer: &[u8]) -> Result<Option<(Terminal, usize)>, Error> {
    const MAGIC: &[u8] = b"GHOSTSNP";
    if buffer.len() < MAGIC.len() {
        return Ok(None);
    }
    if !buffer.starts_with(MAGIC) {
        return Err(Error::NotASnapshot);
    }
    let mut decoder = snapshot::Decoder::from_slice(buffer);
    if decoder.ready().is_err() {
        // Not enough bytes yet (or a corrupt prefix; more bytes decide).
        return Ok(None);
    }
    // Restore scrollback history pages behind the snapshot's end marker.
    // Truncated input can also fail here; treat it as "wait for more
    // bytes", like a failed `ready`.
    loop {
        match decoder.next_page() {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => return Ok(None),
        }
    }
    let Some(consumed) = decoder.source_offset().ok().flatten() else {
        return Ok(None);
    };
    let Some(terminal) = decoder.into_terminal() else {
        return Ok(None);
    };
    Ok(Some((terminal, consumed)))
}

fn apply_message(terminal: &mut Terminal, message: RexMessage, fed: &mut Fed) {
    match message {
        RexMessage::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
            ..
        } => {
            let _ = terminal.resize(
                cols.max(1),
                rows.max(1),
                cell_width_px.max(1),
                cell_height_px.max(1),
            );
        }
        RexMessage::Theme {
            foreground,
            background,
            cursor,
            palette,
            ..
        } => {
            let color = |value: Option<String>| value.as_deref().and_then(parse_hex);
            let _ = terminal.set_default_fg_color(color(foreground));
            let _ = terminal.set_default_bg_color(color(background));
            let _ = terminal.set_default_cursor_color(color(cursor));
            let changes: Vec<(usize, RgbColor)> = palette
                .unwrap_or_default()
                .iter()
                .filter_map(|entry| {
                    let (index, value) = entry.split_once('=')?;
                    let index = index.trim().parse::<u8>().ok()?.into();
                    Some((index, parse_hex(value)?))
                })
                .collect();
            if !changes.is_empty() {
                if let Ok(mut base) = terminal.default_color_palette() {
                    for (index, color) in changes {
                        base.0[index] = color;
                    }
                    let _ = terminal.set_default_color_palette(Some(base));
                }
            }
        }
        RexMessage::ChildExited { exit_code } => fed.exited = Some(exit_code),
        RexMessage::Unknown => {}
    }
}

/// Parses `RRGGBB` or `#RRGGBB`.
pub fn parse_hex(value: &str) -> Option<RgbColor> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    Some(RgbColor {
        r: channel(0..2)?,
        g: channel(2..4)?,
        b: channel(4..6)?,
    })
}

/// A frame row as plain text, with trailing blanks trimmed.
pub fn row_text(row: &[Cell]) -> String {
    row.iter()
        .map(|cell| cell.text.as_str())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic terminal-stream file: a snapshot of a terminal that
    /// has already printed `text`, followed by stream bytes.
    fn snapshot_of(text: &[u8], cols: u16, rows: u16) -> Vec<u8> {
        let mut terminal = Terminal::new(ghostty_vt::terminal::Options {
            cols,
            rows,
            max_scrollback: 10_000_000,
        })
        .unwrap();
        terminal.vt_write(text);
        let bytes = snapshot::encode(&terminal).unwrap();
        bytes.as_ref().to_vec()
    }

    #[test]
    fn restores_a_snapshot_split_across_chunks_then_applies_the_stream() {
        let mut stream = snapshot_of(b"hello", 20, 4);
        stream.extend_from_slice(
            b" world\x1b_rex1;5;{\"type\":\"resize\",\"cols\":30,\"rows\":6}\x1b\\!",
        );
        stream.extend_from_slice(b"\x1b_rex1;5;{\"type\":\"child_exited\",\"exit_code\":2}\x1b\\");

        let mut replica = Replica::new(5).unwrap();
        let mut exited = None;
        for chunk in stream.chunks(7) {
            let fed = replica.feed(chunk).unwrap();
            exited = exited.or(fed.exited);
        }

        assert!(replica.is_live());
        assert_eq!(replica.size(), Some((30, 6)));
        assert_eq!(exited, Some(2));
        let frame = replica.frame().unwrap().unwrap();
        assert_eq!(frame.cols, 30);
        assert_eq!(row_text(&frame.cells[0]), "hello world!");
        assert!(frame.cursor.is_some());
    }

    #[test]
    fn ignores_a_forged_message_with_the_wrong_nonce() {
        let mut stream = snapshot_of(b"", 10, 2);
        // A program printed a look-alike: wrong nonce, and text around it.
        stream.extend_from_slice(
            b"\x1b_rex1;999;{\"type\":\"child_exited\",\"exit_code\":1}\x1b\\ok",
        );
        stream.extend_from_slice(b"\x1b_rex1;1;{\"type\":\"child_exited\",\"exit_code\":0}\x1b\\");

        let mut replica = Replica::new(1).unwrap();
        let mut exited = None;
        let fed = replica.feed(&stream).unwrap();
        exited = exited.or(fed.exited);

        // The forged exit never surfaced; the real one did.
        assert_eq!(exited, Some(0));
        // And the forged sequence was fed to the terminal as plain output,
        // not swallowed: the visible text still has "ok" after it.
        let frame = replica.frame().unwrap().unwrap();
        assert!(row_text(&frame.cells[0]).contains("ok"));
    }

    #[test]
    fn applies_theme_messages() {
        let mut stream = snapshot_of(b"", 10, 2);
        stream.extend_from_slice(
            b"\x1b_rex1;1;{\"type\":\"theme\",\"background\":\"#102030\",\"foreground\":\"a0b0c0\",\"palette\":[\"1=ff0000\"]}\x1b\\\x1b[31mx",
        );
        let mut replica = Replica::new(1).unwrap();
        replica.feed(&stream).unwrap();
        let frame = replica.frame().unwrap().unwrap();
        assert_eq!(
            frame.background,
            Color {
                r: 0x10,
                g: 0x20,
                b: 0x30
            }
        );
        assert_eq!(
            frame.foreground,
            Color {
                r: 0xa0,
                g: 0xb0,
                b: 0xc0
            }
        );
        assert_eq!(
            frame.cells[0][0].foreground,
            Color {
                r: 0xff,
                g: 0,
                b: 0
            }
        );
    }

    #[test]
    fn skips_unknown_message_types() {
        let mut stream = snapshot_of(b"", 10, 2);
        stream.extend_from_slice(b"\x1b_rex1;1;{\"type\":\"future\"}\x1b\\z");
        let mut replica = Replica::new(1).unwrap();
        let fed = replica.feed(&stream).unwrap();
        assert_eq!(fed.exited, None);
        let frame = replica.frame().unwrap().unwrap();
        assert_eq!(row_text(&frame.cells[0]), "z");
    }

    #[test]
    fn rejects_bytes_that_are_not_a_snapshot() {
        let mut replica = Replica::new(1).unwrap();
        assert!(matches!(
            replica.feed(b"plain output, no snapshot"),
            Err(Error::NotASnapshot)
        ));
    }

    #[test]
    fn replays_the_committed_probe_fixture() {
        // A real capture from `probe --command 'echo probe-ok ...'`, taken
        // against the local Rex server. The nonce is auto-detected from the
        // first rex1 APC, as `replay` does.
        let stream = include_bytes!("../../../probe-stream-probe.bin");
        let nonce = detect_nonce(stream).unwrap();
        let mut replica = Replica::new(nonce).unwrap();
        let mut exited = None;
        for chunk in stream.chunks(64) {
            let fed = replica.feed(chunk).unwrap();
            exited = exited.or(fed.exited);
        }
        assert!(replica.is_live());
        let frame = replica.frame().unwrap().unwrap();
        let screen: Vec<String> = frame.cells.iter().map(|r| row_text(r)).collect();
        let text = screen.join("\n");
        assert!(text.contains("probe-ok"), "screen was:\n{text}");
        assert!(text.contains("kivlor@kyrios"), "screen was:\n{text}");
        assert!(text.contains("TERM="), "screen was:\n{text}");
    }
}
