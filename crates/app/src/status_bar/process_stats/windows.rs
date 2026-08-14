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
//! - `GetProcessMemoryInfo` is documented as needing `PROCESS_QUERY_INFORMATION` (or
//!   `PROCESS_QUERY_LIMITED_INFORMATION`) *and* `PROCESS_VM_READ`. In practice the export in
//!   `psapi.dll` forwards to `K32GetProcessMemoryInfo` in `kernel32.dll` on Windows 7 and later,
//!   which is satisfied by the limited right alone - so [`Sampler::resident_bytes`] asks for the
//!   documented pair first and falls back to the limited right by itself if that open is denied,
//!   rather than reporting an honest-but-avoidable `None` for a process it could in fact read.
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
//! This file has never been executed: it was written on a Linux machine. What *was* verified
//! locally is that it type-checks against the real `windows-sys` bindings, by
//! `cargo check --target x86_64-pc-windows-gnu` over this module - see the PR for issue #283.
//! Its first real execution is CI's `windows-latest` job (`.github/workflows/release.yml`).

use super::ProcessSampler;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ,
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
        // The documented access pair first; the limited right alone as the real-world fallback -
        // see the module docs.
        let handle = ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
            .or_else(|| ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION))?;

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
}
