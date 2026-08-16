//! Off-thread, cached inline git blame (GitHub issue #29, part of the umbrella issue #14
//! "Editor polish"): the current line's author/relative-date/summary, rendered dimmed at the
//! end of the line, with a real hover tooltip carrying the full sha and commit message.

use std::time::{Duration, SystemTime};

use super::*;
use crate::root::widgets::text_tooltip;

/// How often [`AdeApp::maybe_refresh_blame`] rechecks whether the currently-visible file's
/// cached blame is still fresh, throttled the same way [`FILE_FRESHNESS_CHECK_INTERVAL`]
/// throttles the syntax-highlight cache's own check - see this module's own "Recompute
/// triggers" docs for why a periodic recheck, not a per-history-event hook, is what actually
/// drives "recomputes on save, on commit, and on branch/HEAD change". Coarser than the 500ms
/// syntax-highlight check: unlike that one (a cheap `std::fs::metadata` call), a stale-blame
/// recheck that finds real staleness spawns a real `git blame` child process, which is
/// meaningfully more expensive to run needlessly often.
pub(crate) const BLAME_FRESHNESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);

impl AdeApp {
    /// Ensures [`Self::blame_cache`] has a fresh entry for `absolute_path`, dispatching a real,
    /// off-thread [`Self::spawn_blame_load`] if it's missing, stale, or hasn't been checked
    /// within [`BLAME_FRESHNESS_CHECK_INTERVAL`]. A no-op whenever
    /// `Settings.blame.show_inline` is off (GitHub issue #29's own "user setting to hide it
    /// entirely" requirement) - no background work happens at all for a user who's turned this
    /// off, not just no rendering.
    pub(in crate::code_surface) fn maybe_refresh_blame(
        &mut self,
        absolute_path: &Path,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.blame.show_inline {
            return;
        }
        let now = Instant::now();
        let should_check = match &self.blame_last_freshness_check {
            Some((checked_path, checked_at)) => {
                checked_path != absolute_path
                    || now.duration_since(*checked_at) >= BLAME_FRESHNESS_CHECK_INTERVAL
            }
            None => true,
        };
        if !should_check {
            return;
        }
        self.blame_last_freshness_check = Some((absolute_path.to_path_buf(), now));

        if matches!(
            self.blame_state.get(absolute_path),
            Some(BlameLoadState::Loading)
        ) {
            // Already in flight for this exact path; the next completion will settle whatever
            // the real, current on-disk state is by the time it lands.
            return;
        }

        let metadata = std::fs::metadata(absolute_path).ok();
        let mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
        let len = metadata.map(|meta| meta.len()).unwrap_or(0);
        let stale = match self.blame_cache.get(absolute_path) {
            Some(entry) => entry.mtime != mtime || entry.len != len,
            None => true,
        };
        if stale {
            self.spawn_blame_load(absolute_path.to_path_buf(), cx);
        }
    }

    /// Forces an immediate blame recompute for `absolute_path`, bypassing
    /// [`BLAME_FRESHNESS_CHECK_INTERVAL`]'s throttle - called right after
    /// [`Self::spawn_file_save_loop`]'s own write succeeds (see this module's own "Recompute
    /// triggers" docs for why a save is worth an immediate refresh rather than waiting on the
    /// generic poll). A no-op when blame is turned off, same as [`Self::maybe_refresh_blame`].
    pub(in crate::code_surface) fn force_refresh_blame_for_save(
        &mut self,
        absolute_path: &Path,
        cx: &mut Context<Self>,
    ) {
        if !self.settings.blame.show_inline {
            return;
        }
        // Clearing the throttle record (rather than checking staleness directly here) makes
        // `spawn_blame_load` unconditional below still bypass a `Loading` in-flight guard the
        // *next* `maybe_refresh_blame` call would otherwise re-apply too early - a save always
        // means the file's real mtime just changed, so a fresh load is always warranted here.
        self.blame_last_freshness_check = None;
        self.spawn_blame_load(absolute_path.to_path_buf(), cx);
    }

    /// Dispatches one real, off-thread `wt_core::blame::blame_file` call for `absolute_path`,
    /// resolved against [`Self::file_tree_root`] - see this module's own top docs for the
    /// threading/caching contract and graceful-absence mapping.
    pub(in crate::code_surface) fn spawn_blame_load(
        &mut self,
        absolute_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.blame_state
            .insert(absolute_path.clone(), BlameLoadState::Loading);
        let Ok(relative_path) = absolute_path
            .strip_prefix(&self.file_tree_root)
            .map(|path| path.to_path_buf())
        else {
            // Not under the current worktree root at all (shouldn't happen for any path this is
            // ever called with, but a real, honest no-op rather than a panic if it somehow is).
            self.blame_state
                .insert(absolute_path, BlameLoadState::Unavailable);
            return;
        };
        let worktree_root = self.file_tree_root.clone();

        let task = cx.spawn({
            let absolute_path = absolute_path.clone();
            async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn({
                        let absolute_path = absolute_path.clone();
                        async move {
                            let metadata = std::fs::metadata(&absolute_path).ok();
                            let mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
                            let len = metadata.map(|meta| meta.len()).unwrap_or(0);
                            let outcome =
                                wt_core::blame::blame_file(&worktree_root, &relative_path);
                            (outcome, mtime, len)
                        }
                    })
                    .await;
                let (outcome, mtime, len) = result;
                let _ = this.update(cx, |this, cx| {
                    match outcome {
                        Ok(wt_core::blame::BlameOutcome::Blame(blame)) => {
                            this.blame_cache.insert(
                                absolute_path.clone(),
                                BlameCacheEntry { mtime, len, blame },
                            );
                            this.blame_state
                                .insert(absolute_path.clone(), BlameLoadState::Ready);
                        }
                        Ok(
                            wt_core::blame::BlameOutcome::NotARepo
                            | wt_core::blame::BlameOutcome::NotTracked,
                        ) => {
                            this.blame_cache.remove(&absolute_path);
                            this.blame_state
                                .insert(absolute_path.clone(), BlameLoadState::Unavailable);
                        }
                        Err(err) => {
                            log::debug!("blame unavailable for {}: {err}", absolute_path.display());
                            this.blame_cache.remove(&absolute_path);
                            this.blame_state.insert(
                                absolute_path.clone(),
                                BlameLoadState::Error(err.to_string()),
                            );
                        }
                    }
                    cx.notify();
                });
            }
        });
        self._blame_tasks.insert(absolute_path, task);
    }

    /// The current line's real, already-cached blame line for `absolute_path`, if inline blame
    /// is on, a cursor line is known, and a fresh [`BlameCacheEntry`] exists for it - a plain
    /// read, no background work triggered here (that's [`Self::maybe_refresh_blame`]'s job,
    /// called separately). `None` while the buffer has unsaved edits: the cached blame reflects
    /// **on-disk** content (see this module's own docs), and a dirty buffer's line numbering can
    /// have already diverged from it - the same honest suppression
    /// `AdeApp::file_view_changed_lines` (the git-gutter changed-line stripe) already applies for
    /// the identical reason; see `Self::render_file_view`'s own docs on `buffer_dirty`.
    pub(in crate::code_surface) fn current_line_blame(
        &self,
        absolute_path: &Path,
        cursor_line: Option<usize>,
        buffer_dirty: bool,
    ) -> Option<&wt_core::blame::BlameLine> {
        if !self.settings.blame.show_inline || buffer_dirty {
            return None;
        }
        let line_number = cursor_line?;
        let entry = self.blame_cache.get(absolute_path)?;
        entry.blame.lines.get(line_number.checked_sub(1)?)
    }

    /// Ensures [`Self::blame_commit_messages`] has an entry (or an in-flight load) for `sha` -
    /// the real, full commit-message body the hover tooltip shows once it arrives (see
    /// `crate::code_surface::blame::inline_blame_label`'s own `full_message` parameter). A
    /// no-op for a sha already cached/loading, and for the synthetic all-zero "uncommitted"
    /// sha (`wt_core::blame::commit_message` itself already refuses to look that up - checked
    /// again here so an uncommitted line's hover never even dispatches a background task for
    /// it).
    pub(in crate::code_surface) fn ensure_blame_commit_message(
        &mut self,
        sha: String,
        cx: &mut Context<Self>,
    ) {
        if sha.is_empty() || sha.bytes().all(|byte| byte == b'0') {
            return;
        }
        if self.blame_commit_messages.contains_key(&sha) {
            return;
        }
        self.blame_commit_messages
            .insert(sha.clone(), CommitMessageState::Loading);
        let worktree_root = self.file_tree_root.clone();
        let task = cx.spawn({
            let sha = sha.clone();
            async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn({
                        let sha = sha.clone();
                        async move { wt_core::blame::commit_message(&worktree_root, &sha) }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok(Some(message)) => {
                            this.blame_commit_messages
                                .insert(sha.clone(), CommitMessageState::Ready(message));
                        }
                        Ok(None) => {
                            this.blame_commit_messages.remove(&sha);
                        }
                        Err(err) => {
                            this.blame_commit_messages
                                .insert(sha.clone(), CommitMessageState::Failed(err.to_string()));
                        }
                    }
                    cx.notify();
                });
            }
        });
        self._blame_message_tasks.insert(sha, task);
    }

    /// The real, current Unix timestamp - the one live-clock read [`Self::render_file_view`]
    /// needs to compute a relative date (`crate::code_surface::blame::format_relative_date`).
    /// Isolated into its own tiny method so a future test can override it deterministically
    /// rather than every relative-date-dependent render assertion racing the real wall clock -
    /// not used that way yet (no such test exists this phase), but a real, honest seam rather
    /// than calling `SystemTime::now()` inline at the render call site.
    pub(in crate::code_surface) fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Builds the real inline blame render model for the current line of `absolute_path`, if
    /// any - see [`Self::current_line_blame`] for when this is `None`. Also kicks off
    /// [`Self::ensure_blame_commit_message`] for a committed line's sha, off-thread, so the
    /// hover tooltip picks up the real full message once it lands (a subsequent render just
    /// finds it already cached).
    pub(in crate::code_surface) fn inline_blame_render_model(
        &mut self,
        absolute_path: &Path,
        cursor_line: Option<usize>,
        buffer_dirty: bool,
        cx: &mut Context<Self>,
    ) -> Option<blame::InlineBlameLabel> {
        let line = self
            .current_line_blame(absolute_path, cursor_line, buffer_dirty)?
            .clone();
        let now = self.now_unix();
        if !line.is_uncommitted {
            self.ensure_blame_commit_message(line.sha.clone(), cx);
        }
        let full_message = match self.blame_commit_messages.get(&line.sha) {
            Some(CommitMessageState::Ready(message)) => Some(message.as_str()),
            _ => None,
        };
        Some(blame::inline_blame_label(&line, now, full_message))
    }
}

/// Renders `label` as the dimmed end-of-line span GitHub issue #29 asks for, with a real GPUI
/// hover tooltip (`crate::root::widgets::text_tooltip`, the same helper `crate::rail`/
/// `crate::sidebar`/`crate::status_bar` already use for their own truncated-text tooltips)
/// carrying the sha/full message. `line_number` only seeds the element id, so two rows never
/// collide.
pub(in crate::code_surface) fn render_inline_blame_span(
    label: &blame::InlineBlameLabel,
    line_number: usize,
) -> gpui::AnyElement {
    let mut el = div()
        .id(("file-view-blame", line_number))
        .min_w_0()
        .max_w(px(320.0))
        .truncate()
        .pl(px(16.0))
        .bg(theme::surface::CURRENT_LINE)
        .text_size(px(10.5))
        .text_color(theme::text::GHOST)
        .child(label.inline_text.clone());
    if let Some(tooltip_text) = label.tooltip_text.clone() {
        el = el.tooltip(text_tooltip(tooltip_text));
    }
    el.into_any_element()
}
