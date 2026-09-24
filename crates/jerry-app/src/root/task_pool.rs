use gpui::Task;

/// A collection of independent, in-flight background `Task`s - the shared shape behind
/// [`super::AdeApp::_merge_write_tasks`], [`super::AdeApp::_lsp_tasks`],
/// [`super::AdeApp::_goto_definition_tasks`], and [`super::AdeApp::_new_agent_pane_task`].
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
