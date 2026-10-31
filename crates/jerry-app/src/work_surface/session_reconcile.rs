//! Pure reconciliation of one worktree's persisted tab session against a repo host's real,
//! current session list (`docs/architecture/decisions.md` §26, decision Q21: "the app owns tab
//! layout and reconciles it with the host's session list at reconnect; orphans show as detached
//! sessions"). No GPUI, no I/O - `crate::work_surface::session` drives this against a real
//! `SessionsQuery` answer and acts on the result.

use std::collections::HashSet;
use std::path::Path;

use jerry_core::{ExitStatusWire, SessionId, SessionRecord};

use crate::work_surface::tab_order_state::SessionTab;

/// One persisted tab, resolved against the host's live session list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReconciledTab {
    /// A live, still-running session answers this tab's own `host_session_id` - reattach to it
    /// in place ([`crate::work_surface::agents::Agents::spawn_reattached`]), never a fresh spawn.
    Reattach {
        tab: SessionTab,
        session_id: SessionId,
    },
    /// No `host_session_id` was ever recorded for this tab (its `SessionSpawn` never resolved
    /// before quit, or the record predates this field) - the pre-existing spawn/CLI-`--resume`
    /// path handles it, entirely unchanged.
    Fallback { tab: SessionTab },
    /// A `host_session_id` was recorded but the host has no live session for it any more - shown
    /// already-exited, with whatever real status the host still remembers. `None` when the host
    /// has no memory of the id at all (a fresh host, or one that has since forgotten it) rather
    /// than a genuine observed exit.
    Gone {
        tab: SessionTab,
        status: Option<ExitStatusWire>,
    },
}

/// Reconciles `persisted` (one worktree's whole recorded session, in its real order) against
/// `live` (every session the repository's host currently knows about, spanning every worktree it
/// serves) - one [`ReconciledTab`] per persisted tab, in the same order, plus every live session
/// under `worktree` that answers to no persisted tab at all (decision Q21's "orphans" - shown as
/// detached sessions, never silently dropped).
pub(crate) fn reconcile(
    persisted: &[SessionTab],
    live: &[SessionRecord],
    worktree: &Path,
) -> (Vec<ReconciledTab>, Vec<SessionRecord>) {
    let tabs: Vec<ReconciledTab> = persisted
        .iter()
        .cloned()
        .map(|tab| reconcile_one(tab, live))
        .collect();

    let claimed: HashSet<&SessionId> = tabs
        .iter()
        .filter_map(|reconciled| match reconciled {
            ReconciledTab::Reattach { session_id, .. } => Some(session_id),
            ReconciledTab::Fallback { .. } | ReconciledTab::Gone { .. } => None,
        })
        .collect();
    let detached = live
        .iter()
        .filter(|record| record.worktree == worktree)
        .filter(|record| record.exit.is_none())
        .filter(|record| !claimed.contains(&record.id))
        .cloned()
        .collect();

    (tabs, detached)
}

fn reconcile_one(tab: SessionTab, live: &[SessionRecord]) -> ReconciledTab {
    let Some(host_session_id) = host_session_id_of(&tab) else {
        return ReconciledTab::Fallback { tab };
    };
    let id = SessionId::from(host_session_id);
    match live.iter().find(|record| record.id == id) {
        Some(record) if record.exit.is_none() => ReconciledTab::Reattach {
            tab,
            session_id: id,
        },
        Some(record) => ReconciledTab::Gone {
            tab,
            status: record.exit.clone(),
        },
        None => ReconciledTab::Gone { tab, status: None },
    }
}

fn host_session_id_of(tab: &SessionTab) -> Option<&str> {
    match tab {
        SessionTab::File(_) => None,
        SessionTab::Shell { host_session_id }
        | SessionTab::Agent {
            host_session_id, ..
        } => host_session_id.as_deref(),
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::{reconcile, ReconciledTab};
    use crate::work_surface::agents::AgentKind;
    use crate::work_surface::tab_order_state::SessionTab;
    use jerry_core::{ExitStatusWire, SessionId, SessionRecord};
    use std::path::{Path, PathBuf};

    fn shell_tab(host_session_id: Option<&str>) -> SessionTab {
        SessionTab::Shell {
            host_session_id: host_session_id.map(str::to_owned),
        }
    }

    fn agent_tab(host_session_id: Option<&str>) -> SessionTab {
        SessionTab::Agent {
            kind: AgentKind::Claude,
            session_id: None,
            host_session_id: host_session_id.map(str::to_owned),
        }
    }

    fn record(id: &str, worktree: &Path, exit: Option<ExitStatusWire>) -> SessionRecord {
        SessionRecord {
            id: SessionId::from(id),
            kind: jerry_core::SessionKind::Pty,
            worktree: worktree.to_path_buf(),
            agent: None,
            started_at: 0,
            exit,
            process_id: Some(1234),
            rows: 24,
            cols: 80,
        }
    }

    #[test]
    fn a_tab_whose_session_lives_reattaches_in_place() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let persisted = vec![shell_tab(Some("session-1"))];
        let live = vec![record("session-1", &worktree, None)];

        let (tabs, detached) = reconcile(&persisted, &live, &worktree);

        assert_eq!(
            tabs,
            vec![ReconciledTab::Reattach {
                tab: persisted[0].clone(),
                session_id: SessionId::from("session-1"),
            }]
        );
        assert!(
            detached.is_empty(),
            "the live session that was just claimed by a real tab must not also show up as \
             detached"
        );
    }

    #[test]
    fn a_live_session_with_no_tab_is_reported_as_detached() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let persisted: Vec<SessionTab> = Vec::new();
        let live = vec![record("session-orphan", &worktree, None)];

        let (tabs, detached) = reconcile(&persisted, &live, &worktree);

        assert!(tabs.is_empty());
        assert_eq!(
            detached, live,
            "the orphaned live session must be reported, not dropped"
        );
    }

    #[test]
    fn a_tab_whose_session_is_gone_carries_its_real_last_known_status() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let status = ExitStatusWire {
            success: false,
            code: 1,
            signal: None,
        };
        let persisted = vec![agent_tab(Some("session-2"))];
        let live = vec![record("session-2", &worktree, Some(status.clone()))];

        let (tabs, detached) = reconcile(&persisted, &live, &worktree);

        assert_eq!(
            tabs,
            vec![ReconciledTab::Gone {
                tab: persisted[0].clone(),
                status: Some(status),
            }]
        );
        assert!(
            detached.is_empty(),
            "an exited session is not a detached one"
        );
    }

    #[test]
    fn a_tab_whose_session_the_host_has_no_memory_of_at_all_is_still_gone() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let persisted = vec![shell_tab(Some("session-forgotten"))];

        let (tabs, _detached) = reconcile(&persisted, &[], &worktree);

        assert_eq!(
            tabs,
            vec![ReconciledTab::Gone {
                tab: persisted[0].clone(),
                status: None,
            }]
        );
    }

    #[test]
    fn a_tab_with_no_recorded_host_session_id_falls_back_unchanged() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let persisted = vec![agent_tab(None)];
        let live = vec![record("session-unrelated", &worktree, None)];

        let (tabs, detached) = reconcile(&persisted, &live, &worktree);

        assert_eq!(
            tabs,
            vec![ReconciledTab::Fallback {
                tab: persisted[0].clone(),
            }]
        );
        assert_eq!(
            detached, live,
            "an unrelated live session must still be reported as detached"
        );
    }

    #[test]
    fn a_file_tab_never_reconciles_against_a_session_at_all() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let persisted = vec![SessionTab::File(worktree.join("a.rs"))];

        let (tabs, _detached) = reconcile(&persisted, &[], &worktree);

        assert_eq!(
            tabs,
            vec![ReconciledTab::Fallback {
                tab: persisted[0].clone(),
            }]
        );
    }

    #[test]
    fn a_live_session_under_a_different_worktree_is_never_detached_here() {
        let worktree = PathBuf::from("/repo/worktree-a");
        let other = PathBuf::from("/repo/worktree-b");
        let live = vec![record("session-elsewhere", &other, None)];

        let (_tabs, detached) = reconcile(&[], &live, &worktree);

        assert!(
            detached.is_empty(),
            "a live session belonging to another worktree is that worktree's own reconciliation \
             to report, not this one's"
        );
    }
}
