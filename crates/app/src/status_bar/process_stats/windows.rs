//! The Windows [`ProcessSampler`](super::ProcessSampler) backend: `GetProcessTimes` for CPU time
//! and PSAPI's `GetProcessMemoryInfo` for the working set (GitHub issue #283).
//!
//! ## The two calls, and the handle each one needs
//!
//! Both readings go through a process handle from `OpenProcess`, so both are wrapped in
//! [`ProcessHandle`], a real RAII guard that `CloseHandle`s on drop - a handle leaked once per
//! agent per status poll would be a genuine, unbounded resource leak in a long-running window,
//! not a theoretical one.
//!
//! - `GetProcessTimes` needs only `PROCESS_QUERY_LIMITED_INFORMATION`, and returns four
//!   `FILETIME`s of which this backend sums the kernel and user ones. A `FILETIME` here is *not*
//!   a wall-clock date: for the kernel/user fields it is a duration in 100-nanosecond units,
//!   which is why the two halves are recombined into a `u64` and multiplied by 100 rather than
//!   run through any date conversion.
//! - `GetProcessMemoryInfo` needs the same right, and *only* that right. Checked against the
//!   current Microsoft documentation rather than assumed: "The handle must have the
//!   **PROCESS_QUERY_INFORMATION** or **PROCESS_QUERY_LIMITED_INFORMATION** access right", with
//!   `PROCESS_VM_READ` additionally required only on "Windows Server 2003 and Windows XP" -
//!   platforms this app does not target and could not run on. So both readings open the process
//!   exactly once, with exactly one access right, rather than trying a wider mask first and
//!   falling back.
//!
//! `WorkingSetSize` is the field read, not `PagefileUsage`/`PrivateUsage`: the working set is the
//! physical memory currently resident for the process, which is what Linux's `VmRSS` and macOS's
//! `ri_resident_size` also are, and issue #283's requirement is that the same status-bar number
//! mean the same thing on all three.
//!
//! ## WSL
//!
//! This backend covers *native* Windows processes. An agent hosted inside a WSL2 distro is a real
//! Linux process, sampled by a real Linux build of this app running inside that distro through
//! `super::linux`'s `/proc` path - see the parent module's docs.
//!
//! See the parent module's docs for the per-process (not per-process-tree) limitation, which is
//! documented once there for every platform rather than repeated per backend - `GetProcessTimes`
//! covers this pid alone, exactly like the Linux and macOS readings. (A Windows job object could
//! roll a whole tree up here, which is precisely why that fix must not land on this platform
//! alone.)
//!
//! ## Verification status
//!
//! This file was written on a Linux machine, so nothing in it was executed locally. What *was*
//! verified locally is that it type-checks against the real `windows-sys` bindings, by
//! cross-compiling this module for `x86_64-pc-windows-gnu` - see the PR for issue #283.
//!
//! Its real execution happens in CI. Note that `cargo build` does *not* compile `#[cfg(test)]`
//! modules at all, so the `windows-latest` build job alone would never have run a line of this
//! file's tests - `.github/workflows/ci.yml`'s Windows job therefore has a dedicated
//! `cargo test -p app --lib status_bar::process_stats` step, added with this backend, which is
//! what actually exercises the code below on a real Windows runner. Without it, "CI covers this"
//! would have been an empty claim.

// This module exists entirely to call Win32 FFI (`OpenProcess`, `GetProcessTimes`,
// `GetProcessMemoryInfo`, `GlobalMemoryStatusEx`) - every call site below carries its own
// `SAFETY` comment; see CLAUDE.md's Rust standards for the project-wide "unsafe only for
// justified FFI" rule this module is the Windows half of.
#![allow(unsafe_code)]

use super::ProcessSampler;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// The Windows Win32/PSAPI backend - a stateless unit struct, per [`ProcessSampler`]'s docs.
pub struct Sampler;

/// Windows has a real backend.
pub const SUPPORTED: bool = true;

impl ProcessSampler for Sampler {
    fn backend_name(&self) -> &'static str {
        "windows"
    }

    fn cpu_time(&self, pid: u32) -> Option<Duration> {
        let handle = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();

        // SAFETY: `handle.0` is a live process handle this function owns for the whole call (see
        // `ProcessHandle`), opened with the `PROCESS_QUERY_LIMITED_INFORMATION` right this call
        // requires. The four out-pointers address distinct, live, uniquely borrowed stack
        // `FILETIME`s, each exactly the size the callee writes; none is retained after the call.
        let ok =
            unsafe { GetProcessTimes(handle.0, &mut creation, &mut exit, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }

        // `saturating_add` on the two 100ns counters, then to nanoseconds: see the module docs
        // for why these are durations rather than timestamps.
        let hundred_nanos =
            filetime_to_hundred_nanos(kernel).saturating_add(filetime_to_hundred_nanos(user));
        Some(Duration::from_nanos(hundred_nanos.saturating_mul(100)))
    }

    fn resident_bytes(&self, pid: u32) -> Option<u64> {
        let handle = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;

        // The struct's own `cb` field is deliberately left at its `default()` zero rather than
        // pre-set to the struct size. That is the classic suspicion with this call, so it was
        // checked rather than assumed: `cb` is an *output* here - the size the callee needs is
        // the `size` argument below, and `GetProcessMemoryInfo` fills `cb` in on success. The
        // reference implementation this was checked against is the `sysinfo` crate
        // (`sysinfo-0.37.2/src/windows/process.rs`), which is exercised on real Windows at
        // enormous scale and likewise passes a `::default()`-zeroed struct plus the size
        // argument alone, reading `WorkingSetSize` back out exactly as this does.
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;

        // SAFETY: `handle.0` is a live process handle this function owns for the whole call. The
        // out-pointer addresses a live, uniquely borrowed stack `PROCESS_MEMORY_COUNTERS`, and
        // `size` is that same struct's real size taken from the type itself, which is the
        // contract this call uses to decide how many bytes to write. The struct is not read
        // unless the call reports success.
        let ok = unsafe { GetProcessMemoryInfo(handle.0, &mut counters, size) };
        if ok == 0 {
            return None;
        }

        Some(counters.WorkingSetSize as u64)
    }
}

/// See [`super::system_memory_bytes`] - this machine's real total physical memory, from one
/// `GlobalMemoryStatusEx` call's `ullTotalPhys`.
///
/// `GlobalMemoryStatusEx` rather than the older `GlobalMemoryStatus`: the older call's fields are
/// `SIZE_T`, so on a machine with more memory than fits the type it reports a clamped value
/// instead of failing - Microsoft's own documentation says to use the `Ex` form for exactly that
/// reason. `ullTotalPhys` is a `u64` and is the installed physical memory, which is the
/// denominator the Resources popover's memory meter needs.
///
/// Read once and cached, for the same reason the macOS backend caches its `hw.memsize`: installed
/// memory cannot change while the process runs, and this is a per-frame call site.
pub fn system_memory_bytes() -> Option<u64> {
    static TOTAL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TOTAL.get_or_init(read_total_memory_bytes)
}

/// The uncached `GlobalMemoryStatusEx` reading behind [`system_memory_bytes`], separate so the
/// test below exercises the real call rather than a cached first result.
fn read_total_memory_bytes() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        // Unlike `PROCESS_MEMORY_COUNTERS::cb` above, this length field is a genuine *input*:
        // `GlobalMemoryStatusEx` fails outright unless `dwLength` is pre-set to the struct's own
        // size, which is how it validates the caller's struct version. Taken from the type
        // itself rather than written as a literal.
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };

    // SAFETY: the out-pointer addresses a live, uniquely borrowed stack `MEMORYSTATUSEX` whose
    // `dwLength` has been set to its own real size, which is the whole contract this call has;
    // it writes only within that struct and does not retain the pointer. The struct is not read
    // unless the call reports success.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 || status.ullTotalPhys == 0 {
        return None;
    }
    Some(status.ullTotalPhys)
}

/// Recombines a `FILETIME`'s two 32-bit halves into the single 64-bit count of 100-nanosecond
/// units it actually is. Pure arithmetic on values already copied out of the struct, so this is
/// safe code and directly testable.
fn filetime_to_hundred_nanos(time: FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | (time.dwLowDateTime as u64)
}

/// A real `OpenProcess` handle, closed exactly once on drop.
///
/// `CloseHandle` on drop rather than at each early return: [`Sampler::cpu_time`] and
/// [`Sampler::resident_bytes`] both have failure paths after the open, and a handle leaked on
/// one of them would leak once per agent per status poll for the lifetime of the window.
struct ProcessHandle(HANDLE);

impl ProcessHandle {
    /// `None` when the process cannot be opened at all - it has exited, or this app genuinely may
    /// not query it. Unlike `crate::hooks::settings_file`'s liveness check, which must treat
    /// "access denied" as *alive* because a wrong answer there deletes files, a failure here is
    /// simply an unknown reading, and the parent module's honest-`None` convention covers it: the
    /// status bar shows what it does know and omits what it doesn't.
    fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<Self> {
        // SAFETY: `OpenProcess` takes only scalars and returns a handle (null on failure). It
        // borrows no memory from this process, so there is nothing for it to invalidate.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `OpenProcess` in `open` (null is never wrapped),
        // is owned solely by this value, and `Drop` runs exactly once - so this closes a valid
        // handle exactly once, and nothing can use it afterwards.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `FILETIME` recombination, against hand-checked bit patterns - the one piece of this
    /// backend that is pure arithmetic and so can be wrong in a way no OS call would catch.
    #[test]
    fn filetime_recombines_its_two_halves() {
        assert_eq!(
            filetime_to_hundred_nanos(FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            }),
            0
        );
        assert_eq!(
            filetime_to_hundred_nanos(FILETIME {
                dwLowDateTime: 12_345,
                dwHighDateTime: 0,
            }),
            12_345
        );
        assert_eq!(
            filetime_to_hundred_nanos(FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 1,
            }),
            1u64 << 32
        );
        assert_eq!(
            filetime_to_hundred_nanos(FILETIME {
                dwLowDateTime: u32::MAX,
                dwHighDateTime: u32::MAX,
            }),
            u64::MAX
        );
    }

    /// One real CPU-second is 10,000,000 `FILETIME` units - the conversion this backend applies
    /// before handing a [`Duration`] to the shared math, checked end to end.
    #[test]
    fn a_hundred_nanosecond_count_converts_to_real_time() {
        let one_cpu_second = filetime_to_hundred_nanos(FILETIME {
            dwLowDateTime: 10_000_000,
            dwHighDateTime: 0,
        });
        assert_eq!(
            Duration::from_nanos(one_cpu_second * 100),
            Duration::from_secs(1)
        );
    }

    /// Real Win32 calls against this very test process, which always exists and which this
    /// process can always open.
    #[test]
    fn reads_this_real_test_process() {
        let pid = std::process::id();
        assert!(
            Sampler.cpu_time(pid).is_some(),
            "this process's own GetProcessTimes should succeed"
        );
        let resident = Sampler
            .resident_bytes(pid)
            .expect("this process's own working set should be readable");
        assert!(
            resident > 0,
            "a running process should have a non-zero working set, got {resident}"
        );
        assert_eq!(Sampler.backend_name(), "windows");
    }

    #[test]
    fn a_nonexistent_pid_is_an_honest_none() {
        assert_eq!(Sampler.cpu_time(u32::MAX), None);
        assert_eq!(Sampler.resident_bytes(u32::MAX), None);
    }

    /// A real `GlobalMemoryStatusEx` read on the runner itself: every real Windows machine has
    /// some physical memory, and it is always at least as large as this process's own working
    /// set - which is what makes it a usable meter denominator rather than a number that could
    /// put the numerator past 100%.
    #[test]
    fn reads_this_real_machines_total_memory() {
        let total =
            read_total_memory_bytes().expect("GlobalMemoryStatusEx must succeed on real Windows");
        let own_rss = Sampler
            .resident_bytes(std::process::id())
            .expect("own working set");
        assert!(
            total >= own_rss,
            "total physical memory ({total}) must be at least this process's own working set \
             ({own_rss})"
        );
        assert_eq!(
            system_memory_bytes(),
            Some(total),
            "the cached accessor must return the same real reading, not a separate guess"
        );
    }
}
