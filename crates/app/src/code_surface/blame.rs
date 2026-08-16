//! Pure logic for Surface C's inline git blame (GitHub issue #29, part of the umbrella issue
//! #14 "Editor polish"): turning a [`wt_core::blame::BlameLine`] into the short label shown
//! dimmed at the end of the current line, and the longer text shown in its hover tooltip.
//! Deliberately `gpui`-window-free (only plain `String`s are produced here, mirroring
//! `crate::lsp::hover`'s own split - `crate::code_surface::editing`/`crate::code_surface::
//! file_view` wrap them in a `SharedString`/real tooltip element at render time).

use crate::root::plural;
use wt_core::blame::BlameLine;

/// The all-zero sha `git blame` uses for a line whose content hasn't been committed yet -
/// re-derived here rather than imported, since [`wt_core::blame::BlameLine::is_uncommitted`]
/// already tells callers this without needing to compare shas themselves; this constant exists
/// only for this module's own tests.
#[cfg(test)]
const UNCOMMITTED_SHA: &str = "0000000000000000000000000000000000000000";

/// The short, dimmed end-of-line label plus the longer hover text for one blamed line - see
/// [`inline_blame_label`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineBlameLabel {
    /// The short text shown dimmed at the end of the current line, e.g. `"Ada Lovelace, 3 days
    /// ago \u{2022} fix the analytical engine"`, or `"You, uncommitted changes"` for a line
    /// that hasn't been committed yet (GitHub issue #29's own required wording).
    pub inline_text: String,
    /// The longer text shown in the hover tooltip: the full sha plus either the real, full
    /// commit message (once `crate::code_surface::blame_view::AdeApp::ensure_blame_commit_
    /// message` has fetched it) or just the one-line summary as a fallback while that fetch is
    /// still in flight. `None` for an uncommitted line - there is no real commit to name a sha
    /// or message for, so the tooltip says so instead (see the caller in `crate::code_surface::
    /// blame_view`) rather than showing a fabricated all-zero sha.
    pub tooltip_text: Option<String>,
}

/// Builds an [`InlineBlameLabel`] for `line`, evaluated as of `now_unix` (seconds since the
/// Unix epoch). `full_message`, when given, is the real commit message body already fetched for
/// `line.sha` (`wt_core::blame::commit_message`); without it, the tooltip falls back to the
/// blame summary alone rather than blocking the render on a not-yet-arrived background fetch.
pub fn inline_blame_label(
    line: &BlameLine,
    now_unix: i64,
    full_message: Option<&str>,
) -> InlineBlameLabel {
    if line.is_uncommitted {
        return InlineBlameLabel {
            inline_text: "You, uncommitted changes".to_string(),
            tooltip_text: None,
        };
    }

    let relative = format_relative_date(line.author_time_unix, now_unix);
    let inline_text = format!("{}, {relative} \u{2022} {}", line.author, line.summary);
    let short_sha = short_sha(&line.sha);
    let body = full_message.unwrap_or(line.summary.as_str());
    let tooltip_text = Some(format!("{short_sha} {}\n\n{body}", line.sha));

    InlineBlameLabel {
        inline_text,
        tooltip_text,
    }
}

/// The first 7 characters of `sha` - `git`'s own conventional short-sha length, used only for
/// the tooltip's leading, human-scannable label (the full `sha` follows it in the same string,
/// so nothing is actually lost by shortening this one copy).
fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(7)
        .map(|(index, _)| index)
        .unwrap_or(sha.len());
    &sha[..end]
}

/// One bucket of [`format_relative_date`]'s output, expressed as a whole-unit threshold in
/// seconds and the singular/plural unit label - checked in order, largest first, so e.g. a gap
/// of exactly 3600 seconds reads as `"1 hour ago"` rather than `"60 minutes ago"`.
const RELATIVE_DATE_BUCKETS: &[(i64, &str)] = &[
    (365 * 24 * 3600, "year"),
    (30 * 24 * 3600, "month"),
    (7 * 24 * 3600, "week"),
    (24 * 3600, "day"),
    (3600, "hour"),
    (60, "minute"),
];

/// A human, git-style relative date (`"3 days ago"`, `"just now"`) for a commit whose
/// author-time is `then_unix`, evaluated as of `now_unix` (both seconds since the Unix epoch).
/// No calendar/timezone-aware "today"/"yesterday" special-casing - a plain elapsed-seconds
/// bucketing, matching what `git log --relative-date`/GitHub's own commit list show, and simple
/// enough to need no new date/time dependency (this workspace has none; see `Cargo.toml`'s own
/// "don't add a dependency that duplicates something already vendored" convention).
pub fn format_relative_date(then_unix: i64, now_unix: i64) -> String {
    let elapsed = now_unix.saturating_sub(then_unix);
    if elapsed < 60 {
        return "just now".to_string();
    }
    for (threshold, unit) in RELATIVE_DATE_BUCKETS {
        if elapsed >= *threshold {
            // `elapsed >= *threshold > 0`, so this is a genuine, non-negative count and the
            // `as usize` cast cannot wrap.
            let count = (elapsed / threshold) as usize;
            return format!("{} ago", plural::count(count, unit, None));
        }
    }
    // Unreachable in practice (60s is `RELATIVE_DATE_BUCKETS`'s own smallest threshold, and
    // `elapsed < 60` already returned above), but a real fallback rather than a panic if the
    // table above is ever edited down to nothing.
    "just now".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed_line(author_time_unix: i64) -> BlameLine {
        BlameLine {
            sha: "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            author: "Ada Lovelace".to_string(),
            author_time_unix,
            summary: "fix the analytical engine".to_string(),
            is_uncommitted: false,
        }
    }

    fn uncommitted_line() -> BlameLine {
        BlameLine {
            sha: UNCOMMITTED_SHA.to_string(),
            author: "Not Committed Yet".to_string(),
            author_time_unix: 0,
            summary: String::new(),
            is_uncommitted: true,
        }
    }

    #[test]
    fn format_relative_date_buckets_correctly() {
        let now = 1_000_000_000i64;
        assert_eq!(format_relative_date(now, now), "just now");
        assert_eq!(format_relative_date(now - 30, now), "just now");
        assert_eq!(format_relative_date(now - 90, now), "1 minute ago");
        assert_eq!(format_relative_date(now - 5 * 60, now), "5 minutes ago");
        assert_eq!(format_relative_date(now - 3600, now), "1 hour ago");
        assert_eq!(format_relative_date(now - 3 * 3600, now), "3 hours ago");
        assert_eq!(format_relative_date(now - 24 * 3600, now), "1 day ago");
        assert_eq!(format_relative_date(now - 3 * 24 * 3600, now), "3 days ago");
        assert_eq!(format_relative_date(now - 7 * 24 * 3600, now), "1 week ago");
        assert_eq!(
            format_relative_date(now - 30 * 24 * 3600, now),
            "1 month ago"
        );
        assert_eq!(
            format_relative_date(now - 365 * 24 * 3600, now),
            "1 year ago"
        );
        assert_eq!(
            format_relative_date(now - 2 * 365 * 24 * 3600, now),
            "2 years ago"
        );
    }

    #[test]
    fn format_relative_date_clamps_a_future_timestamp_to_just_now() {
        let now = 1_000_000_000i64;
        assert_eq!(format_relative_date(now + 10_000, now), "just now");
    }

    #[test]
    fn inline_blame_label_for_a_committed_line_shows_author_date_and_summary() {
        let now = 1_000_000_000i64;
        let line = committed_line(now - 3 * 24 * 3600);
        let label = inline_blame_label(&line, now, None);
        assert_eq!(
            label.inline_text,
            "Ada Lovelace, 3 days ago \u{2022} fix the analytical engine"
        );
        let tooltip = label.tooltip_text.expect("a committed line has a tooltip");
        assert!(tooltip.starts_with("abcdef1"));
        assert!(tooltip.contains(&line.sha));
        assert!(tooltip.contains("fix the analytical engine"));
    }

    #[test]
    fn inline_blame_label_prefers_the_real_full_message_when_given() {
        let now = 1_000_000_000i64;
        let line = committed_line(now - 3 * 24 * 3600);
        let label = inline_blame_label(
            &line,
            now,
            Some("fix the analytical engine\n\nA real, longer body paragraph."),
        );
        let tooltip = label.tooltip_text.expect("a committed line has a tooltip");
        assert!(tooltip.contains("A real, longer body paragraph."));
    }

    #[test]
    fn inline_blame_label_for_an_uncommitted_line_uses_the_required_wording() {
        let now = 1_000_000_000i64;
        let label = inline_blame_label(&uncommitted_line(), now, None);
        assert_eq!(label.inline_text, "You, uncommitted changes");
        assert_eq!(
            label.tooltip_text, None,
            "there is no real commit to show a sha/message for"
        );
    }

    #[test]
    fn short_sha_takes_the_first_seven_characters() {
        assert_eq!(short_sha("abcdef1234567890"), "abcdef1");
    }

    #[test]
    fn short_sha_handles_a_shorter_input_without_panicking() {
        assert_eq!(short_sha("abc"), "abc");
    }
}
