//! The macOS [`ProcessSampler`](super::ProcessSampler) backend: one `proc_pid_rusage` call per
//! pid, for both CPU time and resident memory (GitHub issue #283).
//!
//! ## Why `proc_pid_rusage`, and why raw `libc` rather than the `libproc` crate
//!
//! `proc_pid_rusage` (declared in `<libproc.h>`, implemented in libSystem, which every macOS
//! binary already links) fills a `rusage_info_v0` in a single syscall carrying *both* readings
//! this backend needs - `ri_user_time`/`ri_system_time` and `ri_resident_size` - so a full
//! sample is one call, not two. Shelling out to `ps` was never an option - issue #283 rules it
//! out explicitly, and this runs on the status poll for every open agent.
//!
//! ## `ri_user_time`/`ri_system_time` are mach timebase units, NOT nanoseconds
//!
//! This is the one genuinely dangerous thing about this API, and an earlier version of this file
//! got it wrong, so the evidence is written down rather than assumed. The fields *look* like
//! nanoseconds (they are plain `u64`s named `_time`, next to siblings that are explicitly named
//! `_abstime`), and on Intel Macs they behave exactly like nanoseconds. They are not. Traced
//! through xnu, the value is copied out of the kernel completely unconverted:
//!
//! - `osfmk/kern/recount.h` - `// Time tracking, in Mach timebase units.` / `uint64_t rm_time_mach;`
//! - `osfmk/kern/task.c` - `info->total_user = usage.ru_metrics[RCT_LVL_USER].rm_time_mach;`
//! - `osfmk/kern/bsd_kern.c` (`fill_task_rusage`) - `ri->ri_user_time = powerinfo.total_user;`
//!   and `ri->ri_system_time = powerinfo.total_system;`, a raw copy with no conversion
//! - `bsd/kern/kern_resource.c` (`proc_get_rusage`) - a bare `copyout`, still no conversion
//! - `libsyscall/wrappers/libproc/libproc.c` - `proc_pid_rusage` is a bare `__proc_info`
//!   trampoline
//!
//! By contrast, the same kernel file converts explicitly when it wants real time:
//! `absolutetime_to_microtime(tinfo.total_user + tinfo.total_system, ...)`. There is no
//! `absolutetime_to_nanoseconds` anywhere on this path.
//!
//! On Intel, `mach_timebase_info` is `1/1`, so the raw value *is* nanoseconds and the bug is
//! invisible. On Apple Silicon the timebase is `125/3` (a 24MHz counter, ~41.67ns per tick), so
//! reading the field as nanoseconds under-reports CPU by ~41.7x - an agent pinning a whole core
//! would render as `2% cpu`. This is a real, reported defect in other projects, not a
//! hypothetical: osquery hit it as issue #7459, "Processes columns user_time and system_time
//! values are incorrect on Apple Silicon".
//!
//! [`mach_ticks_to_nanos`] therefore applies the real `mach_timebase_info` ratio, and
//! [`Sampler::cpu_time`]'s test asserts a genuine *lower* bound against wall-clock time - an
//! upper bound alone cannot catch an under-report, which is exactly how the earlier version
//! passed its own tests. `proc_pidinfo(PROC_PIDTASKINFO)` would have needed the identical
//! conversion, so this is not a reason to prefer one over the other; `proc_pid_rusage` is still
//! chosen for carrying both readings in one call.
//!
//! The `libproc` crate (`libproc-rs`) offers the same call behind a safe wrapper, and it was
//! considered and deliberately not used:
//!
//! - `libc` is *already* a real dependency of this crate on every Unix target (see
//!   `crates/app/Cargo.toml`, added for `kill(pid, 0)`), and it already declares
//!   `proc_pid_rusage`, `rusage_info_v0` and `RUSAGE_INFO_V0` with the exact signatures used
//!   below - so this backend adds no new build surface at all, where `libproc` would add a new
//!   third-party crate for one function.
//! - It matches the decision this codebase already made on the other side of the same pair:
//!   `crate::hooks::settings_file` calls Win32 through raw `windows-sys` rather than the
//!   higher-level `windows` crate, for exactly this reason (see that dependency's own comment in
//!   `Cargo.toml`). Two FFI calls in one file, each with a two-line safety note, is not the
//!   place a wrapper crate earns its keep.
//! - `libproc`'s wrapper returns its own re-declared struct types, so a version bump there could
//!   silently change what this file compiles against, whereas `libc`'s Apple bindings are the
//!   ones the rest of this workspace is already pinned to.
//!
//! ## Permissions
//!
//! `proc_pid_rusage` succeeds for processes owned by the calling user without any entitlement or
//! privilege, and every pid this app samples is one of its own spawned children. A pid that has
//! exited, or one this app genuinely may not query, fails the call and is reported as an honest
//! `None`, per the parent module's convention.
//!
//! See the parent module's docs for the per-process (not per-process-tree) limitation, which is
//! documented once there for every platform rather than repeated per backend -
//! `proc_pid_rusage`'s readings cover this pid alone, exactly like the Linux and Windows ones.
//!
//! ## Verification status
//!
//! This file was written on a Linux machine with no macOS host or Apple SDK available, so nothing
//! in it was executed locally. What *was* verified locally is that it type-checks against the
//! real Apple `libc` bindings, by cross-compiling this module for `aarch64-apple-darwin` - see
//! the PR for issue #283.
//!
//! Its real execution happens in CI. Note that `cargo build` does *not* compile `#[cfg(test)]`
//! modules at all, so the `macos-latest` build job alone would never have run a line of this
//! file's tests - `.github/workflows/ci.yml`'s macOS job therefore has a dedicated
//! `cargo test -p app --lib status_bar::process_stats` step, added with this backend, which is
//! what actually exercises the code below on a real Apple Silicon runner. Without it, "CI covers
//! this" would have been an empty claim.

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
///
/// Read once and cached: the ratio is a fixed property of the hardware, it cannot change while
/// the process runs, and [`Sampler::cpu_time`] runs for every agent on every status poll.
///
/// A failed call (which would need `mach_timebase_info` itself to fail - it reads a commpage
/// value and does not really fail) or a zero denominator falls back to `1/1`, i.e. "treat the
/// reading as nanoseconds". That is the honest degradation: correct on Intel, and on Apple
/// Silicon it is the same under-report the pre-fix code had rather than a divide-by-zero panic
/// or a fabricated number.
///
/// `#[allow(deprecated)]`: `libc` deprecated its mach bindings in 0.2.55 in favour of the `mach2`
/// crate, purely as dependency hygiene on their side - the C API itself is unchanged and the
/// binding is correct (`mach_timebase_info_data_t` is two `u32`s and has been ABI-frozen for the
/// lifetime of macOS). Taking the deprecation at face value would mean adding a whole crate for
/// one two-field struct, against this file's own stated reason for using raw `libc` at all.
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
///
/// The intermediate multiply is done in `u128` so the exact product is always representable and
/// no precision is lost to a saturating clamp before the divide; only the final nanosecond count
/// is narrowed back to `u64` (saturating at ~584 years, which [`Duration::from_nanos`] is bounded
/// by anyway).
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
///
/// `sysctlbyname` with the string name rather than a `[CTL_HW, HW_MEMSIZE]` mib array: the name
/// form is the documented, stable spelling and needs no numeric constant whose value would have
/// to be trusted sight-unseen from a machine that cannot run this code. `hw.memsize` (not
/// `hw.physmem`) is the 64-bit field - `hw.physmem` is a 32-bit `int` that saturates at 2 GB and
/// would silently under-report every real Mac this app runs on.
///
/// Read once and cached: installed physical memory cannot change while the process runs, and this
/// is the denominator of a meter redrawn on every frame the Resources popover is open.
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
///
/// `RUSAGE_INFO_V0` rather than a later flavor: it is the smallest struct the call accepts and it
/// already carries every field this backend reads (`ri_user_time`, `ri_system_time`,
/// `ri_resident_size` are all v0 fields, present unchanged in v1-v6). Asking for the smallest
/// flavor that answers the question is also the safest, since the kernel writes exactly the
/// number of bytes that flavor describes into the buffer provided here.
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

    /// Real `proc_pid_rusage` against this very test process, which always exists and is always
    /// owned by the calling user.
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

    /// A real `sysctlbyname("hw.memsize")` read on the runner itself: every real Mac has some
    /// physical memory, and it is always at least as large as this process's own resident set -
    /// which is what makes it a usable meter denominator rather than a number that could put the
    /// numerator past 100%.
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

    /// The timebase ratio is a real, sane hardware value - `1/1` on Intel, `125/3` on Apple
    /// Silicon - never the `(0, 0)` a failed read would leave behind, which would make
    /// [`mach_ticks_to_nanos`] divide by zero.
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

    /// The conversion itself, as arithmetic: the Apple Silicon ratio really does turn the raw
    /// counter into the nanosecond count it represents, and the Intel `1/1` case is a pass-through.
    #[test]
    fn mach_ticks_convert_by_the_real_timebase_ratio() {
        let (numer, denom) = mach_timebase();
        // One real second of CPU time expressed in this machine's own ticks, converted back.
        let ticks_per_second = 1_000_000_000u128 * denom as u128 / numer as u128;
        let nanos = mach_ticks_to_nanos(ticks_per_second as u64);
        let drift = nanos.abs_diff(1_000_000_000);
        assert!(
            drift < 1_000,
            "one second of ticks ({ticks_per_second}) should convert back to ~1e9 ns, got {nanos}"
        );
    }

    /// A genuinely CPU-busy child really does report growing CPU time, **at roughly the rate a
    /// pinned core actually burns it**.
    ///
    /// The lower bound is the entire point of this test and is why it is written this way: the
    /// unit bug this backend actually shipped with once (mach ticks read as nanoseconds -
    /// see the module docs) *under*-reports by ~41.7x on Apple Silicon. A bare
    /// `delta > Duration::ZERO`, or an upper bound alone, passes cleanly against a 41x
    /// under-report and proves nothing. `yes` pins one core, so ~200ms of wall time must produce
    /// a substantial fraction of 200ms of CPU time; anything near zero means the conversion is
    /// wrong again.
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
