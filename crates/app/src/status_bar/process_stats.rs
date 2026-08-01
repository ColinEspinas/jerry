//! Real per-process CPU%/memory sampling for this app's own spawned agent/shell processes, via
//! `/proc/<pid>/stat` (CPU ticks) and `/proc/<pid>/status` (`VmRSS`) - the status bar's `N
//! agents · X% cpu · Y GB` cluster. Linux/WSL2-only, matching this app's current platform scope
//! (`crate::env_info`'s own docs) - cross-platform sampling (Revision R11) is explicitly out of
//! scope here.
//!
//! [`read_cpu_ticks`]/[`read_rss_bytes`] - the only real `/proc` I/O in this module - are gated
//! behind `#[cfg(target_os = "linux")]`, with an honest, always-`None` fallback body on every
//! other platform, rather than a build that reads `/proc` unconditionally and simply fails every
//! time on a platform where it doesn't exist. `crate::status_bar::render` matches this by omitting
//! the whole CPU/memory cluster on a non-Linux build (`#[cfg(not(target_os = "linux"))]`) instead
//! of rendering a perpetual `...% cpu` placeholder that would never resolve - this codebase's
//! established "honestly omit rather than show a fake/perpetually-broken-looking placeholder"
//! discipline, applied to a whole platform rather than a single not-yet-ready value.
//!
//! GPUI-free and stateless at the function level: [`sample_processes`] takes the previous tick's
//! raw samples in and returns the next tick's raw samples out, rather than owning any state
//! itself, so `crate::root::AdeApp::start_status_polling` can thread it through its own existing
//! periodic loop (see that function's docs) without a second, independent polling mechanism.
//!
//! ## Why CPU% needs two samples
//!
//! `/proc/<pid>/stat`'s `utime`/`stime` fields are cumulative CPU ticks since the process
//! started, not an instantaneous rate. A real CPU percentage needs a delta of that cumulative
//! count over a real wall-clock delta, which means every pid's very first sample - right after
//! it starts being tracked - has no prior sample to diff against and honestly reports
//! `cpu_percent: None`, not a fabricated `0%`.
//!
//! ## The `100` clock-tick-rate constant
//!
//! Converting a tick delta into CPU-seconds needs the kernel's `USER_HZ` (`sysconf(_SC_CLK_TCK)`
//! in C). This crate has no `libc` dependency to call `sysconf` with, and glibc/Linux hardcodes
//! `USER_HZ` to 100 on every mainstream architecture (x86, x86_64, arm, aarch64) - verified on
//! this project's own dev machine via `getconf CLK_TCK`. [`LINUX_CLOCK_TICKS_PER_SECOND`] pins
//! that well-known value rather than pulling in a dependency for one syscall.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The standard Linux/glibc `USER_HZ` clock tick rate - see the module docs for why this is a
/// documented constant rather than a real `sysconf(_SC_CLK_TCK)` call.
pub const LINUX_CLOCK_TICKS_PER_SECOND: u64 = 100;

/// Reads `/proc/<pid>/stat` and returns the sum of `utime` + `stime` (fields 14 and 15, in
/// clock ticks) - the process's total CPU time consumed since it started. `None` if the process
/// no longer exists or the file couldn't be parsed.
///
/// The `comm` field (field 2) is parenthesized and may itself contain spaces or parentheses, so
/// this locates the *last* `)` and parses every field after it by position, rather than naively
/// splitting the whole line on whitespace - the same class of care `wt_core::diff`'s own parser
/// applies to unpredictable real-world content.
///
/// ## This measures the named process only, not its real children
///
/// `utime`/`stime` are this one pid's own ticks - a real child or grandchild process it spawned
/// (a tool call, an MCP server, a search subprocess) has its own separate `/proc/<pid>/stat`
/// entry with its own ticks, never rolled up into its parent's here. Agent CLIs routinely shell
/// out to real subprocesses, so this can materially understate an agent's genuine total CPU
/// usage - a parent process pinned at `~0%` while three real children it spawned are each
/// pinning a CPU core is a real, observed shape, not a hypothetical. Walking `/proc/<pid>/task/*`
/// and/or tracking real child pids to roll their ticks into the parent's total is future work,
/// left out of this fix round to keep it small and clean given the existing single-pid shape
/// here; this doc comment exists so that limitation is at least honestly documented rather than
/// silently assumed away.
#[cfg(target_os = "linux")]
pub fn read_cpu_ticks(pid: u32) -> Option<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let content = std::fs::read_to_string(path).ok()?;
    let after_comm = content.rfind(')')?;
    let rest = &content[after_comm + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `fields[0]` is field 3 (state); utime is field 14 -> fields[11], stime is field 15 ->
    // fields[12].
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

/// `/proc` doesn't exist outside Linux - honestly `None` (real cross-platform sampling is
/// Revision R11, a separate, already-tracked phase) rather than a build that would try to read a
/// path that can never exist on this platform.
#[cfg(not(target_os = "linux"))]
pub fn read_cpu_ticks(_pid: u32) -> Option<u64> {
    None
}

/// Reads `/proc/<pid>/status`'s `VmRSS` line and returns it in bytes (the file reports kB).
/// `None` if the process no longer exists, the file has no `VmRSS` line (e.g. a zombie), or it
/// couldn't be parsed.
#[cfg(target_os = "linux")]
pub fn read_rss_bytes(pid: u32) -> Option<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/status"));
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

/// See [`read_cpu_ticks`]'s non-Linux fallback docs - same reasoning.
#[cfg(not(target_os = "linux"))]
pub fn read_rss_bytes(_pid: u32) -> Option<u64> {
    None
}

/// Normalizes a raw, per-process-ticks-summed CPU percentage to a real 0-100%-of-total-system-
/// capacity scale, by dividing by `cores` (the real core count -
/// `std::thread::available_parallelism()` at the call site). Without this, both
/// [`cpu_percent_from_delta`] and (therefore) [`aggregate_process_stats`]'s summed total report a
/// raw, per-core-summed figure that can itself exceed 100% for a single busy multi-threaded
/// process, and sum well past 100% across several busy processes; the status bar's mockup
/// fixture shows single figures like `41% cpu`, implying this normalized, real
/// 0-100%-of-system-capacity scale rather than raw ticks. `cores` is clamped to at least 1 (a
/// real system always has at least one, but this stays defensive against a `0` caller-supplied
/// value rather than dividing by zero).
pub fn normalize_cpu_percent(raw_percent: f32, cores: usize) -> f32 {
    raw_percent / cores.max(1) as f32
}

/// Pure math: a CPU percentage from a real tick delta over a real wall-clock delta. `None` for a
/// non-positive wall delta (can't divide by zero, and a real [`Instant`] delta is never negative
/// in practice, but this stays defensive rather than producing `inf`/`NaN`).
pub fn cpu_percent_from_delta(tick_delta: u64, wall: Duration) -> Option<f32> {
    let wall_secs = wall.as_secs_f64();
    if wall_secs <= 0.0 {
        return None;
    }
    let cpu_seconds = tick_delta as f64 / LINUX_CLOCK_TICKS_PER_SECOND as f64;
    Some(((cpu_seconds / wall_secs) * 100.0) as f32)
}

/// One process's real, current sample - `cpu_percent` is `None` until a second sample has been
/// taken for this pid (see the module docs), `rss_bytes` is `None` only if `/proc/<pid>/status`
/// couldn't be read at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessSample {
    pub cpu_percent: Option<f32>,
    pub rss_bytes: Option<u64>,
}

/// One pid's raw CPU-tick reading and the real instant it was taken, threaded from one
/// [`sample_processes`] call to the next by the caller (see the module docs). Not `pub` fields:
/// callers only ever pass this back in and out, never construct or inspect one directly.
#[derive(Debug, Clone, Copy)]
pub struct RawCpuSample {
    ticks: u64,
    at: Instant,
}

/// Samples every pid in `pids` for real, current [`ProcessSample`]s, diffing against
/// `previous`'s raw ticks (if any) to compute `cpu_percent`. Returns the new sample map plus the
/// raw state to pass back in as `previous` on the next call - a pid whose `/proc/<pid>/stat`
/// read fails (the process has exited since it was last seen live) is silently dropped from both
/// returned maps rather than carrying a stale sample forward.
///
/// Performs blocking filesystem I/O (real `/proc` reads); callers must offload this to a
/// background executor, per this crate's usual convention.
pub fn sample_processes(
    pids: &[u32],
    previous: HashMap<u32, RawCpuSample>,
) -> (HashMap<u32, ProcessSample>, HashMap<u32, RawCpuSample>) {
    let mut samples = HashMap::with_capacity(pids.len());
    let mut raw = HashMap::with_capacity(pids.len());

    for &pid in pids {
        let Some(ticks) = read_cpu_ticks(pid) else {
            continue;
        };
        let now = Instant::now();
        let cpu_percent = previous.get(&pid).and_then(|prev| {
            let tick_delta = ticks.saturating_sub(prev.ticks);
            cpu_percent_from_delta(tick_delta, now.duration_since(prev.at))
        });
        let rss_bytes = read_rss_bytes(pid);

        samples.insert(
            pid,
            ProcessSample {
                cpu_percent,
                rss_bytes,
            },
        );
        raw.insert(pid, RawCpuSample { ticks, at: now });
    }

    (samples, raw)
}

/// Aggregates real per-pid [`ProcessSample`]s across `pids` into a total CPU% and total RSS
/// bytes - the status bar's `X% cpu · Y GB` summary across every currently open agent's
/// real pid.
///
/// An empty `pids` list is a real, honest `(Some(0.0), Some(0))` (the sum over zero processes is
/// genuinely zero, not "unknown"). For a non-empty list, each field sums whatever is genuinely
/// known across `pids` and simply skips a pid that currently can't be read for that field - a
/// pid missing from `stats` entirely (never sampled yet), or present but with that one field
/// still `None` (e.g. a zombie whose `/proc/<pid>/status` has no `VmRSS` line, or its very first
/// CPU tick with no prior sample to diff against, per [`sample_processes`]'s docs), contributes
/// nothing to that field's total rather than nullifying every other pid's real, known
/// contribution. A single un-sampleable agent must never blank the whole cluster for every other
/// agent that *can* be read - that was routine, not rare: a pty child kept alive during its EOF
/// poll (see `crate::terminal::pane`'s `MAX_EOF_POLL_TICKS`) is a real zombie for up to ~10s, and
/// a freshly-opened agent's first CPU tick has no delta to compute a rate from for a full poll
/// interval.
///
/// A field is only `None` when *nothing at all* is known about it yet - not one pid in `pids`
/// has ever contributed a real sample for that field. That is the one case a `None`/`...`
/// placeholder is honest: "no agent has ever been successfully sampled at all", not "at least
/// one agent currently can't be".
pub fn aggregate_process_stats(
    pids: &[u32],
    stats: &HashMap<u32, ProcessSample>,
) -> (Option<f32>, Option<u64>) {
    if pids.is_empty() {
        return (Some(0.0), Some(0));
    }

    let mut cpu_total = 0.0f32;
    let mut cpu_known_any = false;
    let mut rss_total = 0u64;
    let mut rss_known_any = false;

    for pid in pids {
        let Some(sample) = stats.get(pid) else {
            continue;
        };
        if let Some(cpu) = sample.cpu_percent {
            cpu_total += cpu;
            cpu_known_any = true;
        }
        if let Some(rss) = sample.rss_bytes {
            rss_total = rss_total.saturating_add(rss);
            rss_known_any = true;
        }
    }

    (
        cpu_known_any.then_some(cpu_total),
        rss_known_any.then_some(rss_total),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    #[test]
    fn cpu_percent_from_delta_computes_a_real_percentage() {
        // 100 ticks == 1 real CPU-second at the pinned 100Hz rate; over a 2-second wall delta
        // that's 50%.
        let percent = cpu_percent_from_delta(100, Duration::from_secs(2)).expect("some percent");
        assert!((percent - 50.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn cpu_percent_from_delta_at_full_utilization_is_one_hundred() {
        let percent = cpu_percent_from_delta(100, Duration::from_secs(1)).expect("some percent");
        assert!((percent - 100.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn cpu_percent_from_delta_with_zero_wall_time_is_none() {
        assert_eq!(cpu_percent_from_delta(50, Duration::ZERO), None);
    }

    #[test]
    fn cpu_percent_from_delta_with_zero_tick_delta_is_zero_percent() {
        let percent =
            cpu_percent_from_delta(0, Duration::from_secs(1)).expect("some percent, just zero");
        assert!((percent - 0.0).abs() < 0.01);
    }

    #[test]
    fn normalize_cpu_percent_divides_by_the_real_core_count() {
        // Two genuinely busy real agent processes on a 4-core machine summing to a raw 196% -
        // the exact shape the audit flagged - normalizes to 49% of total real system capacity.
        let normalized = normalize_cpu_percent(196.0, 4);
        assert!((normalized - 49.0).abs() < 0.01, "got {normalized}");
    }

    #[test]
    fn normalize_cpu_percent_on_a_single_core_machine_is_unchanged() {
        let normalized = normalize_cpu_percent(41.0, 1);
        assert!((normalized - 41.0).abs() < 0.01, "got {normalized}");
    }

    #[test]
    fn normalize_cpu_percent_clamps_a_zero_core_count_to_one_rather_than_dividing_by_zero() {
        let normalized = normalize_cpu_percent(50.0, 0);
        assert!((normalized - 50.0).abs() < 0.01, "got {normalized}");
    }

    /// Real `/proc/self` reads: this test process's own pid always exists and always has a
    /// readable `stat`/`status`, so both readers should return `Some` with a sane (non-zero for
    /// RSS) value.
    #[test]
    fn reads_this_real_test_processs_own_proc_files() {
        let pid = std::process::id();
        let ticks = read_cpu_ticks(pid);
        assert!(
            ticks.is_some(),
            "this process's own /proc/<pid>/stat should be readable"
        );

        let rss = read_rss_bytes(pid).expect("this process's own VmRSS should be readable");
        assert!(
            rss > 0,
            "a running process should have non-zero RSS, got {rss}"
        );
    }

    #[test]
    fn read_cpu_ticks_on_a_nonexistent_pid_is_none() {
        // pid 1 always exists on a real Linux system (init/systemd) but is very unlikely to be
        // readable by an unprivileged test process's own /proc mount in every sandbox this runs
        // in - use an implausibly large pid instead, which simply won't exist.
        assert_eq!(read_cpu_ticks(u32::MAX), None);
        assert_eq!(read_rss_bytes(u32::MAX), None);
    }

    /// Spawns a real, short-lived CPU-heavy child (`yes`, discarding its output) and takes two
    /// real samples across a real sleep, proving the end-to-end real `/proc` read -> delta ->
    /// percentage pipeline reports genuine, non-zero CPU usage for a process that is genuinely
    /// using CPU - not a mocked or hand-computed number.
    #[test]
    fn sample_processes_reports_real_nonzero_cpu_for_a_real_busy_child() {
        let mut child: Child = Command::new("yes")
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn a real `yes` child process");
        let pid = child.id();

        let (_first, raw) = sample_processes(&[pid], HashMap::new());
        std::thread::sleep(Duration::from_millis(200));
        let (second, _raw) = sample_processes(&[pid], raw);

        let _ = child.kill();
        let _ = child.wait();

        let sample = second
            .get(&pid)
            .expect("the real busy child should still have been alive for the second sample");
        let cpu = sample
            .cpu_percent
            .expect("a second sample against a live pid must report a real cpu_percent");
        assert!(
            cpu > 0.0,
            "a genuinely CPU-busy child should show non-zero usage, got {cpu}"
        );
        assert!(
            sample.rss_bytes.is_some_and(|rss| rss > 0),
            "a live process should report real, non-zero RSS"
        );
    }

    #[test]
    fn sample_processes_drops_a_pid_that_has_already_exited() {
        let (samples, raw) = sample_processes(&[u32::MAX], HashMap::new());
        assert!(samples.is_empty());
        assert!(raw.is_empty());
    }

    #[test]
    fn aggregate_process_stats_with_no_pids_is_a_real_zero_not_unknown() {
        let stats = HashMap::new();
        assert_eq!(aggregate_process_stats(&[], &stats), (Some(0.0), Some(0)));
    }

    #[test]
    fn aggregate_process_stats_sums_known_samples() {
        let mut stats = HashMap::new();
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: Some(10.0),
                rss_bytes: Some(1000),
            },
        );
        stats.insert(
            2,
            ProcessSample {
                cpu_percent: Some(15.0),
                rss_bytes: Some(2000),
            },
        );
        let (cpu, rss) = aggregate_process_stats(&[1, 2], &stats);
        assert_eq!(cpu, Some(25.0));
        assert_eq!(rss, Some(3000));
    }

    /// The audit's exact reproduction: one un-sampleable pid (a real zombie mid-EOF-poll, or one
    /// that simply hasn't been sampled yet this tick) must not blank the aggregate for every
    /// *other* agent whose stats genuinely are known - it contributes nothing to the total, but
    /// the total itself stays real.
    #[test]
    fn aggregate_process_stats_skips_a_pid_that_cant_be_sampled_without_blanking_the_rest() {
        let mut stats = HashMap::new();
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: Some(10.0),
                rss_bytes: Some(1000),
            },
        );
        // pid 2 has no entry at all - e.g. it's a real zombie whose /proc reads failed, or it
        // hasn't been sampled yet this tick.
        let (cpu, rss) = aggregate_process_stats(&[1, 2], &stats);
        assert_eq!(
            cpu,
            Some(10.0),
            "pid 1's real, known cpu usage must not be blanked by pid 2's missing sample"
        );
        assert_eq!(
            rss,
            Some(1000),
            "pid 1's real, known memory usage must not be blanked by pid 2's missing sample"
        );
    }

    /// A real zombie (or any pid whose `/proc/<pid>/stat` reads but `/proc/<pid>/status` has no
    /// `VmRSS` line) has a genuine [`ProcessSample`] entry with `rss_bytes: None` specifically -
    /// distinct from a pid missing from `stats` entirely, but must behave the same way: its own
    /// unknown field contributes nothing, without blanking a sibling pid's real, known value.
    #[test]
    fn aggregate_process_stats_skips_a_zombie_pids_unreadable_field_without_blanking_the_rest() {
        let mut stats = HashMap::new();
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: Some(10.0),
                rss_bytes: Some(1000),
            },
        );
        // A real zombie: `/proc/<pid>/stat` still reads (so this pid has an entry at all), but
        // `/proc/<pid>/status` has no `VmRSS` line.
        stats.insert(
            2,
            ProcessSample {
                cpu_percent: Some(5.0),
                rss_bytes: None,
            },
        );
        let (cpu, rss) = aggregate_process_stats(&[1, 2], &stats);
        assert_eq!(
            cpu,
            Some(15.0),
            "both pids' real cpu usage is known and should sum normally"
        );
        assert_eq!(
            rss,
            Some(1000),
            "the zombie's unknown memory must not blank pid 1's real, known memory - it simply \
             contributes nothing to the total"
        );
    }

    #[test]
    fn aggregate_process_stats_is_none_when_no_pid_has_ever_been_sampled() {
        let stats: HashMap<u32, ProcessSample> = HashMap::new();
        // Neither pid has any entry at all - genuinely nothing is known yet, so `None` (not a
        // fabricated zero) is the honest answer here.
        let (cpu, rss) = aggregate_process_stats(&[1, 2], &stats);
        assert_eq!(cpu, None, "nothing is known about either pid yet");
        assert_eq!(rss, None, "nothing is known about either pid yet");
    }

    #[test]
    fn aggregate_process_stats_is_none_for_cpu_only_on_a_first_ever_sample() {
        let mut stats = HashMap::new();
        // A pid's first-ever sample: rss is known immediately (a single synchronous read), but
        // cpu_percent genuinely has no prior sample to diff against yet.
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: None,
                rss_bytes: Some(1000),
            },
        );
        let (cpu, rss) = aggregate_process_stats(&[1], &stats);
        assert_eq!(cpu, None);
        assert_eq!(rss, Some(1000));
    }
}
