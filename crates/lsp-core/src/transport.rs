//! Hand-rolled JSON-RPC 2.0 stdio message framing: `Content-Length: N\r\n\r\n<json>`, per the
//! LSP spec's "Base Protocol" section. This is deliberately hand-rolled rather than pulled in
//! from an off-the-shelf JSON-RPC crate or `vendor/zed/crates/lsp` (GPL-3.0-or-later, so not a
//! dependency this permissively-licensed project can take) - it is genuinely a few dozen lines,
//! and is the one piece of this crate that has to be written from scratch rather than delegated
//! to `lsp-types` (which only defines the message *payload* shapes, not the header framing
//! around them).
//!
//! Header lines are ASCII, `\r\n`-terminated, and end with one blank `\r\n` line before the
//! JSON body starts. `Content-Length` is the only header this implementation looks at
//! (`Content-Type` is a legal but unused-by-every-real-LSP-server header per the spec; it is
//! read and discarded here like any other unrecognized header line, not rejected).

use std::io::{self, BufRead, Read, Write};

/// Real upper bound on one framed message's declared `Content-Length`, in bytes. Real LSP
/// traffic - even a large `publishDiagnostics` payload for a whole-file reanalysis - is nowhere
/// near this; the cap exists purely so a hostile or desynced peer's claimed length is rejected
/// with a real, immediate error *before* this process ever attempts to allocate it, rather than
/// either aborting the whole process (an allocation failure is uncatchable in Rust) or wedging
/// the reader thread in a blocking read forever for a moderately-large claimed length. See
/// [`read_message`]'s own docs for the real desync scenario (a PATH-shadowing wrapper script
/// printing a stray line before real LSP traffic starts) this defends against, not just an
/// outright-malicious peer.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Writes one real, framed JSON-RPC message: `Content-Length: <n>\r\n\r\n<json bytes>`, then
/// flushes so the bytes actually leave this process rather than sitting in a `Write` adapter's
/// internal buffer (load-bearing for a child process reading from a pipe with no buffering of
/// its own to force a flush on).
pub fn write_message<W: Write>(writer: &mut W, value: &serde_json::Value) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::from)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one real, framed JSON-RPC message from `reader`. Returns `Ok(None)` on a clean EOF
/// encountered while reading the *first* header line (the real, expected shutdown signal: the
/// peer closed its write side because it exited) - a real error is still returned for anything
/// that looks like a message that started but was cut off mid-frame, so a genuinely truncated
/// stream is never silently treated as "no more messages".
///
/// A declared `Content-Length` over [`MAX_MESSAGE_BYTES`] is rejected outright (a real
/// `InvalidData` error naming both the claimed length and the cap) rather than allocated - a
/// real, if unlikely, hazard: a PATH-shadowing wrapper script that prints one stray line to
/// stdout before real LSP traffic starts desyncs this framer, so *some* later, arbitrary byte
/// sequence gets parsed as a `Content-Length` value, which could be astronomically large. The
/// body itself is read via [`Read::take`] rather than pre-allocated up front at the claimed
/// size, so a body that's genuinely shorter than declared also produces a real, clean error
/// (`UnexpectedEof`) instead of blocking forever waiting for bytes that will never arrive.
pub fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<serde_json::Value>> {
    let mut content_length: Option<usize> = None;
    let mut header_lines_read = 0usize;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            if header_lines_read == 0 {
                return Ok(None); // clean EOF between messages
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "stream ended mid-header while reading a framed LSP message",
            ));
        }
        header_lines_read += 1;

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // blank line: end of headers, body follows immediately
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
            // Any other header (e.g. `Content-Type`) is real but unused here - read and
            // discarded, per the spec allowing (but not requiring) a client to ignore it.
        }
    }

    let content_length = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "framed LSP message had no Content-Length header",
        )
    })?;

    if content_length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "framed LSP message declared Content-Length {content_length}, which exceeds \
                 the {MAX_MESSAGE_BYTES}-byte cap - refusing to allocate it (likely a desynced \
                 or hostile peer)"
            ),
        ));
    }

    // Streamed via `Read::take` rather than `vec![0u8; content_length]` + `read_exact` - see
    // this function's own docs. `bytes_read` genuinely less than `content_length` (a body cut
    // off mid-frame) is a real, distinct error from "declared length too large" above.
    let mut body = Vec::new();
    let bytes_read = reader.take(content_length as u64).read_to_end(&mut body)?;
    if bytes_read != content_length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "framed LSP message body ended after {bytes_read} of {content_length} declared \
                 bytes"
            ),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(io::Error::from)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::io::Cursor;

    #[test]
    fn round_trips_a_real_message_through_encode_then_decode() {
        let original = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "processId": 123, "rootUri": null }
        });

        let mut buffer = Vec::new();
        write_message(&mut buffer, &original).expect("write_message should succeed");

        let mut reader = BufReader::new(Cursor::new(buffer));
        let decoded = read_message(&mut reader)
            .expect("read_message should succeed")
            .expect("a real message should have been present");
        assert_eq!(decoded, original);
    }

    #[test]
    fn the_wire_format_matches_the_real_spec_exactly() {
        let value = serde_json::json!({"a": 1});
        let mut buffer = Vec::new();
        write_message(&mut buffer, &value).expect("write_message should succeed");

        let body = serde_json::to_vec(&value).expect("serialize");
        let expected = format!(
            "Content-Length: {}\r\n\r\n{}",
            body.len(),
            String::from_utf8_lossy(&body)
        );
        assert_eq!(String::from_utf8_lossy(&buffer), expected);
    }

    #[test]
    fn reads_two_real_back_to_back_messages_from_one_stream() {
        let first = serde_json::json!({"jsonrpc": "2.0", "method": "a", "params": {}});
        let second = serde_json::json!({"jsonrpc": "2.0", "method": "b", "params": {}});

        let mut buffer = Vec::new();
        write_message(&mut buffer, &first).expect("write first");
        write_message(&mut buffer, &second).expect("write second");

        let mut reader = BufReader::new(Cursor::new(buffer));
        let decoded_first = read_message(&mut reader)
            .expect("read first")
            .expect("first message present");
        let decoded_second = read_message(&mut reader)
            .expect("read second")
            .expect("second message present");
        assert_eq!(decoded_first, first);
        assert_eq!(decoded_second, second);
    }

    #[test]
    fn a_clean_eof_before_any_message_returns_none_not_an_error() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let result = read_message(&mut reader).expect("a clean EOF should not be an error");
        assert!(result.is_none());
    }

    #[test]
    fn a_stream_that_dies_mid_header_is_a_real_error_not_a_silent_none() {
        let mut reader = BufReader::new(Cursor::new(b"Content-Le".to_vec()));
        let result = read_message(&mut reader);
        assert!(
            result.is_err(),
            "a truncated header line must not be treated as a clean end of stream"
        );
    }

    #[test]
    fn a_stream_that_dies_mid_body_is_a_real_error() {
        let mut reader =
            BufReader::new(Cursor::new(b"Content-Length: 50\r\n\r\n{\"a\":1}".to_vec()));
        let result = read_message(&mut reader);
        assert!(
            result.is_err(),
            "a body shorter than its own declared Content-Length must be a real error"
        );
    }

    #[test]
    fn a_missing_content_length_header_is_a_real_error() {
        let mut reader = BufReader::new(Cursor::new(
            b"Content-Type: application/json\r\n\r\n{}".to_vec(),
        ));
        let result = read_message(&mut reader);
        assert!(
            result.is_err(),
            "a message with no Content-Length header at all must be rejected, not guessed at"
        );
    }

    #[test]
    fn a_content_length_far_exceeding_the_cap_is_rejected_without_allocating() {
        // No real body follows - if this were mis-implemented to allocate/read before checking
        // the cap, this test would hang or abort rather than return a real error quickly, which
        // is exactly what this test exists to catch.
        let mut reader = BufReader::new(Cursor::new(
            b"Content-Length: 999999999999\r\n\r\n".to_vec(),
        ));
        let start = std::time::Instant::now();
        let result = read_message(&mut reader);
        assert!(
            result.is_err(),
            "a Content-Length far exceeding the real cap must be a real error, not a panic/abort/hang"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "rejecting an over-cap Content-Length must be near-instant (no real large \
             allocation attempted), took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_content_length_exactly_one_byte_over_the_cap_is_rejected() {
        // A boundary check distinct from the "far exceeding" case above - proves the cap
        // comparison itself (`content_length > MAX_MESSAGE_BYTES`) is exact, not off-by-one.
        let header = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        let mut reader = BufReader::new(Cursor::new(header.into_bytes()));
        let result = read_message(&mut reader);
        assert!(
            result.is_err(),
            "a Content-Length exactly one byte over the cap must still be rejected"
        );
    }

    #[test]
    fn a_real_in_bounds_multi_kilobyte_body_still_round_trips_through_the_streamed_read() {
        // Proves the `Read::take` + `read_to_end` rewrite (replacing the old `vec![0u8; n]` +
        // `read_exact`) still correctly reads a real, complete, larger-than-trivial body - not
        // just tiny fixtures - and doesn't off-by-one truncate it.
        let large_string = "x".repeat(200_000);
        let value = serde_json::json!({ "payload": large_string });
        let mut buffer = Vec::new();
        write_message(&mut buffer, &value).expect("write_message should succeed");

        let mut reader = BufReader::new(Cursor::new(buffer));
        let decoded = read_message(&mut reader)
            .expect("read_message should succeed")
            .expect("a real message should have been present");
        assert_eq!(decoded, value);
    }

    #[test]
    fn an_unrecognized_extra_header_is_read_and_ignored() {
        let value = serde_json::json!({"ok": true});
        let body = serde_json::to_vec(&value).expect("serialize");
        let mut buffer = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        buffer.extend_from_slice(&body);

        let mut reader = BufReader::new(Cursor::new(buffer));
        let decoded = read_message(&mut reader)
            .expect("read_message should succeed")
            .expect("message present");
        assert_eq!(decoded, value);
    }
}
