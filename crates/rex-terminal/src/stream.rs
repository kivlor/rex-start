//! Splits a Rex terminal stream, after its snapshot, into program output and
//! the server's own `rex1` APC messages.
//!
//! A Rex message is `ESC _ rex1;<nonce>;<json> ESC \`, where `<nonce>` is the
//! channel nonce from the data WELCOME. A program running in the terminal can
//! print a look-alike sequence, so a message whose nonce doesn't match is
//! passed through to the terminal as ordinary output, exactly as if the
//! program had printed it.

use serde::Deserialize;
use serde_json::Value;

const INTRODUCER: &[u8] = b"\x1b_rex1;";
const TERMINATOR: &[u8] = b"\x1b\\";
/// Rex messages are a few hundred bytes; anything this large is program
/// output that merely starts like one.
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// A message the server placed in a terminal stream
/// (docs/API-rev19.md, "The Terminal Stream").
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RexMessage {
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
        #[serde(default)]
        owner: Option<String>,
    },
    Theme {
        #[serde(default)]
        foreground: Option<String>,
        #[serde(default)]
        background: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        palette: Option<Vec<String>>,
        #[serde(default)]
        scheme: Option<String>,
    },
    ChildExited {
        #[serde(default)]
        exit_code: i64,
    },
    /// A well-formed message of a type this version doesn't know. The
    /// protocol says to skip it: it belongs to the server, not the program.
    #[serde(other)]
    Unknown,
}

/// One piece of a terminal stream, in stream order.
#[derive(Debug, PartialEq)]
pub enum Piece<'a> {
    /// Bytes for the terminal emulator.
    Output(&'a [u8]),
    /// A server message that passed the nonce check.
    Message(RexMessage),
}

/// Scans terminal-stream bytes for `rex1` APC messages with a given channel
/// nonce. Holds back only bytes that might still turn out to be part of a
/// message, so output reaches the terminal promptly.
pub struct StreamFilter {
    nonce: String,
    pending: Vec<u8>,
    /// Bytes without a terminator after which an introducer is given up on.
    max_message_bytes: usize,
}

impl StreamFilter {
    /// A filter for a stream whose WELCOME carried `nonce`.
    pub fn new(nonce: u64) -> Self {
        Self {
            nonce: nonce.to_string(),
            pending: Vec::new(),
            max_message_bytes: MAX_MESSAGE_BYTES,
        }
    }

    /// A filter with a smaller give-up cap, so tests can exercise the
    /// oversized-message path without feeding megabytes.
    #[cfg(test)]
    fn with_max_bytes(nonce: u64, max_message_bytes: usize) -> Self {
        Self {
            nonce: nonce.to_string(),
            pending: Vec::new(),
            max_message_bytes,
        }
    }

    /// Feeds the next stream bytes and reports the pieces they contain, in
    /// order. Look-alike sequences (wrong nonce, malformed JSON) are reported
    /// as output.
    pub fn feed(&mut self, input: &[u8], mut on_piece: impl FnMut(Piece<'_>)) {
        let buffer: &[u8] = if self.pending.is_empty() {
            input
        } else {
            self.pending.extend_from_slice(input);
            &self.pending
        };

        let mut position = 0;
        while position < buffer.len() {
            let Some(start) = find(&buffer[position..], INTRODUCER).map(|at| position + at) else {
                // No full introducer ahead: emit everything except a suffix
                // that might become one with the next chunk.
                let keep = partial_suffix(&buffer[position..], INTRODUCER);
                let end = buffer.len() - keep;
                if end > position {
                    on_piece(Piece::Output(&buffer[position..end]));
                }
                let held = buffer[end..].to_vec();
                self.pending = held;
                return;
            };
            if start > position {
                on_piece(Piece::Output(&buffer[position..start]));
            }
            let body_start = start + INTRODUCER.len();
            let Some(end) = find(&buffer[body_start..], TERMINATOR).map(|at| body_start + at)
            else {
                // Introducer without a terminator yet.
                if buffer.len() - start > self.max_message_bytes {
                    // Give up on it: this is program output that merely
                    // starts like a message. Emit the introducer itself and
                    // rescan the remainder as an ordinary stream, so later
                    // genuine messages are still found and no bytes are ever
                    // emitted or held twice.
                    on_piece(Piece::Output(&buffer[start..body_start]));
                    position = body_start;
                    continue;
                }
                let held = buffer[start..].to_vec();
                self.pending = held;
                return;
            };
            match parse_message(&buffer[body_start..end], &self.nonce) {
                Some(message) => on_piece(Piece::Message(message)),
                None => on_piece(Piece::Output(&buffer[start..end + TERMINATOR.len()])),
            }
            position = end + TERMINATOR.len();
        }
        self.pending.clear();
    }
}

/// Parses the body between the introducer and the terminator: decimal nonce,
/// a semicolon, then JSON. Returns nothing for anything else.
fn parse_message(body: &[u8], nonce: &str) -> Option<RexMessage> {
    let body = std::str::from_utf8(body).ok()?;
    let (given, json) = body.split_once(';')?;
    if given != nonce {
        return None;
    }
    let value: Value = serde_json::from_str(json).ok()?;
    serde_json::from_value(value).ok()
}

/// Finds the first occurrence of `needle` in `haystack` (both may be empty
/// only trivially; callers pass non-empty needles).
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The length of the longest suffix of `bytes` that is a proper prefix of
/// `needle`, so a split introducer can be held until the rest arrives.
fn partial_suffix(bytes: &[u8], needle: &[u8]) -> usize {
    (1..needle.len().min(bytes.len() + 1))
        .rev()
        .find(|&len| bytes.ends_with(&needle[..len]))
        .unwrap_or(0)
}

/// Scans raw stream bytes for the channel nonce. Prefers the first
/// `rex1` APC whose JSON carries a known message type (resize, theme,
/// child_exited): the server's first message, a resize, comes right after
/// the snapshot, while a program inside it may have printed a look-alike.
/// Falls back to the first APC with a decimal nonce if none parses. Returns
/// nothing when the file holds no APC at all.
pub fn detect_nonce(bytes: &[u8]) -> Option<u64> {
    let mut fallback = None;
    let mut from = 0;
    while let Some(hit) = find(&bytes[from..], INTRODUCER).map(|at| from + at) {
        let body_start = hit + INTRODUCER.len();
        let Some(end) = find(&bytes[body_start..], TERMINATOR).map(|at| body_start + at) else {
            from = body_start;
            continue;
        };
        let body = &bytes[body_start..end];
        if let Some((given, json)) = utf8(body).and_then(|b| b.split_once(';')) {
            if let Ok(nonce) = given.parse::<u64>() {
                if known_type(json) {
                    return Some(nonce);
                }
                fallback = fallback.or(Some(nonce));
            }
        }
        from = end + TERMINATOR.len();
    }
    fallback
}

fn utf8(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

/// True when `json` is a message object with a type this version knows.
fn known_type(json: &str) -> bool {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .is_some_and(|kind| matches!(kind.as_str(), "resize" | "theme" | "child_exited"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(filter: &mut StreamFilter, chunks: &[&[u8]]) -> (Vec<u8>, Vec<RexMessage>) {
        let mut output = Vec::new();
        let mut messages = Vec::new();
        for chunk in chunks {
            filter.feed(chunk, |piece| match piece {
                Piece::Output(bytes) => output.extend_from_slice(bytes),
                Piece::Message(message) => messages.push(message),
            });
        }
        (output, messages)
    }

    #[test]
    fn extracts_messages_with_matching_nonce() {
        let mut filter = StreamFilter::new(42);
        let (output, messages) = collect(
            &mut filter,
            &[b"hi\x1b_rex1;42;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\there"],
        );
        assert_eq!(output, b"hithere");
        assert_eq!(
            messages,
            [RexMessage::Resize {
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                owner: None
            }]
        );
    }

    #[test]
    fn passes_wrong_nonce_look_alikes_through_as_output() {
        let mut filter = StreamFilter::new(42);
        let forged: &[u8] = b"\x1b_rex1;7;{\"type\":\"child_exited\",\"exit_code\":1}\x1b\\plain";
        let (output, messages) = collect(&mut filter, &[forged]);
        assert_eq!(output, forged);
        assert!(messages.is_empty());
    }

    #[test]
    fn passes_malformed_json_through_as_output() {
        let mut filter = StreamFilter::new(42);
        let (output, messages) = collect(&mut filter, &[b"\x1b_rex1;42;{not json}\x1b\\tail"]);
        assert_eq!(output, b"\x1b_rex1;42;{not json}\x1b\\tail");
        assert!(messages.is_empty());
    }

    #[test]
    fn reassembles_messages_split_across_chunks() {
        let stream: &[u8] = b"ab\x1b_rex1;9;{\"type\":\"child_exited\",\"exit_code\":3}\x1b\\cd";
        for split in 0..stream.len() {
            let mut filter = StreamFilter::new(9);
            let (output, messages) = collect(&mut filter, &[&stream[..split], &stream[split..]]);
            assert_eq!(output, b"abcd", "split at {split}");
            assert_eq!(messages, [RexMessage::ChildExited { exit_code: 3 }]);
        }
    }

    #[test]
    fn drops_a_pending_introducer_that_never_completes() {
        let mut filter = StreamFilter::new(9);
        let (output, _) = collect(&mut filter, &[b"x\x1b"]);
        assert_eq!(output, b"x");
        let (output, _) = collect(&mut filter, &[b"[A"]);
        assert_eq!(output, b"\x1b[A");
    }

    #[test]
    fn ignores_unknown_message_types() {
        let mut filter = StreamFilter::new(1);
        let (output, messages) =
            collect(&mut filter, &[b"\x1b_rex1;1;{\"type\":\"future\"}\x1b\\"]);
        assert!(output.is_empty());
        assert_eq!(messages, [RexMessage::Unknown]);
    }

    #[test]
    fn detects_nonce_from_first_apc() {
        let bytes = b"junk\x1b_rex1;12433518746600474347;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\more";
        assert_eq!(detect_nonce(bytes), Some(12_433_518_746_600_474_347));
        assert_eq!(detect_nonce(b"no apc here"), None);
    }

    #[test]
    fn detect_nonce_prefers_a_known_type_over_an_earlier_look_alike() {
        // A program printed a look-alike APC (unknown type) before the
        // server's genuine resize; the genuine one must win.
        let bytes = b"\x1b_rex1;7;{\"type\":\"prog-thing\"}\x1b\\x\x1b_rex1;42;{\"type\":\"resize\",\"cols\":80,\"rows\":24}\x1b\\";
        assert_eq!(detect_nonce(bytes), Some(42));
        // With no known-type APC anywhere, the first decimal nonce is kept
        // as a fallback.
        assert_eq!(detect_nonce(b"\x1b_rex1;9;{nope}\x1b\\"), Some(9));
    }

    #[test]
    fn gives_up_on_an_oversized_introducer_without_duplicating_output() {
        // An introducer followed by terminator-less bytes well past the
        // cap, then a genuine message, fed in small chunks.
        let mut input: Vec<u8> = b"junk\x1b_rex1;".to_vec();
        input.extend(std::iter::repeat_n(b'x', 512));
        let genuine = b"\x1b_rex1;9;{\"type\":\"child_exited\",\"exit_code\":3}\x1b\\tail";
        let full: Vec<u8> = [&input, genuine.as_slice()].concat();
        // What should come back out: everything except the consumed genuine
        // message — the given-up introducer and its junk included, once.
        let mut expected = input.clone();
        expected.extend_from_slice(b"tail");

        let mut filter = StreamFilter::with_max_bytes(9, 256);
        let mut output = Vec::new();
        let mut messages = Vec::new();
        for chunk in full.chunks(7) {
            filter.feed(chunk, |piece| match piece {
                Piece::Output(bytes) => output.extend_from_slice(bytes),
                Piece::Message(message) => messages.push(message),
            });
        }
        // (a) Every input byte accounted for exactly once: emitted once,
        // or consumed as the one genuine message.
        assert_eq!(output, expected);
        // (c) The genuine message after the given-up introducer is found.
        assert_eq!(messages, [RexMessage::ChildExited { exit_code: 3 }]);

        // (b) Later feeds keep the stream ordinary: nothing re-emitted,
        // nothing dropped.
        let tail = b"after\x1b_rex1;9;{\"type\":\"resize\",\"cols\":10,\"rows\":2}\x1b\\!";
        filter.feed(tail, |piece| match piece {
            Piece::Output(bytes) => output.extend_from_slice(bytes),
            Piece::Message(message) => messages.push(message),
        });
        expected.extend_from_slice(b"after!");
        assert_eq!(output, expected);
        assert_eq!(
            messages[1..],
            [RexMessage::Resize {
                cols: 10,
                rows: 2,
                cell_width_px: 0,
                cell_height_px: 0,
                owner: None
            }]
        );
    }
}
