//! Real, on-disk transcripts for finished runs, **keyed by run id** (GitHub issue #227).
//!
//! The design's rule: "Archived runs carry their own transcript, keyed by run id, never the live
//! agent's buffer." This module is that
//! key-value store, and the key is the same [`crate::review::state::baseline_key`] every other
//! per-run persisted thing in this app is filed under, so a transcript can never end up attached
//! to a different run than the record beside it.
//!
//! ## Why a directory of files rather than more fields in `agent-status.toml`
//!
//! [`crate::hooks::store`]'s file is rewritten and `fsync`ed under a process-wide lock on **every
//! status change**, for up to 500 records. A transcript is kilobytes of text; putting them in that
//! file would multiply the cost of every routine `Run` -> `Idle` transition by the whole
//! history's worth of terminal output. Here, a transcript is written exactly once - when its run
//! ends - and read exactly once - when its tab is opened.
//!
//! ## What is stored
//!
//! Plain UTF-8 text, one line per line, exactly as the run's own pane held it
//! (`crate::terminal::pane::TerminalPane::retained_text_lines`). Deliberately no colours: the tab
//! renders a recording at 70% opacity in one body tone (see
//! `crate::run_history::model::LineTone`), and storing per-cell attributes would be storing
//! something nothing reads.
//!
//! Bounded twice, because a terminal's retained scrollback is bounded only by
//! `alacritty_terminal`'s own 10 000-line history: at most [`MAX_TRANSCRIPT_LINES`] lines and
//! [`MAX_TRANSCRIPT_BYTES`] bytes, keeping the **end** of the run in both cases - the tail is
//! where a run says what it did.
//!
//! ## Failure is never fatal
//!
//! Every function here degrades to "no transcript" rather than to an error the user has to deal
//! with. A run whose transcript could not be written, or could not be read back, gets the
//! synthesised body `crate::run_history::model::transcript_body` produces from its own record -
//! which is a real, honest surface, not a fallback that pretends.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The directory transcripts live in, a sibling of the real `settings.toml` and of
/// `agent-status.toml` itself.
pub const TRANSCRIPT_DIR_NAME: &str = "run-transcripts";

/// The most lines one stored transcript keeps. Generous enough to hold a long run's whole
/// conversation, small enough that a full history directory stays a few megabytes.
pub const MAX_TRANSCRIPT_LINES: usize = 400;

/// The most bytes one stored transcript keeps, applied after [`MAX_TRANSCRIPT_LINES`] - a
/// belt-and-braces bound for the pathological case of a run that printed a minified bundle.
pub const MAX_TRANSCRIPT_BYTES: usize = 256 * 1024;

/// The transcript directory for a given real settings-file path - identical reasoning to
/// [`crate::hooks::store::agent_status_path_for`]: a test supplying a temp-dir settings path gets
/// real, isolated persistence there, and a `None` settings path gets none.
pub fn transcript_dir_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(TRANSCRIPT_DIR_NAME),
        None => PathBuf::from(TRANSCRIPT_DIR_NAME),
    }
}

/// The file one run's transcript lives in.
///
/// The run key is hashed rather than used directly, because it is a real
/// [`crate::review::state::baseline_key`] - a worktree path, a kind and a timestamp joined by
/// `|`. That contains `/`, `\` and (on the `bytes:` arm) arbitrary escaped bytes, none of which
/// are legal in a filename on every platform, and it can be far longer than the 255-byte component
/// limit of most filesystems. SHA-256 is already a direct dependency of this crate and of
/// `wt-core` (whose `review::baseline_ref_name` hashes the very same keys, for the very same
/// reason), so this is the established answer to this exact problem here rather than a new one.
pub fn transcript_path(dir: &Path, run_key: &str) -> PathBuf {
    let digest = Sha256::digest(run_key.as_bytes());
    dir.join(format!("{digest:x}.txt"))
}

/// Applies both bounds, keeping the **end** of the run.
fn bounded(lines: &[String]) -> Vec<String> {
    let start = lines.len().saturating_sub(MAX_TRANSCRIPT_LINES);
    let mut kept: Vec<String> = lines[start..].to_vec();
    // `+ 1` per line for the newline that will join them, so the bound really is a bound on what
    // gets written rather than on the sum of the pieces.
    let mut bytes: usize = kept.iter().map(|line| line.len() + 1).sum();
    while bytes > MAX_TRANSCRIPT_BYTES && !kept.is_empty() {
        let dropped = kept.remove(0);
        bytes -= dropped.len() + 1;
    }
    kept
}

/// Writes one run's transcript. A run with nothing to store writes no file at all, so "this run
/// has no stored transcript" and "this run stored an empty one" are the same state on disk rather
/// than two that read differently.
///
/// Not atomic (no temp-and-rename) unlike this app's other persisted state, and deliberately: a
/// transcript is written exactly once, by exactly one instance, for a key nothing else will ever
/// write again, so there is no concurrent writer to lose a race with. A torn write from a crash
/// mid-`write_all` leaves a truncated transcript, which reads back as a shorter transcript - not
/// as corruption, because there is no structure to corrupt.
pub fn save(dir: &Path, run_key: &str, lines: &[String]) -> io::Result<()> {
    let lines = bounded(lines);
    if lines.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(transcript_path(dir, run_key), lines.join("\n"))
}

/// Reads one run's transcript back. `None` for a run that has none, and for any read failure -
/// see the module docs on why that is never surfaced as an error.
pub fn load(dir: &Path, run_key: &str) -> Option<Vec<String>> {
    let contents = std::fs::read_to_string(transcript_path(dir, run_key)).ok()?;
    let lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    (!lines.is_empty()).then_some(lines)
}

/// Deletes every transcript whose run is no longer in `live_keys`.
///
/// [`crate::hooks::store::AgentStatusState::prune_to_most_recent`] caps the record file at 500
/// runs, and a transcript whose record has been pruned is unreachable: nothing can open a tab for
/// a run that is not in the history list. Without this, the directory would be the one piece of
/// this feature that grows forever.
///
/// Errors are counted, not propagated: a file that will not delete is a wasted few kilobytes, and
/// failing a run's *close* over it would be absurd. Returns how many files were really removed.
pub fn prune(dir: &Path, live_keys: &std::collections::BTreeSet<String>) -> usize {
    let live_names: std::collections::HashSet<PathBuf> = live_keys
        .iter()
        .map(|key| transcript_path(dir, key))
        .collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "txt")
            && !live_names.contains(&path)
            && std::fs::remove_file(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_transcript_directory_sits_next_to_the_real_settings_file() {
        assert_eq!(
            transcript_dir_for(Path::new("/home/someone/.config/jerry/settings.toml")),
            PathBuf::from("/home/someone/.config/jerry/run-transcripts")
        );
    }

    #[test]
    fn a_transcript_really_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let key = "utf8:/repo/wt-a|Claude|1700000000";
        let lines = vec![
            "\u{276f} claude".to_string(),
            String::new(),
            "\u{25cf} done".to_string(),
        ];
        save(dir.path(), key, &lines).expect("must save");
        assert_eq!(load(dir.path(), key), Some(lines));
    }

    /// The key is a real `baseline_key` - a path with separators in it. It must never reach the
    /// filesystem as a filename.
    #[test]
    fn a_run_key_full_of_path_separators_still_produces_one_legal_filename() {
        let dir = Path::new("/tmp/jerry-transcripts");
        let path = transcript_path(dir, "utf8:/deep/nested/worktree/path|Claude|1700000000");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a real filename");
        assert!(name.ends_with(".txt"));
        assert_eq!(name.len(), 64 + 4, "a hex sha-256 plus the extension");
        assert!(!name.contains('/') && !name.contains('|'));
        assert_eq!(path.parent(), Some(dir));
    }

    #[test]
    fn two_different_runs_never_share_a_transcript_file() {
        let dir = Path::new("/tmp/jerry-transcripts");
        assert_ne!(
            transcript_path(dir, "utf8:/repo/wt|Claude|1"),
            transcript_path(dir, "utf8:/repo/wt|Claude|2")
        );
    }

    #[test]
    fn a_run_with_nothing_to_store_writes_no_file_at_all() {
        let dir = tempfile::tempdir().expect("temp dir");
        save(dir.path(), "k", &[]).expect("must not fail");
        assert_eq!(load(dir.path(), "k"), None);
        assert!(
            !transcript_path(dir.path(), "k").exists(),
            "an empty transcript must not leave a file behind"
        );
    }

    #[test]
    fn the_line_cap_keeps_the_end_of_the_run() {
        let lines: Vec<String> = (0..(MAX_TRANSCRIPT_LINES + 50))
            .map(|index| format!("line {index}"))
            .collect();
        let kept = bounded(&lines);
        assert_eq!(kept.len(), MAX_TRANSCRIPT_LINES);
        assert_eq!(
            kept.last().map(String::as_str),
            Some(format!("line {}", MAX_TRANSCRIPT_LINES + 49).as_str()),
            "the tail is where a run says what it did"
        );
    }

    #[test]
    fn the_byte_cap_is_a_real_bound_on_what_is_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lines: Vec<String> = (0..200).map(|_| "x".repeat(4000)).collect();
        save(dir.path(), "big", &lines).expect("must save");
        let written = std::fs::metadata(transcript_path(dir.path(), "big"))
            .expect("the file must exist")
            .len() as usize;
        assert!(
            written <= MAX_TRANSCRIPT_BYTES,
            "{written} bytes exceeds the cap"
        );
        let kept = load(dir.path(), "big").expect("something must survive");
        assert!(!kept.is_empty());
    }

    #[test]
    fn pruning_removes_exactly_the_transcripts_whose_runs_are_gone() {
        let dir = tempfile::tempdir().expect("temp dir");
        for key in ["kept-a", "kept-b", "dropped"] {
            save(dir.path(), key, &[format!("transcript for {key}")]).expect("must save");
        }
        let live: BTreeSet<String> = ["kept-a".to_string(), "kept-b".to_string()]
            .into_iter()
            .collect();

        assert_eq!(prune(dir.path(), &live), 1);
        assert!(load(dir.path(), "kept-a").is_some());
        assert!(load(dir.path(), "kept-b").is_some());
        assert_eq!(load(dir.path(), "dropped"), None);
    }

    #[test]
    fn pruning_a_directory_that_does_not_exist_is_a_no_op() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            prune(&dir.path().join("never-created"), &BTreeSet::new()),
            0
        );
    }
}
