//! The Resources popover's data model: the `repo → worktree → agent` cost tree behind the status
//! bar's `41% cpu · 3.4 GB` readout (GitHub issue #293,
//! `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4d).

use std::collections::HashMap;
use std::time::Duration;

use crate::rail::state as rail;
use crate::root::plural;
use crate::status_bar::process_stats::ProcessSample;
use crate::work_surface::agents::ProcessKind;

/// The group label for the app's own process - §4d's `resDefs` carries Jerry itself as a real row
/// ("41.0% and 3.40 GB across four running agents **plus Jerry itself**"), because the bar's
/// tooltip promises "what Jerry is costing this machine right now" and a total that silently
/// excluded the window, its editors and its language servers would not be that number.
pub const JERRY_GROUP_LABEL: &str = "jerry itself";

/// One agent's (or Jerry's own) real, current cost - one `tint · agent · worktree · cpu · memory`
/// row in the popover's `LIVE NOW` tree.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceRow {
    /// Which repo this row is grouped under - the rail's own `RepoGroup::repo_name`, or
    /// [`JERRY_GROUP_LABEL`].
    pub repo_name: String,
    /// The agent tab's own title, or `"Jerry"` for the app's own process.
    pub agent_label: String,
    /// Where it is working - the worktree's branch or label, or a short description of what the
    /// app's own process covers.
    pub worktree_label: String,
    /// The agent's process kind, for its tint chip. `None` for Jerry's own row, which is not an
    /// agent and deliberately does not borrow an agent's colour.
    pub kind: Option<ProcessKind>,
    /// The real OS pid this row's numbers were read from - carried so the tree can prove it never
    /// counts one process twice (see [`ResourceTree::from_rows`]).
    pub pid: u32,
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
}

/// One repo's rows plus, implicitly, its subtotal - §4d's "`LIVE NOW` tree grouped by repo with a
/// per-repo subtotal".
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceGroup {
    pub repo_name: String,
    pub rows: Vec<ResourceRow>,
}

impl ResourceGroup {
    /// This repo's real CPU subtotal - the sum of its own rows, by the same rule the whole tree
    /// uses. See [`sum_cpu`].
    pub fn cpu_percent(&self) -> Option<f32> {
        sum_cpu(self.rows.iter())
    }

    /// This repo's real memory subtotal - see [`sum_memory`].
    pub fn memory_bytes(&self) -> Option<u64> {
        sum_memory(self.rows.iter())
    }

    /// The subtotal text shown at the right of a group header: `"12.3% · 1.6 GB"`, with `...` for
    /// a field nothing is known about yet (never a fabricated `0`).
    pub fn subtotal_label(&self) -> String {
        format!(
            "{} \u{b7} {}",
            cpu_label(self.cpu_percent()),
            memory_label(self.memory_bytes())
        )
    }
}

/// The whole `repo → worktree → agent` cost tree. Group order is the caller's - which is the
/// rail's own urgency order, since `crate::status_bar::render` builds the rows by walking
/// `AdeApp::build_repo_groups`'s already-ranked groups rather than re-sorting them here.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResourceTree {
    pub groups: Vec<ResourceGroup>,
}

impl ResourceTree {
    /// Groups `rows` by [`ResourceRow::repo_name`], preserving first-seen order for both groups
    /// and rows.
    pub fn from_rows(rows: Vec<ResourceRow>) -> Self {
        let mut groups: Vec<ResourceGroup> = Vec::new();
        let mut seen_pids = std::collections::HashSet::new();
        for row in rows {
            if !seen_pids.insert(row.pid) {
                continue;
            }
            match groups
                .iter_mut()
                .find(|group| group.repo_name == row.repo_name)
            {
                Some(group) => group.rows.push(row),
                None => groups.push(ResourceGroup {
                    repo_name: row.repo_name.clone(),
                    rows: vec![row],
                }),
            }
        }
        Self { groups }
    }

    /// Every row in the tree, in group order.
    pub fn rows(&self) -> impl Iterator<Item = &ResourceRow> {
        self.groups.iter().flat_map(|group| group.rows.iter())
    }

    /// **The** total CPU% - the sum of the tree, and the only thing the bar readout is allowed to
    /// show. See this module's own docs.
    pub fn cpu_percent(&self) -> Option<f32> {
        sum_cpu(self.rows())
    }

    /// **The** total resident memory - the sum of the tree. See [`Self::cpu_percent`].
    pub fn memory_bytes(&self) -> Option<u64> {
        sum_memory(self.rows())
    }

    /// The status bar's own recessive readout: `"41% cpu · 3.4 GB"`, the sum of this tree.
    pub fn bar_readout(&self) -> String {
        agent_readout(self.cpu_percent(), self.memory_bytes())
    }
}

/// One process's cost in the window's one readout wording: `"6.2% cpu · 0.51 GB"`.
pub fn agent_readout(cpu_percent: Option<f32>, memory_bytes: Option<u64>) -> String {
    format!(
        "{} cpu \u{b7} {}",
        cpu_label(cpu_percent),
        memory_label(memory_bytes)
    )
}

/// Sums whatever CPU readings are genuinely known - `None` only when not one row has a reading at
/// all. See this module's docs on why that is not the same as `Some(0.0)`.
fn sum_cpu<'a>(rows: impl Iterator<Item = &'a ResourceRow>) -> Option<f32> {
    let mut total = 0.0f32;
    let mut known = false;
    for row in rows {
        if let Some(cpu) = row.cpu_percent {
            total += cpu;
            known = true;
        }
    }
    known.then_some(total)
}

/// Sums whatever memory readings are genuinely known - see [`sum_cpu`].
fn sum_memory<'a>(rows: impl Iterator<Item = &'a ResourceRow>) -> Option<u64> {
    let mut total = 0u64;
    let mut known = false;
    for row in rows {
        if let Some(bytes) = row.memory_bytes {
            total = total.saturating_add(bytes);
            known = true;
        }
    }
    known.then_some(total)
}

/// One pid's real sample, reduced to the two fields a row carries - `(None, None)` for a pid that
/// has never been sampled, which is honest rather than a fabricated zero.
pub fn row_sample(
    pid: u32,
    stats: &HashMap<u32, ProcessSample>,
    cores: usize,
) -> (Option<f32>, Option<u64>) {
    let Some(sample) = stats.get(&pid) else {
        return (None, None);
    };
    (
        sample
            .cpu_percent
            .map(|raw| super::process_stats::normalize_cpu_percent(raw, cores)),
        sample.resident_bytes,
    )
}

/// `"41%"`, or `"..."` when nothing is known yet. One decimal below 10% so a genuinely small but
/// non-zero agent does not render as `0%` and read as "costs nothing".
pub fn cpu_label(percent: Option<f32>) -> String {
    match percent {
        Some(percent) if percent < 10.0 => format!("{percent:.1}%"),
        Some(percent) => format!("{}%", percent.round() as i64),
        None => "...".to_string(),
    }
}

/// `"3.4 GB"`, or `"..."` when nothing is known yet - the same [`rail::format_bytes`] every other
/// byte count in the window goes through, never a second formatter.
pub fn memory_label(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) => rail::format_bytes(bytes),
        None => "...".to_string(),
    }
}

/// §4d's `loadHue()`, verbatim: "grey below 60%, amber to 85%, red above. Healthy load spends no
/// colour - same rule as §4c: amber means your work is affected."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadLevel {
    /// <= 60% - healthy, spends no attention colour.
    Neutral,
    /// > 60% and <= 85% - "your work is affected".
    Elevated,
    /// > 85%.
    Critical,
}

impl LoadLevel {
    /// The real, resolved token for this step.
    pub fn color(self) -> crate::theme::ColorToken {
        use crate::theme::status_bar;
        match self {
            LoadLevel::Neutral => status_bar::LOAD_NEUTRAL,
            LoadLevel::Elevated => status_bar::LOAD_ELEVATED,
            LoadLevel::Critical => status_bar::LOAD_CRITICAL,
        }
    }
}

/// The load step for a real 0-100 percentage. An unknown reading is [`LoadLevel::Neutral`]: a
/// number nobody has yet is never a reason to spend the attention colour.
pub fn load_level(percent: Option<f32>) -> LoadLevel {
    match percent {
        Some(percent) if percent > 85.0 => LoadLevel::Critical,
        Some(percent) if percent > 60.0 => LoadLevel::Elevated,
        _ => LoadLevel::Neutral,
    }
}

/// A meter's fill as a real 0.0-1.0 fraction of `total`, or `None` when either the numerator or
/// the denominator is genuinely unknown - which the render side draws as an empty track rather
/// than a fill against a guessed total.
pub fn meter_fraction(used: Option<u64>, total: Option<u64>) -> Option<f32> {
    let used = used?;
    let total = total?;
    if total == 0 {
        return None;
    }
    Some((used as f32 / total as f32).clamp(0.0, 1.0))
}

/// The popover's freshness line: `"Updated 8s ago"`, `"Updated 3m ago"`, `"Updated just now"`
/// under a second, and an honest `"not sampled yet"` before the first poll has ever landed.
pub fn updated_ago_label(since: Option<Duration>) -> String {
    let Some(since) = since else {
        return "not sampled yet".to_string();
    };
    let seconds = since.as_secs();
    if seconds == 0 {
        return "Updated just now".to_string();
    }
    if seconds < 60 {
        return format!(
            "Updated {} ago",
            plural::count(seconds as usize, "second", None)
        );
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!(
            "Updated {} ago",
            plural::count(minutes as usize, "minute", None)
        );
    }
    format!(
        "Updated {} ago",
        plural::count((minutes / 60) as usize, "hour", None)
    )
}

/// The disk line's left half: `"2 worktrees prunable"`. Conjugated through [`plural`], never an
/// inline ternary.
pub fn prunable_label(count: usize) -> String {
    format!("{} prunable", plural::count(count, "worktree", None))
}

/// The disk line's right half, in bytes: the sum of `known_sizes` for however many candidates
/// there really are.
pub fn prunable_total_bytes(candidate_count: usize, known_sizes: &[u64]) -> Option<u64> {
    if candidate_count == 0 {
        return Some(0);
    }
    if known_sizes.is_empty() {
        return None;
    }
    Some(known_sizes.iter().sum())
}

#[cfg(test)]
mod resources_readout_tests {
    use super::*;
    use crate::status_bar::process_stats::{aggregate_process_stats, ProcessSample};

    fn row(repo: &str, pid: u32, cpu: Option<f32>, mem: Option<u64>) -> ResourceRow {
        ResourceRow {
            repo_name: repo.to_string(),
            agent_label: format!("agent-{pid}"),
            worktree_label: "feature-x".to_string(),
            kind: Some(ProcessKind::claude()),
            pid,
            cpu_percent: cpu,
            memory_bytes: mem,
        }
    }

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn the_bar_readout_is_the_sum_of_the_tree_not_a_second_aggregate() {
        let mut rows = vec![
            row("jerry-core", 11, Some(7.8), Some(GB / 2)),
            row("jerry-core", 12, Some(6.2), Some(GB / 2)),
            row("billing-api", 13, Some(19.4), Some(GB)),
        ];
        let tree = ResourceTree::from_rows(rows.clone());

        let summed_cpu: f32 = rows.iter().filter_map(|row| row.cpu_percent).sum();
        let summed_mem: u64 = rows.iter().filter_map(|row| row.memory_bytes).sum();
        assert_eq!(tree.memory_bytes(), Some(summed_mem));
        assert!(
            (tree.cpu_percent().expect("a real total") - summed_cpu).abs() < 0.001,
            "the tree's CPU total must be the sum of its own rows"
        );
        assert_eq!(
            tree.bar_readout(),
            format!(
                "{} cpu \u{b7} {}",
                cpu_label(Some(summed_cpu)),
                memory_label(Some(summed_mem))
            ),
            "the bar readout must be formatted from that same summed total"
        );

        let before = tree.bar_readout();
        rows[2].cpu_percent = Some(59.4);
        rows[2].memory_bytes = Some(GB * 3);
        let after = ResourceTree::from_rows(rows).bar_readout();
        assert_ne!(
            before, after,
            "changing one agent's real reading must change the bar readout - if it doesn't, the \
             readout is not derived from the tree at all"
        );
    }

    #[test]
    fn every_repo_subtotal_is_the_sum_of_its_own_rows_and_they_sum_to_the_total() {
        let tree = ResourceTree::from_rows(vec![
            row("jerry-core", 11, Some(7.8), Some(GB / 2)),
            row("jerry-core", 12, Some(6.2), Some(GB / 4)),
            row("billing-api", 13, Some(19.4), Some(GB)),
        ]);
        assert_eq!(tree.groups.len(), 2, "two repos, two groups");

        let core = &tree.groups[0];
        assert!((core.cpu_percent().expect("real") - 14.0).abs() < 0.001);
        assert_eq!(core.memory_bytes(), Some(GB / 2 + GB / 4));

        let subtotal_cpu: f32 = tree
            .groups
            .iter()
            .filter_map(|group| group.cpu_percent())
            .sum();
        let subtotal_mem: u64 = tree
            .groups
            .iter()
            .filter_map(|group| group.memory_bytes())
            .sum();
        assert!((tree.cpu_percent().expect("real") - subtotal_cpu).abs() < 0.001);
        assert_eq!(tree.memory_bytes(), Some(subtotal_mem));
    }

    #[test]
    fn the_tree_total_agrees_with_aggregate_process_stats() {
        let mut stats = HashMap::new();
        stats.insert(
            11,
            ProcessSample {
                cpu_percent: Some(12.0),
                resident_bytes: Some(GB),
            },
        );
        stats.insert(
            12,
            ProcessSample {
                cpu_percent: Some(4.0),
                resident_bytes: None,
            },
        );
        stats.insert(
            13,
            ProcessSample {
                cpu_percent: None,
                resident_bytes: Some(GB / 2),
            },
        );
        let pids = [11u32, 12, 13];

        let tree = ResourceTree::from_rows(
            pids.iter()
                .map(|&pid| {
                    let (cpu, mem) = row_sample(pid, &stats, 1);
                    row("jerry-core", pid, cpu, mem)
                })
                .collect(),
        );

        let (aggregate_cpu, aggregate_mem) = aggregate_process_stats(&pids, &stats);
        assert_eq!(tree.memory_bytes(), aggregate_mem);
        assert!(
            (tree.cpu_percent().expect("real") - aggregate_cpu.expect("real")).abs() < 0.001,
            "the tree and aggregate_process_stats must agree - they are one definition"
        );
    }

    #[test]
    fn one_process_seen_twice_is_counted_once() {
        let tree = ResourceTree::from_rows(vec![
            row("jerry-core", 11, Some(10.0), Some(GB)),
            row("jerry-core", 11, Some(10.0), Some(GB)),
        ]);
        assert_eq!(tree.rows().count(), 1);
        assert_eq!(tree.memory_bytes(), Some(GB));
    }

    #[test]
    fn unknown_readings_are_none_not_zero_and_never_blank_a_known_sibling() {
        let all_unknown = ResourceTree::from_rows(vec![row("jerry-core", 11, None, None)]);
        assert_eq!(all_unknown.cpu_percent(), None);
        assert_eq!(all_unknown.memory_bytes(), None);
        assert_eq!(all_unknown.bar_readout(), "... cpu \u{b7} ...");

        let mixed = ResourceTree::from_rows(vec![
            row("jerry-core", 11, None, None),
            row("jerry-core", 12, Some(3.0), Some(GB)),
        ]);
        assert_eq!(mixed.memory_bytes(), Some(GB));
        assert!((mixed.cpu_percent().expect("real") - 3.0).abs() < 0.001);
    }

    #[test]
    fn load_thresholds_are_neutral_below_sixty_amber_to_eighty_five_red_above() {
        assert_eq!(load_level(Some(0.0)), LoadLevel::Neutral);
        assert_eq!(load_level(Some(59.9)), LoadLevel::Neutral);
        assert_eq!(load_level(Some(60.0)), LoadLevel::Neutral);
        assert_eq!(load_level(Some(60.1)), LoadLevel::Elevated);
        assert_eq!(load_level(Some(85.0)), LoadLevel::Elevated);
        assert_eq!(load_level(Some(85.1)), LoadLevel::Critical);
        assert_eq!(load_level(Some(100.0)), LoadLevel::Critical);
    }

    #[test]
    fn an_unknown_load_is_never_amber_or_red() {
        assert_eq!(load_level(None), LoadLevel::Neutral);
    }

    #[test]
    fn the_three_load_steps_resolve_to_three_distinct_colours() {
        let neutral = LoadLevel::Neutral.color().resolve();
        let elevated = LoadLevel::Elevated.color().resolve();
        let critical = LoadLevel::Critical.color().resolve();
        assert_ne!(neutral, elevated);
        assert_ne!(elevated, critical);
        assert_ne!(neutral, critical);
    }

    #[test]
    fn a_meter_never_fills_against_a_guessed_denominator() {
        assert_eq!(meter_fraction(Some(GB), None), None);
        assert_eq!(meter_fraction(None, Some(GB)), None);
        assert_eq!(meter_fraction(Some(GB), Some(0)), None);
        assert_eq!(meter_fraction(Some(GB), Some(GB * 4)), Some(0.25));
        assert_eq!(meter_fraction(Some(GB * 8), Some(GB * 4)), Some(1.0));
    }

    #[test]
    fn the_freshness_line_conjugates_every_unit() {
        assert_eq!(updated_ago_label(None), "not sampled yet");
        assert_eq!(
            updated_ago_label(Some(Duration::from_millis(400))),
            "Updated just now"
        );
        assert_eq!(
            updated_ago_label(Some(Duration::from_secs(1))),
            "Updated 1 second ago"
        );
        assert_eq!(
            updated_ago_label(Some(Duration::from_secs(8))),
            "Updated 8 seconds ago"
        );
        assert_eq!(
            updated_ago_label(Some(Duration::from_secs(60))),
            "Updated 1 minute ago"
        );
        assert_eq!(
            updated_ago_label(Some(Duration::from_secs(180))),
            "Updated 3 minutes ago"
        );
        assert_eq!(
            updated_ago_label(Some(Duration::from_secs(3600))),
            "Updated 1 hour ago"
        );
        assert_eq!(
            updated_ago_label(Some(Duration::from_secs(7200))),
            "Updated 2 hours ago"
        );
    }

    #[test]
    fn the_prunable_line_conjugates() {
        assert_eq!(prunable_label(0), "0 worktrees prunable");
        assert_eq!(prunable_label(1), "1 worktree prunable");
        assert_eq!(prunable_label(2), "2 worktrees prunable");
    }

    #[test]
    fn zero_candidates_is_a_real_zero_not_an_unread_ellipsis() {
        assert_eq!(prunable_total_bytes(0, &[]), Some(0));
        assert_eq!(memory_label(prunable_total_bytes(0, &[])), "0 B");
    }

    #[test]
    fn a_candidate_whose_size_has_not_come_back_yet_is_genuinely_unknown() {
        assert_eq!(prunable_total_bytes(2, &[]), None);
        assert_eq!(memory_label(prunable_total_bytes(2, &[])), "...");
    }

    #[test]
    fn known_candidate_sizes_sum() {
        assert_eq!(prunable_total_bytes(2, &[1_000, 2_000]), Some(3_000));
    }

    #[test]
    fn a_small_but_real_cpu_reading_keeps_a_decimal() {
        assert_eq!(cpu_label(Some(0.4)), "0.4%");
        assert_eq!(cpu_label(Some(7.8)), "7.8%");
        assert_eq!(cpu_label(Some(41.2)), "41%");
        assert_eq!(cpu_label(None), "...");
    }
}
