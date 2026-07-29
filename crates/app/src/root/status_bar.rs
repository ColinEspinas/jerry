use super::*;
use crate::root::widgets::{render_keycap_row, KeycapSize};

impl AdeApp {
    /// The 26px status bar. The mockup's `↑2 ↓0` ahead/behind counts and `{{ statusLine }}`
    /// template placeholder need git plumbing this app doesn't build, so they're left out rather
    /// than bound to nothing. The `⌘K commands` hint is real: clicking it (or pressing the bound
    /// `secondary-k` - see [`TogglePalette`]) opens the command palette. The mockup's second
    /// `⌘⇧K sessions` hint is omitted since that binding is never wired up - showing a keycap for
    /// it would advertise a shortcut that silently does nothing.
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let worktree_count = self.worktrees.len();
        let label = match worktree_count {
            1 => "1 worktree".to_string(),
            n => format!("{n} worktrees"),
        };

        div()
            .id("status-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .px(px(12.0))
            .w_full()
            .h(theme::band::STATUS_BAR)
            .bg(theme::surface::TITLE_BAR)
            .border_t_1()
            .border_color(theme::border::ZONE)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::GHOST)
                    .child(self.repo_path.display().to_string()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::GHOST)
                    .child(label),
            )
            .child(
                div()
                    .id("status-bar-open-palette")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(render_keycap_row(
                        &keymap::resolve_combo("mod+K", self.window_controls_style().is_macos()),
                        KeycapSize::Standard,
                    ))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(10.5))
                            .text_color(theme::text::FAINT)
                            .child("commands"),
                    )
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.open_palette(window, cx);
                    })),
            )
    }
}
