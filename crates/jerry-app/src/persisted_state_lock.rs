//! A single process-wide lock, shared by every real "merged, multi-writer-safe persisted file"
//! this app has - `crate::rail::repo::RepoState` (`repos.toml`), `crate::sidebar::fold_state::
//! FoldState` (`file-tree-state.toml`), and `crate::work_surface::tab_order_state::TabOrderState`
//! (`tab-order.toml`). All three follow the identical shape: load whatever is currently on disk,
//! merge in only the keys *this* instance owns, write the result back
//! (`*::save_merged_at` on each) - already safe against two separate `jerry` *processes* sharing
//! one `~/.config/jerry` (see `RepoState::save_merged_at`'s own docs for the full reasoning that
//! shape exists for).

use std::sync::{Mutex, OnceLock};

/// Runs `f` while holding the shared lock - see the module's own docs for exactly what real
/// hazard this closes and why one global lock is enough. A prior panic while holding the lock
/// poisons the underlying [`Mutex`]; recovered via [`std::sync::PoisonError::into_inner`] rather
/// than propagating (a merged-save failure is already handled by its own caller as a plain
/// `io::Result` - a poisoned lock must not additionally cascade into every *other* future call
/// that never itself did anything wrong).
pub(crate) fn with_locked_merge<T>(f: impl FnOnce() -> T) -> T {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f()
}
