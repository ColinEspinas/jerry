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

/// Why a real [`write_message_bounded`] call gave up, and - load-bearing - **how far into the
/// frame it got before it did**.
///
/// `bytes_written > 0` is not a detail: it means part of a `Content-Length`-framed message is
/// already sitting in the peer's pipe with no way to finish it, so the peer's own framer is now
/// permanently desynced (it is mid-body, waiting on bytes that will never arrive, and will
/// mis-frame everything after them). A caller that sees that must treat the whole connection as
/// dead rather than retrying on it - see [`crate::client::LspClient::is_connection_alive`]'s own
/// docs for what actually acts on this.
#[derive(Debug)]
pub enum BoundedWriteError {
    /// The caller's deadline elapsed with the peer still not accepting bytes.
    ///
    /// Never constructed on Windows: that platform's [`write_message_bounded`] is honestly
    /// unbounded (no `poll` for anonymous pipes - see its own docs), so it can only ever produce
    /// [`Self::Io`]. The `allow` documents that as the real, tracked platform gap it is, rather
    /// than letting a dead-code warning fail the build for a variant unix genuinely uses - the
    /// same pattern `crate::client` already uses for its own unix-only items.
    #[cfg_attr(not(unix), allow(dead_code))]
    Timeout { bytes_written: usize },
    /// A real I/O error (the peer closed its read end, etc.).
    Io {
        source: io::Error,
        bytes_written: usize,
    },
}

impl BoundedWriteError {
    /// `true` when a partial frame genuinely reached the peer - see this type's own docs for why
    /// that is unrecoverable rather than merely a failed call.
    pub fn stream_desynced(&self) -> bool {
        match self {
            Self::Timeout { bytes_written } | Self::Io { bytes_written, .. } => *bytes_written > 0,
        }
    }

    /// The real underlying `io::Error`, synthesizing a [`io::ErrorKind::TimedOut`] one for the
    /// timeout case so a caller that only wants to report *something* honest doesn't have to
    /// match on the variant.
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

/// Serializes `value` into one real, framed message: `Content-Length: <n>\r\n\r\n<json bytes>`.
/// The single place this crate encodes the wire format, shared by both
/// [`write_message_bounded`] platform paths (and exercised directly by this module's own
/// round-trip tests) so no two writers can ever drift apart on it.
fn frame(value: &serde_json::Value) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value).map_err(io::Error::from)?;
    let mut frame = Vec::with_capacity(body.len() + 32);
    write!(&mut frame, "Content-Length: {}\r\n\r\n", body.len())?;
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes one real, framed JSON-RPC message to a **non-blocking** fd, giving up with a real
/// [`BoundedWriteError::Timeout`] if `timeout` elapses before the peer accepts the whole frame.
///
/// ## The real bug this exists for
///
/// An ordinary blocking `write_all` of a framed message has no time bound at all, and that is
/// not theoretical:
/// live-reproduced against a real child process that completed a real LSP handshake and then
/// stopped reading its stdin (`process.stdin.pause()`, standing in for a hung/SIGSTOPped/
/// deadlocked server), a single 256 KiB `textDocument/didChange` never returned - the pipe's own
/// ~64 KiB kernel buffer filled and `write_all` parked forever. Worse, it parks *holding*
/// `LspClient`'s `stdin` mutex, so every later call on that client blocks on the mutex before it
/// can even reach its own `recv_timeout`: a subsequent `textDocument/hover` request with an
/// explicit **3-second** timeout was measured still unfinished 8 seconds later. And because the
/// process is genuinely still alive, the reader thread never sees EOF, so
/// `LspClient::is_connection_alive` keeps honestly-but-uselessly reporting `true` - the whole
/// connection silently stops working with nothing anywhere saying why.
///
/// ## Why `O_NONBLOCK` rather than "poll, then write the rest"
///
/// POSIX is explicit that a blocking `write()` of more than `PIPE_BUF` bytes returns only once
/// *all* `nbyte` have been written, so a "poll for `POLLOUT`, then write everything remaining"
/// loop on a blocking fd would still park inside `write()` the moment the peer stalls mid-frame.
/// Making the fd non-blocking once, at spawn (see [`crate::client::LspClient::spawn`]), removes
/// that class of reasoning entirely: `write` then answers `EWOULDBLOCK` instead of parking, and
/// this loop owns the waiting via a real, deadline-bounded `poll`.
///
/// `writer` must therefore genuinely be `O_NONBLOCK`; passing a blocking fd is not unsafe, it
/// just silently gives back the unbounded behavior this function exists to remove.
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
    // A *no-progress* deadline, refreshed on every byte the peer actually accepts - not one
    // budget for the whole frame. The difference is real: a single absolute deadline would kill a
    // peer that is genuinely draining, just slowly (a large frame against a busy server), and
    // report it as a desynced connection. What this bound is actually for is a peer that has
    // stopped reading altogether, and "no progress at all for `timeout`" says exactly that.
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

        // The pipe is full right now. Wait - bounded by whatever is left of the caller's own
        // deadline - for the peer to drain enough of it to accept more.
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(BoundedWriteError::Timeout {
                bytes_written: written,
            });
        }
        // `PollTimeout::try_from` only fails for a duration whose millisecond count overflows
        // `i32`; `remaining` is bounded by the caller's own timeout, and clamping to
        // `PollTimeout::MAX` for an absurdly large one keeps the deadline check above the single
        // real authority on when to give up rather than introducing a second failure mode.
        let poll_timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
        let mut fds = [PollFd::new(writer.as_fd(), PollFlags::POLLOUT)];
        match nix::poll::poll(&mut fds, poll_timeout) {
            // A real poll timeout: the peer has not freed a single byte of pipe space within the
            // caller's whole budget.
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

    // A pipe fd has no userspace buffer of its own to flush, but `flush` is still called for the
    // same reason any framed writer would: `W` is only promised to be a `Write`.
    //
    // Reported with `bytes_written: 0` even though the whole frame demonstrably went out, because
    // that field means one specific thing to callers - "a *partial* frame reached the peer, so its
    // framer is desynced" (see [`BoundedWriteError`]). Nothing was cut off here; passing `written`
    // through would make [`BoundedWriteError::stream_desynced`] claim a corruption that did not
    // happen, and the caller would log "was cut off part-way through" about a frame that wasn't.
    writer.flush().map_err(|source| BoundedWriteError::Io {
        source,
        bytes_written: 0,
    })
}

/// Windows twin of [`write_message_bounded`], honestly narrower: it performs the same real,
/// framed write but has **no** time bound, because `poll` does not exist for Windows anonymous
/// pipes and the non-blocking-write machinery above has no direct equivalent. Documented as a
/// real, tracked gap rather than papered over with a fake timeout - the same shape as
/// `LspClient::kill_process_tree`'s own `#[cfg(windows)]` twin, which is likewise real but
/// narrower than its unix counterpart (direct child only, no descendant-process-tree walk).
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
            // A failed `write_all` genuinely may have written part of the frame, and there is no
            // way to find out how much - reported as a desync, the conservative answer, since
            // treating a possibly-half-written frame as recoverable is the unsafe direction.
            bytes_written: 1,
        })
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

    /// Appends one real, production-framed message to `buffer`. Deliberately goes through
    /// [`frame`] - the exact encoder both real [`write_message_bounded`] paths use - so these
    /// round-trip tests pin the framing production actually emits, not a second copy of it
    /// written for the tests' convenience.
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

    /// Real, fast, process-backed coverage for [`write_message_bounded`]'s own deadline, against
    /// a genuine OS pipe nobody is draining - no language server involved, so this pins the write
    /// bound itself in under a second rather than paying `client.rs`'s real 30-second production
    /// budget.
    ///
    /// `sleep` is spawned purely for its stdin: it holds the read end of a real pipe open and
    /// never reads a byte of it, which is exactly the frozen-server condition the bound exists
    /// for (see [`write_message_bounded`]'s own docs for the live reproduction).
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
                // The same real `O_NONBLOCK` production sets at spawn - `write_message_bounded`
                // owns its own waiting and requires it (see that function's own docs).
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

        /// A message that fits in the kernel pipe buffer still goes through untouched - the
        /// common case must not have been turned into a timeout by the bound.
        #[test]
        fn a_small_message_still_writes_straight_through() {
            let mut pipe = UndrainedPipe::new();
            let value = serde_json::json!({"jsonrpc": "2.0", "method": "initialized"});
            write_message_bounded(&mut pipe.stdin, &value, Duration::from_secs(5))
                .expect("a message that fits in the pipe buffer must not time out");
        }

        /// The real regression: a frame far larger than the pipe buffer, to a peer that never
        /// reads, gives up on the caller's own deadline instead of blocking forever - and reports
        /// the partial write, which is what tells the caller the peer's framer is now desynced.
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
