use gpui::Task;

/// A real, independent-in-flight-operations collection of background `Task`s - the shared
/// shape behind [`super::AdeApp::_merge_write_tasks`], [`super::AdeApp::_lsp_tasks`],
/// [`super::AdeApp::_goto_definition_tasks`], and [`super::AdeApp::_new_agent_pane_task`].
///
/// Each of those fields exists for the identical real reason: unlike a single `Option<Task<
/// ()>>` slot (the right shape when a *newer* operation should supersede an older, still-
/// in-flight one - see e.g. `AdeApp::_hover_request_task`'s own docs), these hold operations
/// that are genuinely independent of each other, where dropping an unrelated one via a shared
/// single slot would silently cancel real, in-flight work (a real bug this codebase hit more
/// than once - see `AdeApp::_merge_write_tasks`'s own docs for the exact reproduction: resolving
/// one conflicted file's last hunk while a *different* file's write was still in flight used to
/// cancel the earlier write, leaving real conflict markers on disk while the in-memory model
/// already reported it resolved).
///
/// Left unbounded, any of these would only ever grow, since nothing removed a finished task's
/// slot once its own real operation completed. Every one of the 6 real call sites across this
/// codebase (`merge_flow.rs`, `work_surface_render.rs`, `lsp.rs` ×3, `code_surface.rs`) that used
/// one of these fields independently reimplemented the same two-line idiom to prevent that
/// (`self._x_tasks.retain(|task| !task.is_ready()); self._x_tasks.push(task);`), and a future new
/// call site copying only the second line, not the first, would silently reintroduce unbounded
/// growth with nothing to catch it. [`TaskPool::push`] is that idiom, written once.
#[derive(Default)]
pub(super) struct TaskPool(Vec<Task<()>>);

impl TaskPool {
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }

    /// Prunes every already-finished task (`Task::is_ready`), then adds `task` - see this
    /// type's own docs for why both halves matter together.
    pub(super) fn push(&mut self, task: Task<()>) {
        self.0.retain(|task| !task.is_ready());
        self.0.push(task);
    }
}
