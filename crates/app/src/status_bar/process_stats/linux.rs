//! The Linux/WSL2 [`ProcessSampler`](super::ProcessSampler) backend: `/proc/<pid>/stat` for CPU
//! ticks and `/proc/<pid>/status` for `VmRSS`.

use super::ProcessSampler;
use std::path::PathBuf;
use std::time::Duration;

/// The standard Linux/glibc `USER_HZ` clock tick rate - see the module docs.
pub const CLOCK_TICKS_PER_SECOND: u64 = 100;

/// Exactly how long one `USER_HZ` clock tick is, in nanoseconds - `1_000_000_000 / 100`, computed
/// rather than written out so it can never drift from [`CLOCK_TICKS_PER_SECOND`]. The division is
/// exact for any tick rate that divides a billion, which every real `USER_HZ` value does.
pub const NANOS_PER_CLOCK_TICK: u64 = 1_000_000_000 / CLOCK_TICKS_PER_SECOND;

/// The Linux `/proc` backend - a stateless unit struct, per [`ProcessSampler`]'s docs.
pub struct Sampler;

/// Linux has a real backend.
pub const SUPPORTED: bool = true;

impl ProcessSampler for Sampler {
    fn backend_name(&self) -> &'static str {
        "linux"
    }

    fn cpu_time(&self, pid: u32) -> Option<Duration> {
        read_cpu_ticks(pid).map(cpu_time_from_ticks)
    }

    fn resident_bytes(&self, pid: u32) -> Option<u64> {
        read_rss_bytes(pid)
    }
}

/// Exact conversion from the kernel's clock ticks to real CPU time. `saturating_mul` rather than
/// a plain multiply: a `u64` tick count large enough to overflow nanoseconds would need a process
/// to have burned ~584 years of CPU time, but saturating is free and cannot silently wrap.
pub fn cpu_time_from_ticks(ticks: u64) -> Duration {
    Duration::from_nanos(ticks.saturating_mul(NANOS_PER_CLOCK_TICK))
}

/// Reads `/proc/<pid>/stat` and returns the sum of `utime` + `stime` (fields 14 and 15, in
/// clock ticks) - the process's total CPU time consumed since it started. `None` if the process
/// no longer exists or the file couldn't be parsed.
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

/// Reads `/proc/<pid>/status`'s `VmRSS` line and returns it in bytes (the file reports kB).
/// `None` if the process no longer exists, the file has no `VmRSS` line (e.g. a zombie), or it
/// couldn't be parsed.
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

/// This machine's real total physical memory, from `/proc/meminfo`'s `MemTotal` line (reported in
/// kB). `None` if the file is unreadable or has no parseable `MemTotal` - never a fabricated
/// default, since this is the *denominator* of the Resources popover's memory meter and a guessed
/// total would make a real numerator read as a fabricated fraction.
pub fn read_total_memory_bytes() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

/// See [`super::system_memory_bytes`].
pub fn system_memory_bytes() -> Option<u64> {
    read_total_memory_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_real_machines_total_memory() {
        let total = read_total_memory_bytes().expect("/proc/meminfo MemTotal must be readable");
        let own_rss = read_rss_bytes(std::process::id()).expect("own VmRSS");
        assert!(
            total >= own_rss,
            "total physical memory ({total}) must be at least this process's own RSS ({own_rss})"
        );
    }

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

    #[test]
    fn cpu_time_from_ticks_is_the_exact_same_number_the_tick_math_produced() {
        assert_eq!(cpu_time_from_ticks(100), Duration::from_secs(1));
        assert_eq!(cpu_time_from_ticks(1), Duration::from_millis(10));
        assert_eq!(cpu_time_from_ticks(0), Duration::ZERO);
        // The old implementation's own test case: a 100-tick delta over a 2s wall interval was
        // 50%, and still is, now that the delta is taken in real time units instead.
        let percent =
            super::super::cpu_percent_from_delta(cpu_time_from_ticks(100), Duration::from_secs(2))
                .expect("some percent");
        assert!((percent - 50.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn nanos_per_clock_tick_matches_the_pinned_tick_rate() {
        assert_eq!(NANOS_PER_CLOCK_TICK * CLOCK_TICKS_PER_SECOND, 1_000_000_000);
    }

    #[test]
    fn the_sampler_reads_this_real_test_process_through_the_trait() {
        let pid = std::process::id();
        let ticks = read_cpu_ticks(pid).expect("real ticks");
        let via_trait = Sampler.cpu_time(pid).expect("real cpu time via the trait");
        // The process keeps running between the two reads, so the trait's reading is at least
        // the raw one taken just before it - never less, and never a fabricated zero.
        assert!(
            via_trait >= cpu_time_from_ticks(ticks),
            "the trait must report the same monotonic /proc reading, got {via_trait:?}"
        );
        assert!(Sampler.resident_bytes(pid).is_some_and(|rss| rss > 0));
        assert_eq!(Sampler.backend_name(), "linux");
    }
}
