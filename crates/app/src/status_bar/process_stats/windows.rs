//! The Windows [`ProcessSampler`](super::ProcessSampler) backend: `GetProcessTimes` for CPU time
//! and PSAPI's `GetProcessMemoryInfo` for the working set (GitHub issue #283).

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
