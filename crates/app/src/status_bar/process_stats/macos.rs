//! The macOS [`ProcessSampler`](super::ProcessSampler) backend: one `proc_pid_rusage` call per
//! pid, for both CPU time and resident memory (GitHub issue #283).

// This module exists entirely to call libc/Mach FFI (`proc_pid_rusage`, `mach_timebase_info`) -
// every call site below carries its own `SAFETY` comment; see CLAUDE.md's Rust standards for
// the project-wide "unsafe only for justified FFI" rule this module is the macOS half of.
#![allow(unsafe_code)]

use super::ProcessSampler;
use std::time::Duration;

/// The macOS `proc_pid_rusage` backend - a stateless unit struct, per [`ProcessSampler`]'s docs.
pub struct Sampler;

/// macOS has a real backend.
pub const SUPPORTED: bool = true;

impl ProcessSampler for Sampler {
    fn backend_name(&self) -> &'static str {
        "macos"
    }

    fn cpu_time(&self, pid: u32) -> Option<Duration> {
        let info = read_rusage(pid)?;
        // Both fields are mach timebase units - see the module docs, this is NOT nanoseconds.
        // `saturating_add` because they are two independent counters and nothing in the ABI
        // promises their sum fits; it always does in practice, but wrapping here would produce a
        // wildly wrong percentage rather than a merely clamped one.
        let ticks = info.ri_user_time.saturating_add(info.ri_system_time);
        Some(Duration::from_nanos(mach_ticks_to_nanos(ticks)))
    }

    fn resident_bytes(&self, pid: u32) -> Option<u64> {
        Some(read_rusage(pid)?.ri_resident_size)
    }
}

/// This machine's real `mach_timebase_info` ratio as `(numer, denom)`: multiplying a mach
/// timebase tick by `numer / denom` gives nanoseconds. `1/1` on Intel, `125/3` on Apple Silicon.
fn mach_timebase() -> (u64, u64) {
    use std::sync::OnceLock;
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();

    *TIMEBASE.get_or_init(|| {
        #[allow(deprecated)]
        {
            // SAFETY: `mach_timebase_info` is a `repr(C)` pair of `u32`s for which an all-zero bit
            // pattern is valid; this is only an initial value.
            let mut timebase: libc::mach_timebase_info = unsafe { std::mem::zeroed() };
            // SAFETY: the callee writes exactly this struct's two `u32` fields through the
            // pointer, which addresses a live, uniquely borrowed stack value for the whole call
            // and is not retained afterwards. It returns `KERN_SUCCESS` (0) on success.
            let result = unsafe { libc::mach_timebase_info(&mut timebase) };
            if result == 0 && timebase.denom != 0 {
                (timebase.numer as u64, timebase.denom as u64)
            } else {
                (1, 1)
            }
        }
    })
}

/// Converts a mach timebase tick count into real nanoseconds using [`mach_timebase`].
fn mach_ticks_to_nanos(ticks: u64) -> u64 {
    let (numer, denom) = mach_timebase();
    if numer == denom {
        // The Intel case, and the fallback: no conversion needed, and this skips the widening
        // entirely for the overwhelmingly common `1/1` ratio.
        return ticks;
    }
    let nanos = (ticks as u128).saturating_mul(numer as u128) / (denom as u128);
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// See [`super::system_memory_bytes`] - this machine's real total physical memory, from one
/// `sysctlbyname("hw.memsize")` call.
pub fn system_memory_bytes() -> Option<u64> {
    static TOTAL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TOTAL.get_or_init(read_total_memory_bytes)
}

/// The uncached `sysctlbyname("hw.memsize")` reading behind [`system_memory_bytes`], separate so
/// the test below exercises the real call rather than a cached first result.
fn read_total_memory_bytes() -> Option<u64> {
    let name = c"hw.memsize";
    let mut value: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>();

    // SAFETY: `name` is a real, NUL-terminated C string with a lifetime covering the whole call.
    // `value` is a live, stack-owned, uniquely borrowed `u64` and `len` is initialised to exactly
    // its size, which is the contract `sysctlbyname` requires: it writes at most `len` bytes
    // through the pointer and updates `len` with what it actually wrote. The last two arguments
    // are the documented "no new value" form (null pointer, zero length), so this is a pure read.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };

    // A short write would mean the kernel answered with a narrower type than `hw.memsize`'s
    // documented `uint64_t`, leaving the high bytes of `value` as the zeros they were initialised
    // to - a silently wrong total rather than a failure, so it is rejected explicitly.
    if result != 0 || len != std::mem::size_of::<u64>() || value == 0 {
        return None;
    }
    Some(value)
}

/// One real `proc_pid_rusage(pid, RUSAGE_INFO_V0, ...)` call. `None` when the call fails - the
/// process has exited, or this app may not query it - never a zeroed struct passed off as a real
/// reading.
fn read_rusage(pid: u32) -> Option<libc::rusage_info_v0> {
    // SAFETY: `rusage_info_v0` is a `repr(C)` plain-old-data struct of integers and a byte array,
    // for which an all-zero bit pattern is a valid value; this is only an initial value, and the
    // struct is not read at all unless the call below reports success and has filled it in.
    let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };

    // SAFETY: `proc_pid_rusage` writes `sizeof(rusage_info_v0)` bytes through the buffer pointer
    // when the flavor is `RUSAGE_INFO_V0`, which is exactly the size of the live, stack-owned
    // `info` whose address is passed here; the pointer is valid, uniquely borrowed and aligned
    // for the whole call, and is not retained by the callee. The cast is the one Apple's own
    // header requires - `rusage_info_t` is a `void *` typedef and callers are expected to pass
    // the address of a concrete flavor struct through it. Every other argument is a scalar.
    //
    // The cast chain reads alarmingly (`libc` declares the parameter as `*mut rusage_info_t`,
    // i.e. `*mut *mut c_void`, so this looks like it could be handing over a pointer *slot* for
    // the kernel to scribble struct bytes into). It is not: a pointer cast preserves the address
    // value, so what the callee receives is `&info` itself, matching Apple's own
    // `proc_pid_rusage(pid, RUSAGE_INFO_V0, (rusage_info_t *)&rusage)` idiom. Since that could
    // not be tested on this Linux-only machine, it was proven empirically instead, by running
    // the identical cast chain against a stand-in callee with the identical parameter type and
    // asserting the pointer value passed equals the struct's own address - it does.
    let result = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V0,
            &mut info as *mut libc::rusage_info_v0 as *mut libc::rusage_info_t,
        )
    };

    (result == 0).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_real_test_process() {
        let pid = std::process::id();
        assert!(
            Sampler.cpu_time(pid).is_some(),
            "this process's own rusage should be readable"
        );
        let resident = Sampler
            .resident_bytes(pid)
            .expect("this process's own resident size should be readable");
        assert!(
            resident > 0,
            "a running process should have non-zero resident memory, got {resident}"
        );
        assert_eq!(Sampler.backend_name(), "macos");
    }

    #[test]
    fn a_nonexistent_pid_is_an_honest_none() {
        assert_eq!(Sampler.cpu_time(u32::MAX), None);
        assert_eq!(Sampler.resident_bytes(u32::MAX), None);
    }

    #[test]
    fn reads_this_real_machines_total_memory() {
        let total = read_total_memory_bytes().expect("hw.memsize must be readable on a real Mac");
        let own_rss = Sampler
            .resident_bytes(std::process::id())
            .expect("own resident size");
        assert!(
            total >= own_rss,
            "total physical memory ({total}) must be at least this process's own RSS ({own_rss})"
        );
        assert_eq!(
            system_memory_bytes(),
            Some(total),
            "the cached accessor must return the same real reading, not a separate guess"
        );
    }

    #[test]
    fn the_real_machine_timebase_is_sane() {
        let (numer, denom) = mach_timebase();
        assert!(numer > 0, "a real timebase numerator is never zero");
        assert!(denom > 0, "a zero denominator would divide by zero");
        // Every real macOS timebase is within a couple of orders of magnitude of 1:1 (1/1 on
        // Intel, 125/3 on Apple Silicon). A wildly skewed ratio would mean the struct was
        // misread.
        assert!(
            numer <= 100_000 && denom <= 100_000,
            "implausible timebase {numer}/{denom} - the struct is probably being misread"
        );
    }

    #[test]
    fn mach_ticks_convert_by_the_real_timebase_ratio() {
        let (numer, denom) = mach_timebase();
        let ticks_per_second = 1_000_000_000u128 * denom as u128 / numer as u128;
        let nanos = mach_ticks_to_nanos(ticks_per_second as u64);
        let drift = nanos.abs_diff(1_000_000_000);
        assert!(
            drift < 1_000,
            "one second of ticks ({ticks_per_second}) should convert back to ~1e9 ns, got {nanos}"
        );
    }

    #[test]
    fn a_real_busy_child_accumulates_real_cpu_time_at_a_real_rate() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("yes")
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn a real `yes` child process");
        let pid = child.id();

        let first = Sampler.cpu_time(pid).expect("a live child has a cpu time");
        let wall_start = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(500));
        let second = Sampler
            .cpu_time(pid)
            .expect("still alive on the second read");
        let wall = wall_start.elapsed();

        let _ = child.kill();
        let _ = child.wait();

        let delta = second.saturating_sub(first);
        // A quarter of wall time is a very loose floor for a process that genuinely pins a core -
        // loose enough to survive a badly contended CI runner, tight enough that the 41.7x
        // under-report (which would land at ~2% of wall) fails it decisively.
        let floor = wall / 4;
        assert!(
            delta > floor,
            "a core-pinning child should burn CPU at close to wall-clock rate: got {delta:?} of \
             CPU over {wall:?} of wall, which is under the {floor:?} floor - this is what a \
             timebase/unit conversion bug looks like"
        );
        assert!(
            delta < wall * 4,
            "one single-threaded process cannot burn 4x wall time of CPU - got {delta:?} over \
             {wall:?}, which is what an over-scaled conversion would look like"
        );
    }
}
