//! Real per-process CPU%/memory sampling for this app's own spawned agent/shell processes - the
//! status bar's `N agents · X% cpu · Y GB` cluster, and (per GitHub issue #283) the Resources
//! popover that attributes that load repo → worktree → agent.
//!
//! One shared API, three real OS backends, selected at compile time by `target_os`:
//!
//! | platform | CPU time | resident memory | module |
//! |---|---|---|---|
//! | Linux/WSL2 | `/proc/<pid>/stat` `utime`+`stime` (clock ticks) | `/proc/<pid>/status` `VmRSS` | [`linux`] |
//! | macOS | `proc_pid_rusage` `ri_user_time`+`ri_system_time` (mach timebase units) | `ri_resident_size` | [`macos`] |
//! | Windows | `GetProcessTimes` kernel+user (100ns `FILETIME`) | PSAPI `GetProcessMemoryInfo` `WorkingSetSize` | [`windows`] |
//! | anything else | - | - | [`unsupported`] |
//!
//! Every backend implements [`ProcessSampler`], whose two readings are deliberately in
//! *platform-neutral units* - a [`Duration`] of cumulative CPU time and a byte count - rather
//! than each platform's own raw counter. That is what makes a number mean the same thing on
//! every OS: the tick/nanosecond/100-nanosecond conversion happens once, inside the backend that
//! owns that unit, and everything above [`ProcessSampler`] ([`sample_processes`],
//! [`cpu_percent_from_delta`], [`normalize_cpu_percent`], [`aggregate_process_stats`]) is a
//! single, shared, platform-free implementation - not three parallel ones that could drift.
//!
//! ## Honest `None`, everywhere, for the same reasons
//!
//! Every reading is `Option`: a pid that has exited, a process this app may not query, a kernel
//! call that simply failed. None of those are ever papered over with a fabricated `0`. See
//! [`aggregate_process_stats`] for the one case where a `None` really does reach the screen as
//! `...`, and why a *single* unsampleable agent must never blank the whole cluster.
//!
//! [`unsupported`] is the deliberate exception to "`None` is fine": a platform with no backend
//! at all would show `...% cpu` *forever*, which reads as broken rather than as transient. That
//! is what [`PLATFORM_SAMPLING_SUPPORTED`] is for - `crate::status_bar::render` omits the
//! cpu/memory fields entirely on such a build, showing just the real agent count, rather than a
//! placeholder that can never resolve. Linux, macOS and Windows are all genuinely supported, so
//! in practice that path only covers the remaining targets this workspace can be built for at
//! all (FreeBSD, via `gpui_linux`'s own `cfg` - see `crates/app/Cargo.toml`).
//!
//! ## WSL
//!
//! An agent hosted inside a WSL2 distro is a real Linux process seen by a real Linux build of
//! this app running inside that distro, so it keeps using [`linux`]'s `/proc` path unchanged.
//! [`windows`] covers *native* Windows processes spawned by a native Windows build. Nothing
//! reaches across that boundary, and nothing needs to.
//!
//! ## This measures each named process only, not its real children - on every platform
//!
//! Every backend reads exactly the pid it is given. A real child or grandchild process an agent
//! spawned (a tool call, an MCP server, a search subprocess) is its own pid with its own
//! counters, never rolled up into its parent's here - `/proc/<pid>/stat`'s `utime`/`stime`,
//! `proc_pid_rusage`'s `ri_user_time`/`ri_system_time` and `GetProcessTimes`' kernel/user times
//! are all strictly per-process. Agent CLIs routinely shell out to real subprocesses, so this
//! can materially understate an agent's genuine total CPU usage - a parent process pinned at
//! `~0%` while three real children it spawned are each pinning a core is a real, observed shape,
//! not a hypothetical.
//!
//! That limitation is stated once, here, on purpose: issue #283 requires it to hold *uniformly*
//! across platforms rather than be fixed on one, which would silently make the same status-bar
//! row mean different things depending on the OS. Rolling children in (a real process-tree walk,
//! or a Windows job object, or a macOS `proc_listchildpids`) is future work that must land on
//! all three backends together.
//!
//! ## GPUI-free and stateless
//!
//! [`sample_processes`] takes the previous tick's raw samples in and returns the next tick's raw
//! samples out, rather than owning any state itself, so `crate::rail::render`'s existing
//! `start_status_polling` loop can thread it through the poll it already runs (see that
//! function's docs) without a second, independent polling mechanism. Issue #283's "sampling
//! stays on the existing rail status-poll cadence; no new timers" is a property of that call
//! site, and this module's shape is what keeps it true.
//!
//! ## Why CPU% needs two samples
//!
//! Every platform's per-process CPU counter is *cumulative* time consumed since the process
//! started, not an instantaneous rate. A real CPU percentage needs a delta of that cumulative
//! count over a real wall-clock delta, which means every pid's very first sample - right after
//! it starts being tracked - has no prior sample to diff against and honestly reports
//! `cpu_percent: None`, not a fabricated `0%`.

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
///
/// A `const`, not a `cfg!` re-derived at each call site: `crate::status_bar::render` uses it to
/// decide whether to render the cpu/memory fields *at all* (see the module docs), and that
/// decision must follow the backend selection above automatically rather than being a second,
/// hand-maintained `cfg` predicate that could drift out of step with it the next time a
/// platform is added.
pub use backend::SUPPORTED as PLATFORM_SAMPLING_SUPPORTED;

/// The single sampler value this build uses - see [`PlatformSampler`]. Every backend's `Sampler`
/// is a stateless unit struct (all per-tick state lives in the [`RawCpuSample`] map the caller
/// threads through [`sample_processes`]), so this is a `const` rather than something that has to
/// be constructed and kept alive.
pub const PLATFORM_SAMPLER: PlatformSampler = PlatformSampler;

/// One OS's real per-process readings, in platform-neutral units.
///
/// Implementors are the per-`target_os` backends in this module's submodules; exactly one is
/// compiled into any given build. Both methods return `None` honestly - see the module docs -
/// and both read exactly the pid they are given, never its children (also the module docs, where
/// that limitation is documented once for all platforms).
///
/// Performs real, blocking OS calls (`/proc` reads on Linux, `proc_pid_rusage` on macOS,
/// `OpenProcess` + PSAPI on Windows); callers must offload this to a background executor, per
/// this crate's usual convention.
///
/// ## Two methods, not one combined reading
///
/// The two are separate calls even though macOS could return both from a single
/// `proc_pid_rusage` (and Windows could reuse one `OpenProcess` handle for both), so a full
/// sample costs one extra OS call per pid on those two platforms. That is a deliberate trade:
/// the split is what lets the "CPU time is required, memory is optional" rule live in the shared
/// [`sample_processes_with`] rather than being restated inside each backend's return type, and
/// the cost is a syscall or two per agent per *multi-second* status poll, on a background
/// executor - genuinely negligible against the clarity. The two readings are therefore taken
/// microseconds apart rather than atomically, which no consumer of a status-bar summary can
/// observe.
pub trait ProcessSampler {
    /// A short name for the backend, for tests and diagnostics - `"linux"`, `"macos"`,
    /// `"windows"`, `"unsupported"`.
    fn backend_name(&self) -> &'static str;

    /// Total CPU time (user + system) this pid has consumed since it started, converted from
    /// whatever unit the platform's own counter uses. Cumulative, not a rate - see the module
    /// docs on why a percentage needs two of these.
    ///
    /// `None` if the process no longer exists, cannot be queried, or the reading could not be
    /// parsed. A pid whose CPU time cannot be read at all is dropped from the sample entirely by
    /// [`sample_processes`], since without it there is nothing to attribute memory to either.
    fn cpu_time(&self, pid: u32) -> Option<Duration>;

    /// This pid's resident physical memory in bytes: `VmRSS` on Linux, `ri_resident_size` on
    /// macOS, the working-set size on Windows. All three are "physical memory currently held by
    /// this process", which is the number the status bar's `Y GB` field means.
    ///
    /// `None` if the process no longer exists or the reading is genuinely unavailable (a real
    /// Linux zombie's `/proc/<pid>/status` has no `VmRSS` line at all, for instance) - never a
    /// fabricated zero.
    fn resident_bytes(&self, pid: u32) -> Option<u64>;
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
///
/// Shared across every platform on purpose: this and [`cpu_percent_from_delta`] together are the
/// entire definition of "what the number means", and issue #283's requirement that the same
/// figure mean the same thing on Linux, macOS and Windows is satisfied by there being literally
/// one implementation of it, fed by backends that have already converted to a common unit.
pub fn normalize_cpu_percent(raw_percent: f32, cores: usize) -> f32 {
    raw_percent / cores.max(1) as f32
}

/// Pure math: a CPU percentage from a real CPU-time delta over a real wall-clock delta. `None`
/// for a non-positive wall delta (can't divide by zero, and a real [`Instant`] delta is never
/// negative in practice, but this stays defensive rather than producing `inf`/`NaN`).
///
/// `100%` means "one core, fully busy, for the whole wall interval", identically on every
/// platform - the backends having already converted their own counters into real CPU
/// [`Duration`]s is what makes that true without any per-platform scaling here.
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
///
/// Performs blocking OS calls; callers must offload this to a background executor, per this
/// crate's usual convention.
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
///
/// Generic over the sampler rather than calling the platform backend directly, so this shared
/// logic - the delta, the first-sample-is-`None` rule, the drop-on-exit rule - is genuinely
/// testable against a deterministic fake on any machine, instead of only being exercised on
/// whichever OS the test happens to run on. Every real call site uses [`sample_processes`].
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
///
/// An empty `pids` list is a real, honest `(Some(0.0), Some(0))` (the sum over zero processes is
/// genuinely zero, not "unknown"). For a non-empty list, each field sums whatever is genuinely
/// known across `pids` and simply skips a pid that currently can't be read for that field - a
/// pid missing from `stats` entirely (never sampled yet), or present but with that one field
/// still `None` (e.g. a zombie whose `/proc/<pid>/status` has no `VmRSS` line, or its very first
/// CPU reading with no prior sample to diff against, per [`sample_processes`]'s docs), contributes
/// nothing to that field's total rather than nullifying every other pid's real, known
/// contribution. A single un-sampleable agent must never blank the whole cluster for every other
/// agent that *can* be read - that was routine, not rare: a pty child kept alive during its EOF
/// poll (see `crate::terminal::pane`'s `MAX_EOF_POLL_TICKS`) is a real zombie for up to ~10s, and
/// a freshly-opened agent's first CPU reading has no delta to compute a rate from for a full poll
/// interval.
///
/// A field is only `None` when *nothing at all* is known about it yet - not one pid in `pids`
/// has ever contributed a real sample for that field. That is the one case a `None`/`...`
/// placeholder is honest: "no agent has ever been successfully sampled at all", not "at least
/// one agent currently can't be". Issue #283's "unsampleable states still render honestly, but
/// only for genuinely transient gaps, not as a platform's permanent condition" is exactly this
/// rule plus [`PLATFORM_SAMPLING_SUPPORTED`] covering the permanent case separately.
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
        // One real CPU-second over a 2-second wall delta is 50%.
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

    /// A multi-threaded process genuinely can burn more CPU-seconds than wall-seconds, and that
    /// is reported as the real >100% figure it is - [`normalize_cpu_percent`] is what turns it
    /// into a share of total system capacity, not this.
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

    /// The platform-neutral-unit contract, tested as arithmetic rather than as prose: the same
    /// real CPU-time delta produces the same percentage whatever backend produced it, which is
    /// the whole point of [`ProcessSampler`] returning [`Duration`] rather than ticks/`FILETIME`
    /// units/nanoseconds.
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

    /// This build really did select exactly one backend, and it is the one this `target_os`
    /// should have - a compile-time-only guarantee otherwise, asserted here so a future platform
    /// addition that mis-wires the `cfg` cascade above fails a test rather than silently
    /// sampling nothing.
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

    /// The shared delta pipeline, driven by a scripted backend rather than a real OS: a pid's
    /// first sample honestly has no percentage, and its second reports the real rate implied by
    /// the CPU-time delta.
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

    /// A cumulative counter never goes backwards in practice, but a pid *can* be reused by the
    /// OS after the process that owned it exited, which would present a smaller reading than the
    /// one carried forward. That must produce a real `0%`, never a wrapped, astronomically large
    /// percentage.
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

    /// A pid whose CPU reading fails entirely is dropped from both returned maps - it must not
    /// carry a stale sample forward, and it must not appear with a fabricated zero either.
    #[test]
    fn sample_processes_with_drops_a_pid_whose_cpu_reading_fails() {
        let sampler = ScriptedSampler::new(vec![(1, vec![(None, Some(1024))])]);
        let (samples, raw) = sample_processes_with(&sampler, &[1], HashMap::new());
        assert!(samples.is_empty());
        assert!(raw.is_empty());
    }

    /// A pid with a readable CPU time but no readable memory (a real Linux zombie's missing
    /// `VmRSS`, a Windows process whose working set can't be queried) is still a real sample -
    /// just one with an honest `None` in the memory half.
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

    /// Real, end-to-end sampling of this very test process through whatever backend this build
    /// selected - the one place the platform backend itself is exercised generically. Every
    /// supported platform can read its *own* process, so this is a genuine cross-platform
    /// assertion rather than a Linux-only one.
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

    /// Spawns a real, short-lived CPU-heavy child and takes two real samples across a real sleep
    /// through the *platform* backend, proving the end-to-end real OS read -> delta ->
    /// percentage pipeline reports genuine, non-zero CPU usage for a process that is genuinely
    /// using CPU - not a mocked or hand-computed number.
    ///
    /// Unix-only because the busy child is `yes`; the pipeline above it is platform-free and is
    /// covered everywhere by the [`ScriptedSampler`] tests.
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

    /// A real zombie (or any pid whose CPU time reads but whose memory reading doesn't) has a
    /// genuine [`ProcessSample`] entry with `resident_bytes: None` specifically - distinct from a
    /// pid missing from `stats` entirely, but must behave the same way: its own unknown field
    /// contributes nothing, without blanking a sibling pid's real, known value.
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

    /// The same pid listed twice contributes once, not twice - otherwise one process seen through
    /// two views would silently double the whole cluster's reported cpu and memory.
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
