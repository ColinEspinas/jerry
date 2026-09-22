//! SPIKE (issue #503, part A), test-only: exercises `gpui_base::dock::PaneTree`'s real edit
//! algebra as a stand-in for this crate's own hand-rolled tab-strip reorder
//! (`crate::work_surface::state::move_tab_order`, and `crate::root::AdeApp`'s
//! `dragging_tab`/`tab_drag_insertion`/`tab_bounds` fields). Not wired into any render path -
//! see `docs/architecture/decisions.md`'s spike entry for what this measures and what it
//! doesn't. Deliberately `#[cfg(test)]`-only: nothing outside this module's own tests ever
//! calls it, matching that it proves an equivalence rather than shipping a real feature.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use gpui_base::dock::{InsertTarget, NodeId, PaneRef, PaneTree, PanelId, RootKind};
use gpui_base::Placement;

use crate::work_surface::state::TabRef;

/// `PanelId` wraps a real panel entity's `EntityId` in production use (`gpui_base::dock::panel`'s
/// own docs) - this prototype renders nothing, so a stable hash of the `TabRef` itself stands in.
fn panel_id_for(tab: &TabRef) -> PanelId {
    let mut hasher = DefaultHasher::new();
    tab.hash(&mut hasher);
    PanelId::from_u64(hasher.finish())
}

/// Builds a single, unsplit tab strip - never a real multi-pane dock, since Jerry's own strip
/// never splits. `RootKind::Any` is required: `RootKind::Split` pins a `Split` wrapper in place
/// even when normalization would otherwise collapse it to the bare `Tabs` node this needs.
///
/// The first panel has nowhere to land yet, so it goes in via `split()` (beside the tree's own
/// empty root, which normalization then collapses down to just the new `Tabs` group). Every
/// later panel is appended into that same, now-known, group via `insert_panel` - calling
/// `split()` again would instead open a second, sibling tab group each time.
fn build_strip(order: &[TabRef]) -> PaneTree {
    let mut tree = PaneTree::new(RootKind::Any);
    let mut tabs_node = None;
    for tab in order {
        match tabs_node {
            None => {
                let root_id = tree.root().id();
                tree.split(root_id, panel_id_for(tab), Placement::Right, None);
                tabs_node = Some(tree.root().id());
            }
            Some(node) => {
                tree.insert_panel(
                    panel_id_for(tab),
                    InsertTarget::Tabs {
                        node,
                        ix: None,
                        activate: false,
                    },
                );
            }
        }
    }
    tree
}

/// The strip's one `Tabs` node - `build_strip` never leaves a `Split` behind once a tab exists,
/// so this is always the tree's own root.
fn strip_node_id(tree: &PaneTree) -> NodeId {
    tree.root().id()
}

fn strip_order(tree: &PaneTree) -> Vec<PanelId> {
    match tree.root().kind() {
        PaneRef::Tabs { panels, .. } => panels.to_vec(),
        PaneRef::Split { .. } => Vec::new(),
    }
}

/// Mirrors `crate::work_surface::state::move_tab_order`'s own contract: drag `dragged` beside
/// `target`, landing after it when `insert_after`. A no-op (dock's own `EditResult::changed`)
/// for the same cases `move_tab_order` silently ignores.
///
/// Dropping a tab on itself needs its own guard: `move_tab_order` special-cases it, but dock's
/// `move_panel` does not - fed `dragged == target, insert_after: true`, it happily detaches and
/// reinserts one slot over, which is a real behavior change from today's no-op. A host adopting
/// dock keeps this guard; dock does not supply it.
fn reorder(tree: &mut PaneTree, dragged: &TabRef, target: &TabRef, insert_after: bool) -> bool {
    if dragged == target {
        return false;
    }
    let node = strip_node_id(tree);
    let dragged_id = panel_id_for(dragged);
    // `move_tab_order` finds `target`'s index only *after* removing `dragged` from the order -
    // otherwise a drop immediately after `dragged`'s own current neighbour is off by one, since
    // `move_panel` itself detaches before it inserts. Simulating that removal here, rather than
    // indexing the still-`dragged`-inclusive list `strip_order` returns, is what keeps this in
    // step with `move_tab_order`'s own contract.
    let mut panels = strip_order(tree);
    let Some(from) = panels.iter().position(|panel| *panel == dragged_id) else {
        return false;
    };
    panels.remove(from);
    let Some(target_ix) = panels
        .iter()
        .position(|panel| *panel == panel_id_for(target))
    else {
        return false;
    };
    let ix = if insert_after {
        target_ix + 1
    } else {
        target_ix
    };
    tree.move_panel(
        dragged_id,
        InsertTarget::Tabs {
            node,
            ix: Some(ix),
            activate: false,
        },
    )
    .changed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_surface::agents::AgentId;
    use crate::work_surface::state::move_tab_order;

    fn sample_order() -> Vec<TabRef> {
        vec![
            TabRef::Agent(1 as AgentId),
            TabRef::File("a.rs".into()),
            TabRef::Agent(2 as AgentId),
            TabRef::Graph,
        ]
    }

    #[test]
    fn build_strip_preserves_insertion_order() {
        let order = sample_order();
        let tree = build_strip(&order);
        let expected: Vec<PanelId> = order.iter().map(panel_id_for).collect();
        assert_eq!(strip_order(&tree), expected);
    }

    /// The real equivalence check: for every reorder this sample strip can perform, dock's
    /// `move_panel` and the hand-rolled `move_tab_order` land on the same final order.
    #[test]
    fn reorder_matches_move_tab_order_for_every_pair() {
        let order = sample_order();
        for dragged in &order {
            for target in &order {
                for insert_after in [false, true] {
                    let mut tree = build_strip(&order);
                    reorder(&mut tree, dragged, target, insert_after);

                    let mut expected = order.clone();
                    move_tab_order(&mut expected, dragged, target, insert_after);
                    let expected: Vec<PanelId> = expected.iter().map(panel_id_for).collect();

                    assert_eq!(
                        strip_order(&tree),
                        expected,
                        "dragged={dragged:?} target={target:?} insert_after={insert_after}"
                    );
                }
            }
        }
    }

    #[test]
    fn reorder_is_a_no_op_for_an_unknown_target() {
        let order = sample_order();
        let mut tree = build_strip(&order);
        let unknown = TabRef::Run;
        assert!(!reorder(&mut tree, &order[0], &unknown, false));
        let expected: Vec<PanelId> = order.iter().map(panel_id_for).collect();
        assert_eq!(strip_order(&tree), expected);
    }
}
