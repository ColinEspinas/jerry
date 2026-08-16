//! Real per-process CPU%/memory sampling for this app's own spawned agent/shell processes - the
//! status bar's `N agents · X% cpu · Y GB` cluster, and (per GitHub issue #283) the Resources
//! popover that attributes that load repo → worktree → agent.

use std::collections::HashMap;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub mod unsupported;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
use linux as backend;
#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use unsupported as backend;
#[cfg(target_os = "windows")]
use windows as backend;

/// The one [`ProcessSampler`] implementation compiled into this build, chosen by `target_os`
/// alone - there is never more than one, and never a runtime choice between them.
pub use backend::Sampler as PlatformSampler;

/// Whether this build has a real sampling backend at all - `false` only on [`unsupported`].
pub use backend::SUPPORTED as PLATFORM_SAMPLING_SUPPORTED;

/// The single sampler value this build uses - see [`PlatformSampler`]. Every backend's `Sampler`
/// is a stateless unit struct (all per-tick state lives in the [`RawCpuSample`] map the caller
/// threads through [`sample_processes`]), so this is a `const` rather than something that has to
/// be constructed and kept alive.
pub const PLATFORM_SAMPLER: PlatformSampler = PlatformSampler;

/// One OS's real per-process readings, in platform-neutral units.
pub trait ProcessSampler {
    /// A short name for the backend, for tests and diagnostics - `"linux"`, `"macos"`,
    /// `"windows"`, `"unsupported"`.
    fn backend_name(&self) -> &'static str;

    /// Total CPU time (user + system) this pid has consumed since it started, converted from
    /// whatever unit the platform's own counter uses. Cumulative, not a rate - see the module
    /// docs on why a percentage needs two of these.
    fn cpu_time(&self, pid: u32) -> Option<Duration>;

    /// This pid's resident physical memory in bytes: `VmRSS` on Linux, `ri_resident_size` on
    /// macOS, the working-set size on Windows. All three are "physical memory currently held by
    /// this process", which is the number the status bar's `Y GB` field means.
    fn resident_bytes(&self, pid: u32) -> Option<u64>;
}

/// This machine's real total physical memory in bytes, or `None` on a build with no real backend
/// ([`unsupported`]) or if the platform reading genuinely failed.
pub fn system_memory_bytes() -> Option<u64> {
    backend::system_memory_bytes()
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

/// Pure math: a CPU percentage from a real CPU-time delta over a real wall-clock delta. `None`
/// for a non-positive wall delta (can't divide by zero, and a real [`Instant`] delta is never
/// negative in practice, but this stays defensive rather than producing `inf`/`NaN`).
pub fn cpu_percent_from_delta(cpu_delta: Duration, wall: Duration) -> Option<f32> {
    let wall_secs = wall.as_secs_f64();
    if wall_secs <= 0.0 {
        return None;
    }
    Some(((cpu_delta.as_secs_f64() / wall_secs) * 100.0) as f32)
}

/// One process's real, current sample - `cpu_percent` is `None` until a second sample has been
/// taken for this pid (see the module docs), `resident_bytes` is `None` only if the platform's
/// memory reading genuinely failed for this pid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessSample {
    pub cpu_percent: Option<f32>,
    pub resident_bytes: Option<u64>,
}

/// One pid's raw cumulative CPU-time reading and the real instant it was taken, threaded from one
/// [`sample_processes`] call to the next by the caller (see the module docs). Not `pub` fields:
/// callers only ever pass this back in and out, never construct or inspect one directly.
#[derive(Debug, Clone, Copy)]
pub struct RawCpuSample {
    cpu_time: Duration,
    at: Instant,
}

/// Samples every pid in `pids` with this build's [`PLATFORM_SAMPLER`] - see
/// [`sample_processes_with`], which this is the platform-wired convenience form of.
pub fn sample_processes(
    pids: &[u32],
    previous: HashMap<u32, RawCpuSample>,
) -> (HashMap<u32, ProcessSample>, HashMap<u32, RawCpuSample>) {
    sample_processes_with(&PLATFORM_SAMPLER, pids, previous)
}

/// Samples every pid in `pids` for real, current [`ProcessSample`]s through `sampler`, diffing
/// against `previous`'s raw CPU times (if any) to compute `cpu_percent`. Returns the new sample
/// map plus the raw state to pass back in as `previous` on the next call - a pid whose CPU-time
/// reading fails (the process has exited since it was last seen live) is silently dropped from
/// both returned maps rather than carrying a stale sample forward.
pub fn sample_processes_with<S: ProcessSampler + ?Sized>(
    sampler: &S,
    pids: &[u32],
    previous: HashMap<u32, RawCpuSample>,
) -> (HashMap<u32, ProcessSample>, HashMap<u32, RawCpuSample>) {
    let mut samples = HashMap::with_capacity(pids.len());
    let mut raw = HashMap::with_capacity(pids.len());

    for &pid in pids {
        let Some(cpu_time) = sampler.cpu_time(pid) else {
            continue;
        };
        let now = Instant::now();
        let cpu_percent = previous.get(&pid).and_then(|prev| {
            let cpu_delta = cpu_time.saturating_sub(prev.cpu_time);
            cpu_percent_from_delta(cpu_delta, now.duration_since(prev.at))
        });
        let resident_bytes = sampler.resident_bytes(pid);

        samples.insert(
            pid,
            ProcessSample {
                cpu_percent,
                resident_bytes,
            },
        );
        raw.insert(pid, RawCpuSample { cpu_time, at: now });
    }

    (samples, raw)
}

/// Aggregates real per-pid [`ProcessSample`]s across `pids` into a total CPU% and total resident
/// bytes - the status bar's `X% cpu · Y GB` summary across every currently open agent's
/// real pid.
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
    // A pid is counted at most once even if `pids` lists it twice. `sample_processes` is
    // structurally immune to this (it keys a `HashMap`), but this function walks the caller's
    // slice, so a duplicate would add the same process's CPU and memory to the totals twice and
    // silently inflate the whole cluster. Today's caller collects one pid per open agent, so this
    // is a guard rather than a live bug - but "the bar readout is the sum of the tree" (issue
    // #283's acceptance criterion) has to survive two views of the same process, which is exactly
    // what the Resources popover this feeds will introduce.
    let mut counted = std::collections::HashSet::with_capacity(pids.len());

    for pid in pids {
        if !counted.insert(*pid) {
            continue;
        }
        let Some(sample) = stats.get(pid) else {
            continue;
        };
        if let Some(cpu) = sample.cpu_percent {
            cpu_total += cpu;
            cpu_known_any = true;
        }
        if let Some(rss) = sample.resident_bytes {
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
    use std::cell::RefCell;

    /// One scripted tick's answer for one pid: its CPU time and its resident memory, either of
    /// which may be a `None` modelling a reading that genuinely failed.
    type ScriptedReading = (Option<Duration>, Option<u64>);

    /// A deterministic [`ProcessSampler`] that replays a scripted sequence of readings per pid,
    /// so the shared sampling logic above ([`sample_processes_with`]'s delta, first-sample and
    /// drop-on-exit rules) is tested for real on every platform this suite runs on, not only on
    /// whichever OS happens to have a working backend here.
    struct ScriptedSampler {
        /// Per pid, the readings to hand out in order. A `None` CPU reading models a pid that
        /// has exited or can't be queried. Running past the end of a pid's script yields `None`
        /// too, which is the same thing.
        script: RefCell<HashMap<u32, std::vec::IntoIter<ScriptedReading>>>,
        /// The reading `cpu_time` most recently handed out, so `resident_bytes` (called straight
        /// after it, for the same pid, by `sample_processes_with`) returns that same reading's
        /// memory half rather than advancing the script a second time.
        current: RefCell<HashMap<u32, Option<u64>>>,
    }

    impl ScriptedSampler {
        fn new(script: Vec<(u32, Vec<ScriptedReading>)>) -> Self {
            Self {
                script: RefCell::new(
                    script
                        .into_iter()
                        .map(|(pid, readings)| (pid, readings.into_iter()))
                        .collect(),
                ),
                current: RefCell::new(HashMap::new()),
            }
        }
    }

    impl ProcessSampler for ScriptedSampler {
        fn backend_name(&self) -> &'static str {
            "scripted"
        }

        fn cpu_time(&self, pid: u32) -> Option<Duration> {
            let (cpu, rss) = self
                .script
                .borrow_mut()
                .get_mut(&pid)
                .and_then(|readings| readings.next())
                .unwrap_or((None, None));
            self.current.borrow_mut().insert(pid, rss);
            cpu
        }

        fn resident_bytes(&self, pid: u32) -> Option<u64> {
            self.current.borrow().get(&pid).copied().flatten()
        }
    }

    #[test]
    fn cpu_percent_from_delta_computes_a_real_percentage() {
        let percent = cpu_percent_from_delta(Duration::from_secs(1), Duration::from_secs(2))
            .expect("some percent");
        assert!((percent - 50.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn cpu_percent_from_delta_at_full_utilization_is_one_hundred() {
        let percent = cpu_percent_from_delta(Duration::from_secs(1), Duration::from_secs(1))
            .expect("some percent");
        assert!((percent - 100.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn cpu_percent_from_delta_with_zero_wall_time_is_none() {
        assert_eq!(
            cpu_percent_from_delta(Duration::from_millis(500), Duration::ZERO),
            None
        );
    }

    #[test]
    fn cpu_percent_from_delta_with_zero_cpu_delta_is_zero_percent() {
        let percent = cpu_percent_from_delta(Duration::ZERO, Duration::from_secs(1))
            .expect("some percent, just zero");
        assert!((percent - 0.0).abs() < 0.01);
    }

    #[test]
    fn cpu_percent_from_delta_above_one_core_is_not_clamped() {
        let percent = cpu_percent_from_delta(Duration::from_secs(3), Duration::from_secs(1))
            .expect("some percent");
        assert!((percent - 300.0).abs() < 0.01, "got {percent}");
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

    #[test]
    fn the_same_cpu_duration_means_the_same_percentage_whatever_backend_produced_it() {
        // 500ms of CPU over a 1s wall interval is 50%, whether that 500ms arrived as 50 Linux
        // clock ticks, 500_000_000 macOS nanoseconds, or 5_000_000 Windows 100ns FILETIME units.
        let from_linux_ticks = Duration::from_nanos(50 * 10_000_000);
        let from_macos_nanos = Duration::from_nanos(500_000_000);
        let from_windows_filetime = Duration::from_nanos(5_000_000 * 100);
        assert_eq!(from_linux_ticks, from_macos_nanos);
        assert_eq!(from_macos_nanos, from_windows_filetime);

        let percent =
            cpu_percent_from_delta(from_linux_ticks, Duration::from_secs(1)).expect("some percent");
        assert!((percent - 50.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn the_compiled_backend_matches_this_target_os() {
        let expected = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "unsupported"
        };
        assert_eq!(PLATFORM_SAMPLER.backend_name(), expected);
        assert_eq!(
            PLATFORM_SAMPLING_SUPPORTED,
            expected != "unsupported",
            "PLATFORM_SAMPLING_SUPPORTED must follow the selected backend, not a second cfg"
        );
    }

    #[test]
    fn sample_processes_with_reports_none_on_the_first_sample_then_a_real_rate() {
        let sampler = ScriptedSampler::new(vec![(
            7,
            vec![
                (Some(Duration::from_secs(1)), Some(4096)),
                (Some(Duration::from_secs(2)), Some(8192)),
            ],
        )]);

        let (first, raw) = sample_processes_with(&sampler, &[7], HashMap::new());
        let first = first.get(&7).copied().expect("pid 7 sampled");
        assert_eq!(
            first.cpu_percent, None,
            "a first-ever sample has nothing to diff against and must not fabricate 0%"
        );
        assert_eq!(first.resident_bytes, Some(4096));

        // A real, non-zero wall delta so the percentage below is a genuine division, not a
        // divide-by-zero `None`.
        std::thread::sleep(Duration::from_millis(20));
        let (second, _) = sample_processes_with(&sampler, &[7], raw);
        let second = second.get(&7).copied().expect("pid 7 sampled again");
        let cpu = second
            .cpu_percent
            .expect("a second sample against a live pid must report a real cpu_percent");
        assert!(
            cpu > 0.0,
            "a full extra CPU-second between samples is a genuinely non-zero rate, got {cpu}"
        );
        assert_eq!(second.resident_bytes, Some(8192));
    }

    #[test]
    fn sample_processes_with_saturates_a_backwards_cpu_reading_to_zero() {
        let sampler = ScriptedSampler::new(vec![(
            9,
            vec![
                (Some(Duration::from_secs(10)), Some(1024)),
                (Some(Duration::from_secs(1)), Some(1024)),
            ],
        )]);

        let (_, raw) = sample_processes_with(&sampler, &[9], HashMap::new());
        std::thread::sleep(Duration::from_millis(20));
        let (second, _) = sample_processes_with(&sampler, &[9], raw);
        let cpu = second
            .get(&9)
            .and_then(|sample| sample.cpu_percent)
            .expect("a second sample still reports a real percentage");
        assert!(
            (cpu - 0.0).abs() < 0.01,
            "a backwards reading saturates to a zero delta, got {cpu}"
        );
    }

    #[test]
    fn sample_processes_with_drops_a_pid_whose_cpu_reading_fails() {
        let sampler = ScriptedSampler::new(vec![(1, vec![(None, Some(1024))])]);
        let (samples, raw) = sample_processes_with(&sampler, &[1], HashMap::new());
        assert!(samples.is_empty());
        assert!(raw.is_empty());
    }

    #[test]
    fn sample_processes_with_keeps_a_pid_whose_memory_reading_alone_fails() {
        let sampler = ScriptedSampler::new(vec![(3, vec![(Some(Duration::from_secs(1)), None)])]);
        let (samples, raw) = sample_processes_with(&sampler, &[3], HashMap::new());
        assert_eq!(
            samples.get(&3).copied(),
            Some(ProcessSample {
                cpu_percent: None,
                resident_bytes: None,
            })
        );
        assert!(
            raw.contains_key(&3),
            "its CPU reading is still carried forward"
        );
    }

    #[test]
    fn the_platform_backend_reads_this_real_test_process() {
        if !PLATFORM_SAMPLING_SUPPORTED {
            // On a platform with no backend there is genuinely nothing to assert here beyond
            // the honest `None` the `unsupported` module's own tests already cover.
            return;
        }
        let pid = std::process::id();
        assert!(
            PLATFORM_SAMPLER.cpu_time(pid).is_some(),
            "this process's own CPU time should be readable on a supported platform"
        );
        let resident = PLATFORM_SAMPLER
            .resident_bytes(pid)
            .expect("this process's own resident memory should be readable");
        assert!(
            resident > 0,
            "a running process should have non-zero resident memory, got {resident}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sample_processes_reports_real_nonzero_cpu_for_a_real_busy_child() {
        use std::process::{Child, Command, Stdio};

        let mut child: Child = Command::new("yes")
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn a real `yes` child process");
        let pid = child.id();

        let (_first, raw) = sample_processes(&[pid], HashMap::new());
        std::thread::sleep(Duration::from_millis(500));
        let (second, _raw) = sample_processes(&[pid], raw);

        let _ = child.kill();
        let _ = child.wait();

        let sample = second
            .get(&pid)
            .expect("the real busy child should still have been alive for the second sample");
        let cpu = sample
            .cpu_percent
            .expect("a second sample against a live pid must report a real cpu_percent");
        // A real *rate* floor, not just `> 0.0`. `yes` pins one core, so the honest answer is
        // near 100% of one core; 25% is loose enough for a contended CI runner but still fails
        // decisively against a unit-conversion bug, which is precisely how the macOS backend's
        // mach-timebase defect (see `macos`'s module docs) once passed a `> 0.0` assertion while
        // under-reporting by ~41.7x.
        assert!(
            cpu > 25.0,
            "a core-pinning child should report close to 100% of one core, got {cpu}% - a value \
             just above zero is what a CPU-time unit bug looks like"
        );
        assert!(
            sample.resident_bytes.is_some_and(|rss| rss > 0),
            "a live process should report real, non-zero resident memory"
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
                resident_bytes: Some(1000),
            },
        );
        stats.insert(
            2,
            ProcessSample {
                cpu_percent: Some(15.0),
                resident_bytes: Some(2000),
            },
        );
        let (cpu, rss) = aggregate_process_stats(&[1, 2], &stats);
        assert_eq!(cpu, Some(25.0));
        assert_eq!(rss, Some(3000));
    }

    #[test]
    fn aggregate_process_stats_skips_a_pid_that_cant_be_sampled_without_blanking_the_rest() {
        let mut stats = HashMap::new();
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: Some(10.0),
                resident_bytes: Some(1000),
            },
        );
        // pid 2 has no entry at all - e.g. it's a real zombie whose readings failed, or it
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

    #[test]
    fn aggregate_process_stats_skips_a_zombie_pids_unreadable_field_without_blanking_the_rest() {
        let mut stats = HashMap::new();
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: Some(10.0),
                resident_bytes: Some(1000),
            },
        );
        // A real zombie: its CPU time still reads (so this pid has an entry at all), but
        // `/proc/<pid>/status` has no `VmRSS` line.
        stats.insert(
            2,
            ProcessSample {
                cpu_percent: Some(5.0),
                resident_bytes: None,
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
    fn aggregate_process_stats_counts_a_duplicated_pid_only_once() {
        let mut stats = HashMap::new();
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: Some(10.0),
                resident_bytes: Some(1000),
            },
        );
        let (cpu, rss) = aggregate_process_stats(&[1, 1, 1], &stats);
        assert_eq!(
            cpu,
            Some(10.0),
            "pid 1's cpu must not be counted three times"
        );
        assert_eq!(
            rss,
            Some(1000),
            "pid 1's memory must not be counted three times"
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
        // A pid's first-ever sample: memory is known immediately (a single synchronous read), but
        // cpu_percent genuinely has no prior sample to diff against yet.
        stats.insert(
            1,
            ProcessSample {
                cpu_percent: None,
                resident_bytes: Some(1000),
            },
        );
        let (cpu, rss) = aggregate_process_stats(&[1], &stats);
        assert_eq!(cpu, None);
        assert_eq!(rss, Some(1000));
    }
}
