//! Real, on-disk persistence for **one worktree's whole tab session** - which tabs were open, of
//! which kind, in which order - so quitting Jerry and relaunching it brings them all back.
//!
//! Started life as GitHub issue #16's drag-order-only store ("the resulting layout... persists per
//! session/worktree and restores on relaunch"), and still is exactly that for file tabs;
//! [`WorktreeTabOrder::tabs`] widened it from "the order of whatever happens to be open" into "the
//! real set of tabs that *were* open", which is what `crate::work_surface::session` needs to
//! genuinely reopen them. `crate::root::AdeApp::tab_order` remains the live, in-session mirror of
//! one worktree's order; this module is the durable half.
//!
//! ## Why an agent tab *can* be recorded now, when it deliberately couldn't before
//!
//! A [`crate::work_surface::state::TabRef::Agent`] carries an
//! [`crate::work_surface::agents::AgentId`] - a per-window counter that restarts at zero on every
//! launch, so a freshly-spawned agent can never match a persisted one by id, and this module's
//! original docs correctly refused to write a value nothing could read back.
//!
//! [`SessionTab::Agent`] persists something else entirely: never an id, only the two facts that
//! genuinely survive a restart - the [`crate::work_surface::agents::AgentKind`] and the real
//! Claude Code `session_id` (`crate::hooks::event::HookReport::session_id`, GitHub issue #227)
//! that a literal `claude --resume <session_id>` can pick the same conversation back up from.
//! A record with no session id is therefore *not* resumable and is honestly dropped at restore
//! time rather than turned into a fresh, contextless agent occupying the same slot - see
//! `crate::work_surface::session::AdeApp::restore_worktree_session`'s own docs.
//!
//! [`SessionTab::Shell`] carries nothing at all, for the same honesty: a real OS shell process
//! cannot survive an app quit, so "restoring" one can only ever mean spawning a fresh shell into
//! the same worktree, in the same tab slot. There is no process-reattachment anywhere in this
//! codebase to pretend otherwise with.
//!
//! ## [`WorktreeTabOrder::files`] is kept, written, and never independently authored
//!
//! [`WorktreeTabOrder::tabs`] is authoritative. `files` is issue #16's original field, still
//! written on every save - derived by the one writer ([`TabOrderState::set_session_tabs`]) from
//! the very same list - purely so a Jerry build older than this one, sharing a `~/.config/jerry`,
//! keeps reading a real drag order rather than an empty one. It is read back only when `tabs` is
//! absent (a file written *by* such a build), which is why the two can never disagree: nothing
//! ever sets one without setting the other in the same call.
//!
//! ## Everything else - file format, atomicity, multi-instance merge - mirrors `fold_state`
//!
//! Copied rather than reinvented, matching `crate::rail::repo::RepoState`'s own precedent for
//! doing the same against this exact module: real crash-safe atomic writes
//! ([`TabOrderState::save_at`]: a process-unique sibling temp file, `sync_all`, `rename`, a
//! directory sync), and a merge-not-clobber real write path
//! ([`TabOrderState::save_merged_at`]) so a second `jerry` instance browsing a different
//! repository can't erase this one's saved order. See `crate::sidebar::fold_state`'s own module
//! docs for the full reasoning behind both, which applies here unchanged - including the same
//! honest limits (an unlocked read-modify-write around the merge; two instances open on the
//! *same* worktree still last-writer-wins).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::work_surface::agents::AgentKind;

/// The tab-order file's name, resolved next to the real `settings.toml` - mirrors
/// `crate::sidebar::fold_state::FOLD_STATE_FILE_NAME`.
pub const TAB_ORDER_FILE_NAME: &str = "tab-order.toml";

/// The tab-order file for a given real settings-file path - identical reasoning to
/// `crate::sidebar::fold_state::fold_state_path_for`: a test that supplies a temp-dir settings
/// path gets real, isolated tab-order persistence in that same directory, and a test that passes
/// `None` gets none at all.
pub fn tab_order_path_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join(TAB_ORDER_FILE_NAME),
        None => PathBuf::from(TAB_ORDER_FILE_NAME),
    }
}

/// The map key for one worktree - see `crate::sidebar::fold_state::worktree_key`'s own docs for
/// the identical canonicalization/UTF-8 reasoning, copied here unchanged.
pub fn worktree_key(root: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    canonical.to_str().map(str::to_owned)
}

/// One worktree's stored relative file path, or `None` if `file` isn't a real descendant of
/// `root` - mirrors `crate::sidebar::fold_state::relative_key`'s own traversal guard.
fn relative_key(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The read-side half of [`relative_key`] - refuses anything that isn't a plain sequence of
/// normal path segments, so a hand-edited or corrupted file can never name a path outside the
/// worktree it's filed under. Mirrors `crate::sidebar::fold_state::absolute_from_key` exactly.
fn absolute_from_key(root: &Path, key: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    let mut segments = 0;
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return None;
        }
        if segment.contains('\\') || Path::new(segment).components().count() != 1 {
            return None;
        }
        path.push(segment);
        segments += 1;
    }
    (segments > 0).then_some(path)
}

/// How stale a `*.tmp` sibling must be before a save sweeps it - mirrors
/// `crate::sidebar::fold_state`'s own constant/reasoning.
const ORPHANED_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Mirrors `crate::sidebar::fold_state::sweep_orphaned_temp_files` exactly - see that function's
/// own docs.
fn sweep_orphaned_temp_files(path: &Path, file_name: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let prefix = format!("{file_name}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| {
                std::time::SystemTime::now()
                    .duration_since(modified)
                    .map_err(|err| io::Error::other(err.to_string()))
            })
            .is_ok_and(|age| age > ORPHANED_TEMP_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The whole on-disk file: every worktree this app has ever recorded a drag order for, keyed by
/// [`worktree_key`]. A `BTreeMap` so the serialized file has a stable, diffable order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabOrderState {
    pub worktrees: BTreeMap<String, WorktreeTabOrder>,
}

/// One worktree's entry - its whole tab session, in real tab-strip order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeTabOrder {
    /// GitHub issue #16's original field: this worktree's file tabs' relative paths, in real drag
    /// order. Derived from [`Self::tabs`] by the single writer and written on every save purely
    /// for an older Jerry build sharing the same config directory - see the module docs.
    pub files: Vec<String>,
    /// The authoritative record: every tab that was open, of every kind, in real order.
    pub tabs: Vec<PersistedTab>,
}

/// One tab's on-disk record. Deliberately a flat struct with a [`Self::kind`] *string* tag rather
/// than a `#[derive(Serialize)]`'d Rust enum, matching `crate::hooks::store::status_key`'s own
/// reasoning exactly: this file is a compatibility surface a future release must still be able to
/// read, and deriving the format from an enum would silently couple it to a type this codebase
/// renames freely. An unrecognised `kind` is skipped on read, never guessed at
/// ([`PersistedTab::decode`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedTab {
    /// [`TAB_KIND_FILE`], [`TAB_KIND_SHELL`] or [`TAB_KIND_AGENT`].
    pub kind: String,
    /// [`TAB_KIND_FILE`] only: the worktree-relative path, in the same `/`-joined form
    /// [`WorktreeTabOrder::files`] uses.
    pub path: Option<String>,
    /// [`TAB_KIND_AGENT`] only: `crate::work_surface::agents::AgentKind::label`.
    pub agent: Option<String>,
    /// [`TAB_KIND_AGENT`] only: the real Claude Code `session_id` this agent's hooks last
    /// reported, if any. `None` for a Codex agent (no hooks exist for Codex at all) and for a
    /// Claude agent that closed before any hook reported one - both genuinely unresumable, and
    /// treated as such rather than downgraded to a fresh spawn.
    pub session_id: Option<String>,
}

/// The stable on-disk spelling of a [`SessionTab`]'s kind - see [`PersistedTab::kind`].
pub const TAB_KIND_FILE: &str = "file";
pub const TAB_KIND_SHELL: &str = "shell";
pub const TAB_KIND_AGENT: &str = "agent";

/// One tab, decoded into the form a caller can act on - the read/write currency of this module's
/// whole session API ([`TabOrderState::session_tabs`]/[`TabOrderState::set_session_tabs`]).
///
/// [`Self::File`] carries an **absolute** path (already resolved against the worktree root), so a
/// caller never has to re-derive one and can never accidentally resolve it against the wrong root;
/// the relative encoding is this module's own storage detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTab {
    File(PathBuf),
    Shell,
    Agent {
        kind: AgentKind,
        session_id: Option<String>,
    },
}

impl PersistedTab {
    /// `self`, decoded against `root`, or `None` if this record isn't usable - an unrecognised
    /// `kind`/`agent` label (a record written by a future release), or a `path` that doesn't decode
    /// to a plain descendant of `root` ([`absolute_from_key`]). Skipping such a record rather than
    /// guessing at it mirrors `crate::hooks::history::PastAgent::from_record`'s own contract.
    fn decode(&self, root: &Path) -> Option<SessionTab> {
        match self.kind.as_str() {
            TAB_KIND_FILE => absolute_from_key(root, self.path.as_deref()?).map(SessionTab::File),
            TAB_KIND_SHELL => Some(SessionTab::Shell),
            TAB_KIND_AGENT => Some(SessionTab::Agent {
                kind: AgentKind::from_label(self.agent.as_deref()?)?,
                session_id: self.session_id.clone(),
            }),
            _ => None,
        }
    }

    /// `tab`, encoded relative to `root`, or `None` for a [`SessionTab::File`] whose path isn't a
    /// plain, UTF-8 descendant of `root` ([`relative_key`]) - the same single unrecordable entry
    /// [`TabOrderState::set_session_tabs`] drops without losing the real entries around it.
    fn encode(root: &Path, tab: &SessionTab) -> Option<PersistedTab> {
        match tab {
            SessionTab::File(path) => Some(PersistedTab {
                kind: TAB_KIND_FILE.to_owned(),
                path: Some(relative_key(root, path)?),
                ..PersistedTab::default()
            }),
            SessionTab::Shell => Some(PersistedTab {
                kind: TAB_KIND_SHELL.to_owned(),
                ..PersistedTab::default()
            }),
            SessionTab::Agent { kind, session_id } => Some(PersistedTab {
                kind: TAB_KIND_AGENT.to_owned(),
                agent: Some(kind.label().to_owned()),
                session_id: session_id.clone(),
                ..PersistedTab::default()
            }),
        }
    }
}

impl TabOrderState {
    /// Loads `path`, falling back to an empty state for *any* failure - same "never important
    /// enough to fail startup over" rule `crate::sidebar::fold_state::FoldState::load_at` states.
    pub fn load_at(path: &Path) -> TabOrderState {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return TabOrderState::default();
        };
        match toml::from_str::<TabOrderState>(&contents) {
            Ok(state) => state,
            Err(err) => {
                log::warn!(
                    "{} failed to parse ({err}) - starting from an empty tab order",
                    path.display()
                );
                TabOrderState::default()
            }
        }
    }

    /// Writes `self` to `path` atomically - a sibling `*.tmp` file, `sync_all`, then a rename
    /// over the target, then a parent-directory sync. See
    /// `crate::sidebar::fold_state::FoldState::save_at`'s own docs for the full reasoning; this
    /// is that same sequence, copied.
    pub fn save_at(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| TAB_ORDER_FILE_NAME.to_string());
        static WRITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = WRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = path.with_file_name(format!("{file_name}.{}.{unique}.tmp", std::process::id()));

        let write_result = (|| -> io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()
        })();
        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
        if let Err(err) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(err);
        }
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        sweep_orphaned_temp_files(path, &file_name);
        Ok(())
    }

    /// The app's real write path: merges `self`'s entries for the worktrees this instance
    /// actually owns into whatever is currently on disk, then writes the result via
    /// [`Self::save_at`] - identical reasoning to
    /// `crate::sidebar::fold_state::FoldState::save_merged_at`.
    ///
    /// GitHub issue #90: wrapped in `crate::persisted_state_lock::with_locked_merge` - see that
    /// module's own docs for the real intra-process concurrent-writer race "New Window" made
    /// reachable here, and why one process-wide lock, shared with `crate::rail::repo::RepoState`/
    /// `crate::sidebar::fold_state::FoldState`'s own identical methods, is enough to close it.
    pub fn save_merged_at(&self, path: &Path, owned: &BTreeSet<String>) -> io::Result<()> {
        crate::persisted_state_lock::with_locked_merge(|| {
            let mut merged = TabOrderState::load_at(path);
            for key in owned {
                match self.worktrees.get(key) {
                    Some(entry) => merged.worktrees.insert(key.clone(), entry.clone()),
                    None => merged.worktrees.remove(key),
                };
            }
            merged.save_at(path)
        })
    }

    /// `root`'s real, currently-recorded tab session, in real tab-strip order. A worktree with no
    /// entry at all (never seen, or every tab has since closed) returns an empty list - never an
    /// error, matching a fresh worktree's own real "nothing recorded yet" state. Individual
    /// records that don't decode are silently skipped ([`PersistedTab::decode`]), so one entry
    /// written by a future release can never cost a user the rest of their session.
    ///
    /// Falls back to [`WorktreeTabOrder::files`] when [`WorktreeTabOrder::tabs`] is empty - see
    /// the module docs: that is exactly a file written by a Jerry build predating the session
    /// record, whose file tabs are still real, still in the user's real drag order, and still
    /// worth reopening.
    pub fn session_tabs(&self, root: &Path) -> Vec<SessionTab> {
        let Some(key) = worktree_key(root) else {
            return Vec::new();
        };
        let Some(entry) = self.worktrees.get(&key) else {
            return Vec::new();
        };
        if entry.tabs.is_empty() {
            return entry
                .files
                .iter()
                .filter_map(|key| absolute_from_key(root, key))
                .map(SessionTab::File)
                .collect();
        }
        entry
            .tabs
            .iter()
            .filter_map(|tab| tab.decode(root))
            .collect()
    }

    /// `root`'s recorded **file** tabs only, as absolute paths ready to feed into
    /// [`crate::work_surface::state::TabRef::File`] - [`Self::session_tabs`] narrowed to the one
    /// kind `crate::work_surface::render::AdeApp::combined_tab_order`'s own persisted-order
    /// fallback can use directly (an agent tab's slot there is decided by whichever live agent
    /// really exists, not by this file).
    pub fn file_order(&self, root: &Path) -> Vec<PathBuf> {
        self.session_tabs(root)
            .into_iter()
            .filter_map(|tab| match tab {
                SessionTab::File(path) => Some(path),
                SessionTab::Shell | SessionTab::Agent { .. } => None,
            })
            .collect()
    }

    /// Records `root`'s real, current tab session (in real tab-strip order) - the write-side
    /// counterpart to [`Self::session_tabs`], and the **only** writer of either field, which is
    /// what keeps [`WorktreeTabOrder::files`] from ever disagreeing with
    /// [`WorktreeTabOrder::tabs`] (see the module docs).
    ///
    /// A [`SessionTab::File`] whose path isn't a plain, UTF-8 descendant of `root` is silently
    /// dropped from the recorded session rather than refusing the whole call: unlike
    /// `FoldState::set_expanded`'s single-path calls, this always records a whole worktree's
    /// session at once, and one unrecordable entry must not lose every other real, recordable one
    /// alongside it.
    ///
    /// An empty result removes the worktree's entry entirely rather than leaving an empty one
    /// behind forever - the same "closing every tab forgets the worktree" contract this method's
    /// file-only predecessor already had.
    pub fn set_session_tabs(&mut self, root: &Path, tabs: &[SessionTab]) {
        let Some(root_key) = worktree_key(root) else {
            return;
        };
        let encoded: Vec<PersistedTab> = tabs
            .iter()
            .filter_map(|tab| PersistedTab::encode(root, tab))
            .collect();
        if encoded.is_empty() {
            self.worktrees.remove(&root_key);
            return;
        }
        let files: Vec<String> = encoded
            .iter()
            .filter(|tab| tab.kind == TAB_KIND_FILE)
            .filter_map(|tab| tab.path.clone())
            .collect();
        self.worktrees.insert(
            root_key,
            WorktreeTabOrder {
                files,
                tabs: encoded,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common "these files, in this order" session, so a test about *ordering* doesn't have
    /// to spell out a `SessionTab::File` wrapper per entry.
    fn file_session(paths: &[PathBuf]) -> Vec<SessionTab> {
        paths.iter().cloned().map(SessionTab::File).collect()
    }

    #[test]
    fn tab_order_lives_next_to_the_real_settings_file() {
        let path = tab_order_path_for(Path::new("/home/someone/.config/jerry/settings.toml"));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.config/jerry/tab-order.toml")
        );
    }

    #[test]
    fn an_unseen_worktree_has_no_recorded_order() {
        let state = TabOrderState::default();
        assert!(state
            .file_order(Path::new("/repo/fresh-worktree"))
            .is_empty());
    }

    #[test]
    fn set_and_read_round_trips_a_real_order() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(
            root,
            &file_session(&[root.join("src/main.rs"), root.join("README.md")]),
        );
        assert_eq!(
            state.file_order(root),
            vec![root.join("src/main.rs"), root.join("README.md")],
            "the real drag order must round-trip exactly, including which file comes first"
        );
    }

    #[test]
    fn recording_an_empty_session_forgets_the_worktree_entirely() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(root, &file_session(&[root.join("a.txt")]));
        assert!(!state.worktrees.is_empty());

        state.set_session_tabs(root, &[]);
        assert!(
            state.worktrees.is_empty(),
            "closing every file tab must not leave an empty entry behind forever"
        );
    }

    #[test]
    fn a_path_outside_the_worktree_is_dropped_but_the_rest_of_the_order_survives() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(
            root,
            &file_session(&[
                root.join("src/main.rs"),
                PathBuf::from("/etc/passwd"),
                root.join("README.md"),
            ]),
        );
        assert_eq!(
            state.file_order(root),
            vec![root.join("src/main.rs"), root.join("README.md")],
            "an unrecordable entry must be dropped without losing the real, recordable ones \
             around it"
        );
    }

    #[test]
    fn tab_order_for_one_worktree_never_leaks_into_another() {
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");
        let mut state = TabOrderState::default();
        state.set_session_tabs(a, &file_session(&[a.join("src/main.rs")]));

        assert_eq!(state.file_order(a), vec![a.join("src/main.rs")]);
        assert!(
            state.file_order(b).is_empty(),
            "worktree B shares the relative path `src/main.rs` with A, and must still start \
             with no recorded order"
        );
    }

    #[test]
    fn saving_and_reloading_round_trips_through_a_real_file() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        let root = Path::new("/repo/worktree-a");

        let mut state = TabOrderState::default();
        state.set_session_tabs(root, &file_session(&[root.join("a.rs"), root.join("b.rs")]));
        state.save_at(&path).expect("save");

        let reloaded = TabOrderState::load_at(&path);
        assert_eq!(reloaded, state);
        assert_eq!(
            reloaded.file_order(root),
            vec![root.join("a.rs"), root.join("b.rs")]
        );
    }

    #[test]
    fn saving_merges_with_another_instances_entries_instead_of_erasing_them() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        let a = Path::new("/repo/worktree-a");
        let b = Path::new("/repo/worktree-b");

        let mut instance_a = TabOrderState::default();
        instance_a.set_session_tabs(a, &file_session(&[a.join("a.rs")]));
        let owned_a: BTreeSet<String> = [worktree_key(a).expect("key")].into_iter().collect();
        instance_a.save_merged_at(&path, &owned_a).expect("save a");

        let mut instance_b = TabOrderState::default();
        instance_b.set_session_tabs(b, &file_session(&[b.join("b.rs")]));
        let owned_b: BTreeSet<String> = [worktree_key(b).expect("key")].into_iter().collect();
        instance_b.save_merged_at(&path, &owned_b).expect("save b");

        let on_disk = TabOrderState::load_at(&path);
        assert_eq!(
            on_disk.file_order(a),
            vec![a.join("a.rs")],
            "instance B's save must not have erased instance A's worktree"
        );
        assert_eq!(on_disk.file_order(b), vec![b.join("b.rs")]);
    }

    #[test]
    fn a_missing_file_loads_as_empty_state_rather_than_failing() {
        let dir = crate::test_support::temp_root();
        let state = TabOrderState::load_at(&dir.path().join("does-not-exist.toml"));
        assert_eq!(state, TabOrderState::default());
    }

    #[test]
    fn a_corrupted_file_loads_as_empty_state_rather_than_failing() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        std::fs::write(&path, "this is not valid toml {{{").expect("write");
        assert_eq!(TabOrderState::load_at(&path), TabOrderState::default());
    }

    #[test]
    fn a_traversal_entry_in_a_hand_edited_file_is_ignored() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        std::fs::write(
            &path,
            "[worktrees.\"/repo/worktree-a\"]\nfiles = [\"../worktree-b/secret.rs\", \"src/main.rs\"]\n",
        )
        .expect("write");

        let state = TabOrderState::load_at(&path);
        assert_eq!(
            state.file_order(Path::new("/repo/worktree-a")),
            vec![PathBuf::from("/repo/worktree-a/src/main.rs")],
            "the `..` entry must be silently dropped, and the good one kept"
        );
    }

    #[test]
    fn saving_leaves_no_temp_file_behind_and_the_result_parses() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("nested").join("tab-order.toml");
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(root, &file_session(&[root.join("a.rs")]));

        state.save_at(&path).expect("save");

        assert!(path.exists());
        let siblings: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(siblings, vec!["tab-order.toml".to_string()]);
    }

    /// The whole point of the session record: a heterogeneous tab strip - a file, then a shell,
    /// then a resumable Claude agent - must round-trip through a real file with every kind, every
    /// payload, and the real interleaved *order* intact.
    #[test]
    fn a_mixed_session_round_trips_through_a_real_file_with_its_order_intact() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        let root = Path::new("/repo/worktree-a");
        let session = vec![
            SessionTab::File(root.join("src/main.rs")),
            SessionTab::Shell,
            SessionTab::Agent {
                kind: AgentKind::Claude,
                session_id: Some("5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned()),
            },
            SessionTab::File(root.join("README.md")),
        ];

        let mut state = TabOrderState::default();
        state.set_session_tabs(root, &session);
        state.save_at(&path).expect("save");

        assert_eq!(
            TabOrderState::load_at(&path).session_tabs(root),
            session,
            "every tab kind, its payload, and its real slot in the strip must survive a restart"
        );
    }

    /// A Codex agent never has a session id (no hooks exist for it at all) - the record must still
    /// round-trip honestly as a Codex agent with `None`, so the restore side can make its own real
    /// "this one can't be resumed" decision rather than this layer inventing one.
    #[test]
    fn an_agent_with_no_session_id_round_trips_as_exactly_that() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(
            root,
            &[SessionTab::Agent {
                kind: AgentKind::Codex,
                session_id: None,
            }],
        );
        assert_eq!(
            state.session_tabs(root),
            vec![SessionTab::Agent {
                kind: AgentKind::Codex,
                session_id: None,
            }]
        );
    }

    /// `files` exists only so an older Jerry build sharing this config directory keeps reading a
    /// real drag order - it must therefore always be written, and always agree with the file half
    /// of `tabs`, never be authored separately.
    #[test]
    fn the_legacy_files_field_is_always_written_from_the_same_session_record() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(
            root,
            &[
                SessionTab::File(root.join("a.rs")),
                SessionTab::Shell,
                SessionTab::File(root.join("b.rs")),
            ],
        );
        let entry = state
            .worktrees
            .get(&worktree_key(root).expect("key"))
            .expect("entry");
        assert_eq!(
            entry.files,
            vec!["a.rs".to_string(), "b.rs".to_string()],
            "the legacy field must hold exactly the file tabs, in the same real order"
        );
        assert_eq!(entry.tabs.len(), 3, "and `tabs` keeps the shell as well");
    }

    /// The other half of that compatibility promise: a `tab-order.toml` written *by* such an older
    /// build has no `tabs` at all, and its file tabs are still real, still in the user's own drag
    /// order, and must still be restored rather than silently ignored.
    #[test]
    fn a_file_written_before_sessions_existed_still_restores_its_file_tabs() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        std::fs::write(
            &path,
            "[worktrees.\"/repo/worktree-a\"]\nfiles = [\"src/main.rs\", \"README.md\"]\n",
        )
        .expect("write");

        let root = Path::new("/repo/worktree-a");
        assert_eq!(
            TabOrderState::load_at(&path).session_tabs(root),
            vec![
                SessionTab::File(root.join("src/main.rs")),
                SessionTab::File(root.join("README.md")),
            ]
        );
    }

    /// A record written by a *future* release (an unknown tab kind, or an agent label this build
    /// doesn't know) must be skipped, never guessed at - and skipping it must not cost the user
    /// the real tabs recorded around it.
    #[test]
    fn an_unrecognised_record_is_skipped_without_losing_the_real_tabs_around_it() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        std::fs::write(
            &path,
            "[worktrees.\"/repo/worktree-a\"]\n\
             files = []\n\
             [[worktrees.\"/repo/worktree-a\".tabs]]\n\
             kind = \"file\"\n\
             path = \"a.rs\"\n\
             [[worktrees.\"/repo/worktree-a\".tabs]]\n\
             kind = \"holodeck\"\n\
             [[worktrees.\"/repo/worktree-a\".tabs]]\n\
             kind = \"agent\"\n\
             agent = \"SomeFutureAgent\"\n\
             [[worktrees.\"/repo/worktree-a\".tabs]]\n\
             kind = \"shell\"\n",
        )
        .expect("write");

        let root = Path::new("/repo/worktree-a");
        assert_eq!(
            TabOrderState::load_at(&path).session_tabs(root),
            vec![SessionTab::File(root.join("a.rs")), SessionTab::Shell],
        );
    }

    /// The same traversal guard `files` has always had, applied to a hand-edited `tabs` entry: a
    /// `..` path must never decode into a file outside the worktree it is filed under.
    #[test]
    fn a_traversal_session_entry_in_a_hand_edited_file_is_ignored() {
        let dir = crate::test_support::temp_root();
        let path = dir.path().join("tab-order.toml");
        std::fs::write(
            &path,
            "[worktrees.\"/repo/worktree-a\"]\n\
             files = []\n\
             [[worktrees.\"/repo/worktree-a\".tabs]]\n\
             kind = \"file\"\n\
             path = \"../worktree-b/secret.rs\"\n\
             [[worktrees.\"/repo/worktree-a\".tabs]]\n\
             kind = \"file\"\n\
             path = \"src/main.rs\"\n",
        )
        .expect("write");

        assert_eq!(
            TabOrderState::load_at(&path).session_tabs(Path::new("/repo/worktree-a")),
            vec![SessionTab::File(PathBuf::from(
                "/repo/worktree-a/src/main.rs"
            ))],
        );
    }

    /// A worktree whose whole session is a single unrecordable file (outside the root) must be
    /// forgotten entirely rather than left with an empty entry - the same contract an explicitly
    /// empty session already has.
    #[test]
    fn a_session_that_encodes_to_nothing_forgets_the_worktree() {
        let root = Path::new("/repo/worktree-a");
        let mut state = TabOrderState::default();
        state.set_session_tabs(root, &file_session(&[root.join("a.rs")]));
        state.set_session_tabs(root, &file_session(&[PathBuf::from("/etc/passwd")]));
        assert!(state.worktrees.is_empty());
    }
}
