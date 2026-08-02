//! A single process-wide lock, shared by every real "merged, multi-writer-safe persisted file"
//! this app has - `crate::rail::repo::RepoState` (`repos.toml`), `crate::sidebar::fold_state::
//! FoldState` (`file-tree-state.toml`), and `crate::work_surface::tab_order_state::TabOrderState`
//! (`tab-order.toml`). All three follow the identical shape: load whatever is currently on disk,
//! merge in only the keys *this* instance owns, write the result back
//! (`*::save_merged_at` on each) - already safe against two separate `jerry` *processes* sharing
//! one `~/.config/jerry` (see `RepoState::save_merged_at`'s own docs for the full reasoning that
//! shape exists for).
//!
//! GitHub issue #90's "New Window" introduced a real hazard none of the three had before: two
//! independent `AdeApp` instances - each with its own async writer loop - now genuinely run
//! *inside the same process*, and can call `save_merged_at` on the exact same file at (as near
//! as makes no difference) the same instant. The `owned`-scoped merge above only protects against
//! two writers whose `load_at` calls are properly ordered relative to each other's writes -
//! two truly concurrent calls can both read the same pre-write state, merge their own keys into
//! their own independent copy, and whichever `save_at` lands second silently discards the first's
//! freshly-written keys.
//!
//! [`with_locked_merge`] closes that window: every one of the three `save_merged_at` methods
//! wraps its whole read-modify-write cycle in this same lock, so real in-process concurrent
//! writers now genuinely serialize instead of racing. Deliberately **one** global lock, not three
//! independent ones and not one keyed per real file path: this app only ever has one real path
//! for each of the three files in production, and serializing an unrelated pair of test temp-dir
//! paths against each other costs nothing worth a per-path lock registry's own added complexity -
//! the identical tradeoff `RepoState::save_merged_at`'s own first version of this already made
//! before this module existed to share it.

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
