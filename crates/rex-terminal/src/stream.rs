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
}

impl StreamFilter {
    /// A filter for a stream whose WELCOME carried `nonce`.
    pub fn new(nonce: u64) -> Self {
        Self {
            nonce: nonce.to_string(),
            pending: Vec::new(),
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
                if buffer.len() - start > MAX_MESSAGE_BYTES {
                    // Give up on it: this is program output that merely
                    // starts like a message.
                    on_piece(Piece::Output(&buffer[start..]));
                    let tail = buffer[start..].to_vec();
                    self.pending = tail;
                } else {
                    let held = buffer[start..].to_vec();
                    self.pending = held;
                }
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

/// Scans raw stream bytes for the first `rex1` APC and returns its decimal
/// nonce. Used by `replay` when the WELCOME's nonce wasn't recorded: the
/// server's first message (a resize) comes right after the snapshot, so the
/// first well-formed APC in the file is the channel's own. Returns nothing
/// when the file holds no APC at all.
pub fn detect_nonce(bytes: &[u8]) -> Option<u64> {
    let mut from = 0;
    while let Some(hit) = find(&bytes[from..], INTRODUCER).map(|at| from + at) {
        let body_start = hit + INTRODUCER.len();
        if let Some(end) = find(&bytes[body_start..], TERMINATOR).map(|at| body_start + at) {
            if let Ok(body) = std::str::from_utf8(&bytes[body_start..end]) {
                if let Some((given, _)) = body.split_once(';') {
                    if let Ok(nonce) = given.parse::<u64>() {
                        return Some(nonce);
                    }
                }
            }
        }
        from = body_start;
    }
    None
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
}
