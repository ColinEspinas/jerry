//! A deliberately simplified terminal output buffer.
//!
//! ## Scope decision
//!
//! `pty-core`'s own docs record that composing a PTY spawn primitive with
//! `alacritty_terminal`'s `Term` grid parser is a real, larger undertaking (Zed's own
//! `terminal` crate drives `alacritty_terminal::tty::Pty` and
//! `alacritty_terminal::event_loop::EventLoop` together end to end; that composition isn't
//! separable the way a standalone "spawn primitive" crate needs to be). Pulling
//! `alacritty_terminal` into this step as well - a `git` dependency pinned in *vendor/zed's
//! own* workspace, not this repo's - would mean this crate takes on an unpinned,
//! independently-versioned dependency edge outside this repo's lockfile discipline, for a
//! step whose brief explicitly allows a simpler alternative.
//!
//! So `app` renders a **scrolling plain-text buffer**, not a full ANSI/SGR-aware grid:
//! - Real bytes come from a genuinely running child process via `pty-core` - nothing here
//!   is simulated or canned.
//! - CSI (`ESC [ ... final-byte`) and OSC (`ESC ] ... BEL-or-ST`) escape sequences are
//!   recognized and *dropped* rather than interpreted, so cursor movement, SGR color codes,
//!   and terminal-title-setting sequences don't show up as visible garbage in the output.
//!   No color, cursor-repositioning, or alternate-screen-buffer fidelity is attempted.
//! - `\r` is handled as a **deferred** "move to column 0": seeing `\r` alone doesn't erase
//!   anything yet, it just records that the *next* character should start overwriting from
//!   column 0. If that next character is `\n` (the overwhelmingly common case: a pty with
//!   `ONLCR` on, which is the default, rewrites every `\n` a child writes as `\r\n`), the
//!   pending `\r` is simply absorbed into the normal newline/commit and nothing is erased -
//!   this is what makes ordinary line-based output actually show up as committed lines
//!   instead of being wiped by the `\r` half of `\r\n` a moment before the `\n` half
//!   commits it. If the next character is anything else, *then* the current line is
//!   cleared before that character is written, approximating (not exactly matching - a
//!   real `\r` doesn't erase on its own) the single most common real-world use of a bare
//!   `\r`: progress bars and spinners that repeatedly redraw one line.
//!
//! This means prompts, `ls` output, `cat`, command output, etc. all render correctly as
//! text; a full-screen curses-style program (`vim`, `htop`) would render as a garbled
//! stream of its plain-text draw commands, since there's no cursor-addressed grid to
//! target. That trade-off is accepted for this step.

use std::collections::VecDeque;

/// How many completed lines [`TerminalBuffer`] retains. Older lines are dropped once this
/// is exceeded, bounding memory for a long-lived shell session.
const MAX_LINES: usize = 4000;

/// Hard cap, in bytes, on how large `current_line` (the not-yet-newline-terminated tail)
/// is allowed to grow before it's force-committed as a completed line, as if a newline had
/// occurred. Without this, a child process that emits a very long run of bytes with no
/// `\n` (or no `\r\n`) would grow `current_line` without bound - on the GPUI foreground
/// thread, re-laid-out roughly every 33ms while output keeps streaming (see
/// `crate::terminal_pane`). This isn't real terminal column-width wrapping (this buffer
/// has no concept of the pty's actual column count - see the module docs), just a defensive
/// bound.
const MAX_CURRENT_LINE_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Text,
    /// Just saw `ESC`; the next byte decides what kind of sequence this is.
    Escape,
    /// Inside a CSI (`ESC [`) sequence, consuming parameter/intermediate bytes until a
    /// final byte in `0x40..=0x7E`.
    Csi,
    /// Inside an OSC (`ESC ]`) sequence, consuming bytes until a `BEL` (`0x07`) or an
    /// `ESC \` (String Terminator).
    Osc,
    /// Inside an OSC sequence, just saw `ESC`; a following `\` ends the OSC (ST), anything
    /// else is treated as still inside the OSC body.
    OscEscape,
}

/// A scrolling buffer of decoded terminal output. See the module docs for the fidelity
/// trade-off. GPUI-independent and unit tested on its own.
#[derive(Debug)]
pub struct TerminalBuffer {
    lines: VecDeque<String>,
    current_line: String,
    state: ScanState,
    /// Text bytes (post-escape-stripping) not yet decoded to UTF-8 and flushed into
    /// `current_line`, because they might be the incomplete tail of a multi-byte UTF-8
    /// sequence split across two `append_bytes` calls (chunk boundaries from pty-core's
    /// reader thread do not respect UTF-8 or escape-sequence boundaries).
    pending_text: Vec<u8>,
    /// `true` immediately after a `\r` has been seen but the character that follows it
    /// (which decides whether the `\r` was really just the first half of a `\r\n`, or a
    /// bare `\r` that should overwrite the current line) hasn't arrived yet. See the
    /// module docs' `\r` section.
    pending_cr: bool,
    /// `true` once the backing process has exited (the output channel disconnected); the
    /// buffer stops changing after this, but is not cleared, so the last output stays
    /// visible.
    pub ended: bool,
}

impl TerminalBuffer {
    pub fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            current_line: String::new(),
            state: ScanState::Text,
            pending_text: Vec::new(),
            pending_cr: false,
            ended: false,
        }
    }

    /// Feeds a chunk of raw bytes read from the pty into the buffer, stripping escape
    /// sequences and appending decoded text. Safe to call repeatedly with arbitrarily
    /// split chunks.
    pub fn append_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                ScanState::Text => {
                    if byte == 0x1b {
                        self.flush_pending_text();
                        self.state = ScanState::Escape;
                    } else {
                        self.pending_text.push(byte);
                    }
                }
                ScanState::Escape => {
                    self.state = match byte {
                        b'[' => ScanState::Csi,
                        b']' => ScanState::Osc,
                        // Any other single/two-byte escape (character set selection, RIS,
                        // etc.): consumed as "not text" and then back to normal scanning.
                        _ => ScanState::Text,
                    };
                }
                ScanState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.state = ScanState::Text;
                    }
                }
                ScanState::Osc => {
                    self.state = match byte {
                        0x07 => ScanState::Text,
                        0x1b => ScanState::OscEscape,
                        _ => ScanState::Osc,
                    };
                }
                ScanState::OscEscape => {
                    self.state = if byte == b'\\' {
                        ScanState::Text
                    } else {
                        ScanState::Osc
                    };
                }
            }
        }
        self.flush_pending_text();
    }

    /// Marks the buffer as belonging to an exited process; called once the pty output
    /// channel disconnects.
    pub fn mark_ended(&mut self) {
        self.ended = true;
    }

    /// Completed lines followed by the current (not yet newline-terminated) line, oldest
    /// first. Used by the renderer; also convenient for tests.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.current_line.as_str()))
    }

    /// Decodes as much of `pending_text` as is valid UTF-8 and appends it to
    /// `current_line`/`lines`, retaining only a genuinely incomplete trailing multi-byte
    /// sequence (if any) in `pending_text` for the next call. A byte that is definitively
    /// invalid UTF-8 (not just "incomplete so far") is replaced with U+FFFD and skipped, so
    /// one bad byte can never stall the buffer forever.
    fn flush_pending_text(&mut self) {
        loop {
            if self.pending_text.is_empty() {
                return;
            }
            // Decoded into an owned `String` (rather than pushed straight from a borrow of
            // `self.pending_text`) so `self.push_str` below is free to mutably borrow
            // `self` while `self.pending_text` is separately drained/cleared - `from_utf8`
            // borrows `self.pending_text` immutably, which would otherwise conflict with
            // `push_str`'s `&mut self`.
            match std::str::from_utf8(&self.pending_text) {
                Ok(text) => {
                    let text = text.to_string();
                    self.pending_text.clear();
                    self.push_str(&text);
                    return;
                }
                Err(err) => {
                    let valid_up_to = err.valid_up_to();
                    let valid_text = (valid_up_to > 0)
                        .then(|| std::str::from_utf8(&self.pending_text[..valid_up_to]).ok())
                        .flatten()
                        .map(str::to_string);

                    match err.error_len() {
                        // Incomplete sequence at the end of the buffer: wait for more bytes.
                        None => {
                            self.pending_text.drain(..valid_up_to);
                            if let Some(text) = valid_text {
                                self.push_str(&text);
                            }
                            return;
                        }
                        // A genuinely invalid byte sequence: drop it and keep scanning the
                        // rest immediately, so a single bad byte doesn't wedge the buffer.
                        Some(bad_len) => {
                            self.pending_text.drain(..valid_up_to + bad_len);
                            if let Some(text) = valid_text {
                                self.push_str(&text);
                            }
                            self.push_str("\u{fffd}");
                        }
                    }
                }
            }
        }
    }

    fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            if self.pending_cr {
                self.pending_cr = false;
                if ch == '\n' {
                    // The `\r` was just the first half of a `\r\n` (ONLCR, the default):
                    // commit the line normally, nothing was overwritten.
                    self.newline();
                    continue;
                }
                // A bare `\r` not followed by `\n`: *now* it's a real column-0 overwrite.
                self.current_line.clear();
            }

            match ch {
                '\n' => self.newline(),
                '\r' => self.pending_cr = true,
                '\u{8}' => {
                    self.current_line.pop();
                }
                '\t' => self.current_line.push_str("    "),
                c if c.is_control() => {}
                c => {
                    self.current_line.push(c);
                    if self.current_line.len() >= MAX_CURRENT_LINE_BYTES {
                        self.newline();
                    }
                }
            }
        }
    }

    fn newline(&mut self) {
        let line = std::mem::take(&mut self.current_line);
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
    }
}

impl Default for TerminalBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_newlines_render_as_lines() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"hello\nworld\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["hello", "world", ""]);
    }

    #[test]
    fn csi_sequences_are_stripped_not_shown() {
        let mut buffer = TerminalBuffer::new();
        // A colored "hello" via SGR codes: ESC[31m hello ESC[0m
        buffer.append_bytes(b"\x1b[31mhello\x1b[0m\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["hello", ""]);
    }

    #[test]
    fn osc_title_sequence_is_stripped() {
        let mut buffer = TerminalBuffer::new();
        // OSC 0 sets the window title, terminated by BEL.
        buffer.append_bytes(b"\x1b]0;my title\x07visible\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["visible", ""]);
    }

    #[test]
    fn osc_sequence_terminated_by_st_is_stripped() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"\x1b]0;title\x1b\\visible\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["visible", ""]);
    }

    #[test]
    fn escape_sequence_split_across_chunks_is_still_stripped() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"before\x1b[");
        buffer.append_bytes(b"31");
        buffer.append_bytes(b"mafter\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["beforeafter", ""]);
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_reconstructs_correctly() {
        let mut buffer = TerminalBuffer::new();
        // "héllo\n" - the 'é' (0xC3 0xA9 in UTF-8) is split across two chunks.
        let full = "héllo\n".as_bytes().to_vec();
        let (first, second) = full.split_at(3); // splits inside the 2-byte 'é'
        buffer.append_bytes(first);
        buffer.append_bytes(second);
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["héllo", ""]);
    }

    #[test]
    fn carriage_return_restarts_the_current_line() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"progress: 10%\rprogress: 99%\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["progress: 99%", ""]);
    }

    /// Regression test for a real end-to-end bug: a pty with `ONLCR` on (the default)
    /// rewrites every `\n` a child writes as `\r\n`. Treating `\r` as an *eager* clear (the
    /// original implementation) wiped the line's text a moment before the `\n` half
    /// committed it, so real multi-line process output rendered as nothing but blank
    /// lines - the only reason a bare shell prompt ever appeared to work is that a prompt
    /// has no trailing newline and survives as `current_line`. `\r\n` must commit the line
    /// intact, not erase it.
    #[test]
    fn crlf_from_onlcr_commits_the_line_instead_of_erasing_it() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"HELLO-AUDIT\r\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["HELLO-AUDIT", ""]);
    }

    /// The same regression as `crlf_from_onlcr_commits_the_line_instead_of_erasing_it`, but
    /// end-to-end through a genuinely spawned process on a real pty (via `pty_core::spawn`)
    /// instead of hand-written bytes - this is the exact shape of test that originally
    /// caught the bug (a hand-fed `\r\n` literal is easy to get right in a test and still
    /// miss that a *real* pty's line discipline is what actually produces it). `printf`'s
    /// literal `\n` gets rewritten to `\r\n` on the way out by the kernel's tty driver
    /// (`ONLCR`, on by default for a freshly opened pty), so this only passes if the
    /// CR-then-LF handling in `TerminalBuffer` is correct against real pty output, not just
    /// against a literal the test author already knows is a `\r\n` pair.
    #[test]
    fn end_to_end_real_pty_output_renders_as_completed_lines_not_blank() {
        let session = pty_core::spawn(
            pty_core::SpawnOptions::new("printf").arg("HELLO-AUDIT\nSECOND-LINE\n"),
        )
        .expect("spawning `printf` should succeed");

        let mut buffer = TerminalBuffer::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let Ok(chunk) = session
            .output()
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            buffer.append_bytes(&chunk);
        }

        let lines: Vec<&str> = buffer.lines().collect();
        assert!(
            lines.contains(&"HELLO-AUDIT"),
            "expected a real completed line \"HELLO-AUDIT\", got {lines:?} - if this is all \
             blank lines, `\\r` handling is eagerly clearing the line before `\\n` commits it"
        );
        assert!(
            lines.contains(&"SECOND-LINE"),
            "expected a real completed line \"SECOND-LINE\", got {lines:?}"
        );
    }

    #[test]
    fn crlf_split_across_chunks_still_commits_the_line_intact() {
        let mut buffer = TerminalBuffer::new();
        // The `\r` and `\n` of a single `\r\n` arriving in two separate `append_bytes`
        // calls (a real possibility: pty-core's reader thread forwards whatever a single
        // `read(2)` returned, without regard for character/sequence boundaries).
        buffer.append_bytes(b"HELLO-AUDIT\r");
        buffer.append_bytes(b"\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["HELLO-AUDIT", ""]);
    }

    #[test]
    fn multiple_crlf_lines_all_commit_correctly() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"line one\r\nline two\r\nline three\r\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["line one", "line two", "line three", ""]);
    }

    #[test]
    fn bare_carriage_return_still_overwrites_when_not_followed_by_newline() {
        // A real progress-bar-style redraw: `\r` followed by more text and *no* `\n` at
        // all yet. This must still behave like the pre-fix "overwrite" approximation.
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"10%\r99%");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["99%"]);
    }

    #[test]
    fn current_line_is_force_committed_past_the_byte_cap() {
        let mut buffer = TerminalBuffer::new();
        // One long run with no newline at all: without a cap this would grow
        // `current_line` unboundedly.
        let long_run = "x".repeat(MAX_CURRENT_LINE_BYTES * 3);
        buffer.append_bytes(long_run.as_bytes());
        let lines: Vec<&str> = buffer.lines().collect();
        // Force-committed into multiple lines, each within the byte cap, plus a
        // (possibly empty) trailing `current_line`.
        assert!(
            lines.len() >= 3,
            "expected the long run to be force-committed into multiple lines, got {} line(s)",
            lines.len()
        );
        for line in &lines {
            assert!(
                line.len() <= MAX_CURRENT_LINE_BYTES,
                "line exceeded the byte cap: {} bytes",
                line.len()
            );
        }
        assert_eq!(lines.join(""), long_run);
    }

    #[test]
    fn backspace_removes_the_previous_character() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"hellp\x08o\n");
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["hello", ""]);
    }

    #[test]
    fn old_lines_are_dropped_once_the_cap_is_exceeded() {
        let mut buffer = TerminalBuffer::new();
        for i in 0..(MAX_LINES + 10) {
            buffer.append_bytes(format!("line{i}\n").as_bytes());
        }
        let lines: Vec<&str> = buffer.lines().collect();
        // MAX_LINES completed lines retained, plus the trailing empty current line.
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!(lines[0], "line10");
    }

    #[test]
    fn mark_ended_sets_the_flag_without_clearing_output() {
        let mut buffer = TerminalBuffer::new();
        buffer.append_bytes(b"last output\n");
        buffer.mark_ended();
        assert!(buffer.ended);
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["last output", ""]);
    }

    #[test]
    fn invalid_utf8_byte_is_replaced_and_does_not_stall_the_buffer() {
        let mut buffer = TerminalBuffer::new();
        let mut bytes = b"before".to_vec();
        bytes.push(0xff); // not valid UTF-8 anywhere
        bytes.extend_from_slice(b"after\n");
        buffer.append_bytes(&bytes);
        let lines: Vec<&str> = buffer.lines().collect();
        assert_eq!(lines, vec!["before\u{fffd}after", ""]);
    }
}
