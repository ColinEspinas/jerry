use super::*;
use crate::root::widgets::{render_keycap_row, KeycapSize};

impl AdeApp {
    /// The 26px status bar (`design_handoff_jerry_ade/README.md`'s Layout table: height 26,
    /// bg `#101214`, top border `#1e2225`). The mockup's own `↑2 ↓0` ahead/behind counts and
    /// `{{ statusLine }}` template placeholder still need git plumbing this phase doesn't build,
    /// so they're left out (rendering those would be exactly the "component bound to nothing"
    /// this project's constraints forbid) - but the `⌘K commands` hint is now real: the command
    /// palette exists as of this phase, so clicking it (or pressing the real `secondary-k`
    /// binding - see [`TogglePalette`]) really opens it, the same as `Jerry.dc.html`'s own
    /// `onClick={{onOpenPalette}}`. The mockup's second `⌘⇧K sessions` hint is deliberately
    /// omitted: that binding was never wired up in this phase (see the "Command palette" task
    /// docs' own scope), so showing a keycap for it would advertise a shortcut that silently
    /// does nothing if pressed.
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
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(self.repo_path.display().to_string()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
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
                        &keymap::resolve_combo("mod+K", self.window_controls_style.is_macos()),
                        KeycapSize::Standard,
                    ))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(10.5))
                            .text_color(theme::text::FAINT)
                            .child("commands"),
                    )
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.open_palette(window, cx);
                    })),
            )
    }
}
