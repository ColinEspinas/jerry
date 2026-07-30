use gpui::Task;

/// A collection of independent, in-flight background `Task`s - the shared shape behind
/// [`super::AdeApp::_merge_write_tasks`], [`super::AdeApp::_lsp_tasks`],
/// [`super::AdeApp::_goto_definition_tasks`], and [`super::AdeApp::_new_agent_pane_task`].
///
/// Unlike a single `Option<Task<()>>` slot (the right shape when a *newer* operation should
/// supersede an older, still-in-flight one - see e.g. `AdeApp::_hover_request_task`), these hold
/// operations that are genuinely independent, where dropping an unrelated one via a shared
/// single slot would silently cancel real in-flight work (see `AdeApp::_merge_write_tasks` for a
/// concrete reproduction of that bug). Left unbounded, the collection would only ever grow, since
/// nothing removes a finished task's slot on its own - [`TaskPool::push`] prunes before adding so
/// every one of the 6 call sites gets that for free.
#[derive(Default)]
pub(crate) struct TaskPool(Vec<Task<()>>);

impl TaskPool {
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }

    /// Prunes every already-finished task, then adds `task`.
    pub(crate) fn push(&mut self, task: Task<()>) {
        self.0.retain(|task| !task.is_ready());
        self.0.push(task);
    }
}
