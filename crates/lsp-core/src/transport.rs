//! JSON-RPC 2.0 stdio framing: `Content-Length: N\r\n\r\n<json>`.
//!
//! Hand-rolled because `lsp-types` defines payload shapes but not framing, and Zed's own `lsp`
//! crate is GPL. It is a few dozen lines.
//!
//! Headers are ASCII, `\r\n`-terminated, and end with a blank line. `Content-Length` is the only
//! one read; anything else is discarded rather than rejected.

use std::io::{self, BufRead, Read, Write};

/// Upper bound on a declared `Content-Length`, well above any real traffic.
///
/// Rejects a desynced peer's claimed length *before* allocating it - an allocation failure is
/// uncatchable in Rust, and a merely large claim would wedge the reader in a blocking read.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Why a [`write_message_bounded`] call gave up, and how far into the frame it got.
///
/// `bytes_written > 0` means a partial frame is already in the peer's pipe with no way to finish
/// it, so the peer's framer is permanently desynced - mid-body, waiting on bytes that will never
/// arrive, and mis-framing everything after. A caller seeing that must treat the connection as
/// dead rather than retry on it.
#[derive(Debug)]
pub enum BoundedWriteError {
    /// The deadline elapsed with the peer still not accepting bytes.
    ///
    /// Never constructed on Windows, whose [`write_message_bounded`] is unbounded.
    #[cfg_attr(not(unix), allow(dead_code))]
    Timeout { bytes_written: usize },
    Io {
        source: io::Error,
        bytes_written: usize,
    },
}

impl BoundedWriteError {
    /// `true` when a partial frame reached the peer, which is unrecoverable rather than a
    /// failed call.
    pub fn stream_desynced(&self) -> bool {
        match self {
            Self::Timeout { bytes_written } | Self::Io { bytes_written, .. } => *bytes_written > 0,
        }
    }

    /// The underlying `io::Error`, synthesizing a [`io::ErrorKind::TimedOut`] one for the timeout
    /// case so a caller need not match on the variant.
    pub fn into_io_error(self) -> io::Error {
        match self {
            Self::Timeout { .. } => io::Error::new(
                io::ErrorKind::TimedOut,
                "the language server stopped reading its stdin",
            ),
            Self::Io { source, .. } => source,
        }
    }
}

/// Serializes `value` into one framed message. The only place the wire format is encoded, so the
/// two platform paths cannot drift apart.
fn frame(value: &serde_json::Value) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value).map_err(io::Error::from)?;
    let mut frame = Vec::with_capacity(body.len() + 32);
    write!(&mut frame, "Content-Length: {}\r\n\r\n", body.len())?;
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes one framed message to a **non-blocking** fd, giving up with
/// [`BoundedWriteError::Timeout`] if `timeout` elapses before the peer accepts the whole frame.
///
/// A blocking `write_all` has no time bound: against a server that stops reading its stdin, a
/// 256 KiB `didChange` fills the pipe's ~64 KiB buffer and parks forever - *holding* the stdin
/// mutex, so every later call blocks before reaching its own timeout, while the still-alive
/// process means the reader never sees EOF and the connection keeps reporting itself healthy.
///
/// `O_NONBLOCK`, rather than polling then writing the rest, because POSIX says a blocking `write`
/// past `PIPE_BUF` returns only once *all* bytes are written - so it would still park mid-frame.
/// Passing a blocking fd is not unsafe, it just silently restores the unbounded behaviour.
#[cfg(unix)]
pub fn write_message_bounded<W: Write + std::os::fd::AsFd>(
    writer: &mut W,
    value: &serde_json::Value,
    timeout: std::time::Duration,
) -> Result<(), BoundedWriteError> {
    use nix::poll::{PollFd, PollFlags, PollTimeout};

    let frame = frame(value).map_err(|source| BoundedWriteError::Io {
        source,
        bytes_written: 0,
    })?;
    // A *no-progress* deadline, refreshed per accepted byte, not one budget for the whole frame:
    // an absolute deadline would kill a peer that is draining slowly and call it desynced. What
    // this guards against is a peer that stopped reading altogether.
    let mut deadline = std::time::Instant::now() + timeout;
    let mut written = 0usize;

    while written < frame.len() {
        match writer.write(&frame[written..]) {
            Ok(0) => {
                return Err(BoundedWriteError::Io {
                    source: io::Error::new(io::ErrorKind::WriteZero, "wrote zero bytes"),
                    bytes_written: written,
                });
            }
            Ok(count) => {
                written += count;
                deadline = std::time::Instant::now() + timeout;
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(source) => {
                return Err(BoundedWriteError::Io {
                    source,
                    bytes_written: written,
                });
            }
        }

        // Pipe full: wait, bounded by what is left of the deadline, for the peer to drain.
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(BoundedWriteError::Timeout {
                bytes_written: written,
            });
        }
        // Clamping an overflowing duration keeps the deadline check above the sole authority on
        // when to give up, rather than adding a second failure mode.
        let poll_timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(writer.as_fd(), PollFlags::POLLOUT)];
        match nix::poll::poll(&mut fds, poll_timeout) {
            // The peer freed no pipe space at all within the whole budget.
            Ok(0) => {
                return Err(BoundedWriteError::Timeout {
                    bytes_written: written,
                });
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => {}
            Err(errno) => {
                return Err(BoundedWriteError::Io {
                    source: io::Error::from(errno),
                    bytes_written: written,
                });
            }
        }
    }

    // `bytes_written: 0` even though the whole frame went out: that field means "a *partial* frame
    // reached the peer, so its framer is desynced". Passing `written` here would make
    // `stream_desynced` claim a corruption that did not happen.
    writer.flush().map_err(|source| BoundedWriteError::Io {
        source,
        bytes_written: 0,
    })
}

/// Windows twin of [`write_message_bounded`], with **no** time bound: `poll` does not exist for
/// anonymous pipes there. A tracked gap rather than a fake timeout.
#[cfg(not(unix))]
pub fn write_message_bounded<W: Write>(
    writer: &mut W,
    value: &serde_json::Value,
    _timeout: std::time::Duration,
) -> Result<(), BoundedWriteError> {
    let frame = frame(value).map_err(|source| BoundedWriteError::Io {
        source,
        bytes_written: 0,
    })?;
    writer
        .write_all(&frame)
        .and_then(|()| writer.flush())
        .map_err(|source| BoundedWriteError::Io {
            source,
            // A failed `write_all` may have written part of the frame with no way to find out how
            // much, so this reports a desync - treating it as recoverable is the unsafe direction.
            bytes_written: 1,
        })
}

/// Reads one framed message. `Ok(None)` only for a clean EOF on the *first* header line - the
/// peer exiting - so a stream cut off mid-frame errors rather than reading as "no more messages".
///
/// A `Content-Length` over [`MAX_MESSAGE_BYTES`] is rejected rather than allocated: a wrapper
/// script printing one stray line before LSP traffic desyncs this framer, after which some
/// arbitrary byte sequence parses as a length. The body streams through [`Read::take`], so a body
/// shorter than declared errors instead of blocking for bytes that never arrive.
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
            // Any other header is discarded; the spec allows ignoring them.
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

    // Streamed rather than pre-allocated. A short read is a distinct error from "length too big".
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

    /// Appends a framed message via [`frame`] itself, so these tests pin the encoder production
    /// uses rather than a second copy written for their convenience.
    fn write_message(buffer: &mut Vec<u8>, value: &serde_json::Value) -> io::Result<()> {
        buffer.extend_from_slice(&frame(value)?);
        Ok(())
    }

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
        // No body follows: allocating or reading before checking the cap would hang or abort here
        // rather than erroring quickly.
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
        // A boundary case, proving the cap comparison is exact rather than off-by-one.
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
        // A body larger than the tiny fixtures elsewhere, so streaming cannot truncate it unseen.
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

    /// Covers the write deadline against an OS pipe nobody drains, with no language server
    /// involved, so it runs in under a second.
    ///
    /// `sleep` is spawned purely for its stdin: it holds the read end open and never reads it,
    /// which is exactly the frozen-server condition the bound exists for.
    #[cfg(unix)]
    mod bounded_write_tests {
        use super::*;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        struct UndrainedPipe {
            child: std::process::Child,
            stdin: std::process::ChildStdin,
        }

        impl UndrainedPipe {
            fn new() -> Self {
                let mut child = Command::new("sleep")
                    .arg("60")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawning a real `sleep` for its stdin pipe");
                let stdin = child.stdin.take().expect("piped stdin");
                // The same `O_NONBLOCK` production sets at spawn, which the bound requires.
                use std::os::fd::AsRawFd;
                let fd = stdin.as_raw_fd();
                let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).expect("F_GETFL");
                let flags =
                    nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
                nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags)).expect("F_SETFL");
                Self { child, stdin }
            }
        }

        impl Drop for UndrainedPipe {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        /// A message fitting in the pipe buffer must not have been turned into a timeout.
        #[test]
        fn a_small_message_still_writes_straight_through() {
            let mut pipe = UndrainedPipe::new();
            let value = serde_json::json!({"jsonrpc": "2.0", "method": "initialized"});
            write_message_bounded(&mut pipe.stdin, &value, Duration::from_secs(5))
                .expect("a message that fits in the pipe buffer must not time out");
        }

        /// A frame far larger than the pipe buffer, to a peer that never reads, must give up on
        /// the deadline and report the partial write that tells the caller the framer is desynced.
        #[test]
        fn an_oversized_message_to_a_peer_that_never_reads_times_out_and_reports_the_desync() {
            let mut pipe = UndrainedPipe::new();
            let value = serde_json::json!({ "payload": "x".repeat(1024 * 1024) });

            let started = Instant::now();
            let error = write_message_bounded(&mut pipe.stdin, &value, Duration::from_millis(250))
                .expect_err("a 1 MiB frame cannot fit in a pipe nobody is draining");
            let elapsed = started.elapsed();

            assert!(
                matches!(error, BoundedWriteError::Timeout { .. }),
                "the peer is alive and the fd is fine - this is a real timeout, not an I/O \
                 error: {error:?}"
            );
            assert!(
                error.stream_desynced(),
                "part of the frame genuinely reached the pipe, so the peer's framer is mid-body \
                 and can never recover - the caller has to be told that, not just that one write \
                 failed"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "the write must give up on the 250ms deadline it was given, not block - took \
                 {elapsed:?}"
            );
        }
    }
}
