//! The status bar's real update chip - one `impl AdeApp` method, pushed into
//! `crate::status_bar::render::AdeApp::render_status_bar_left`'s segment list, first (higher
//! priority than the transient worktree-history notice - an available/ready update is
//! persistent, actionable information, not a one-off completion message). Renders nothing at
//! all (`None`) while [`state::UpdateState::Idle`] - which, per that enum's own docs, covers
//! "never checked", "genuinely up to date", and "the last check failed" alike, satisfying "hidden
//! when up to date, and never intrusive on a check failure" in one real branch.

use super::*;

impl AdeApp {
    /// Follows `crate::status_bar::render::AdeApp::render_status_branch_cluster`'s exact
    /// clickable-chip idiom (`cursor_pointer` + `rounded(theme::radius::CHIP)` +
    /// `hover(theme::surface::ROW_HOVER_ALT)` + `on_click(cx.listener(..))`) for the two states
    /// that are genuinely clickable; `Downloading`/`Failed` render the same 10px mono text
    /// without the clickable wrapper.
    pub(crate) fn render_status_update_notice(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        match self.update_state.clone() {
            state::UpdateState::Idle => None,
            state::UpdateState::UpdateAvailable(_) => Some(
                div()
                    .id("status-bar-update-available")
                    .cursor_pointer()
                    .rounded(theme::radius::CHIP)
                    .px(px(4.0))
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.start_update_download(cx);
                    }))
                    .child(self.render_update_text("Update available", theme::term::LINK))
                    .into_any_element(),
            ),
            state::UpdateState::Downloading { .. } => Some(
                div()
                    .id("status-bar-update-downloading")
                    .child(self.render_update_text("Updating\u{2026}", theme::text::DIM))
                    .into_any_element(),
            ),
            state::UpdateState::ReadyToRestart { .. } => Some(
                div()
                    .id("status-bar-update-restart")
                    .cursor_pointer()
                    .rounded(theme::radius::CHIP)
                    .px(px(4.0))
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.restart_after_update(cx);
                    }))
                    .child(self.render_update_text("Restart to update", theme::term::WARN))
                    .into_any_element(),
            ),
            state::UpdateState::Failed { error, .. } => Some(
                div()
                    .id("status-bar-update-failed")
                    .min_w_0()
                    .max_w(px(200.0))
                    .truncate()
                    .tooltip(text_tooltip(error))
                    .child(self.render_update_text("Update failed", theme::button::DANGER_FG))
                    .into_any_element(),
            ),
        }
    }

    /// The plain, non-clickable 10px mono status-bar text style, colored per `update_state` -
    /// a colored twin of `crate::status_bar::render::AdeApp::render_status_text`'s own plain
    /// `theme::text::DIM`-only version (that method is private to `crate::status_bar::render`,
    /// so this is a small, deliberate, differently-colored copy rather than a visibility change
    /// to a different feature's own helper).
    fn render_update_text(
        &self,
        label: &'static str,
        color: theme::ColorToken,
    ) -> impl IntoElement {
        div()
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(10.0))
            .text_color(color)
            .child(label)
    }
}
