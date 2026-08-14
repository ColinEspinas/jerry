//! The macOS [`ProcessSampler`](super::ProcessSampler) backend: one `proc_pid_rusage` call per
//! pid, for both CPU time and resident memory (GitHub issue #283).
//!
//! ## Why `proc_pid_rusage`, and why raw `libc` rather than the `libproc` crate
//!
//! `proc_pid_rusage` (declared in `<libproc.h>`, implemented in libSystem, which every macOS
//! binary already links) fills a `rusage_info_v0` in a single syscall carrying *both* readings
//! this backend needs - `ri_user_time`/`ri_system_time` and `ri_resident_size` - so a full
//! sample is one call, not two. The alternative in-process API,
//! `proc_pidinfo(PROC_PIDTASKINFO)`, reports its CPU times in mach absolute-time units, which
//! then need `mach_timebase_info` to become real seconds; `proc_pid_rusage` reports nanoseconds
//! directly (xnu converts them with `absolutetime_to_nanoseconds` before filling the struct), so
//! it avoids a whole class of unit bug. Shelling out to `ps` was never an option - issue #283
//! rules it out explicitly, and this runs on the status poll for every open agent.
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
//! This file has never been executed: it was written on a Linux machine with no macOS host or
//! Apple SDK available. What *was* verified locally is that it type-checks against the real
//! Apple `libc` bindings, by `cargo check --target aarch64-apple-darwin` over this module - see
//! the PR for issue #283. Its first real execution is CI's `macos-latest` job
//! (`.github/workflows/release.yml`).

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
        // Both fields are nanoseconds. `saturating_add` because they are two independent
        // counters and nothing in the ABI promises their sum fits - it always does in practice
        // (their sum would have to exceed ~584 years of CPU time), but wrapping here would
        // produce a wildly wrong percentage rather than a merely clamped one.
        Some(Duration::from_nanos(
            info.ri_user_time.saturating_add(info.ri_system_time),
        ))
    }

    fn resident_bytes(&self, pid: u32) -> Option<u64> {
        Some(read_rusage(pid)?.ri_resident_size)
    }
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

    /// A genuinely CPU-busy child really does report growing CPU time - the end-to-end proof
    /// that the nanosecond fields are being read as nanoseconds and not, say, mach ticks.
    #[test]
    fn a_real_busy_child_accumulates_real_cpu_time() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("yes")
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn a real `yes` child process");
        let pid = child.id();

        let first = Sampler.cpu_time(pid).expect("a live child has a cpu time");
        std::thread::sleep(Duration::from_millis(200));
        let second = Sampler
            .cpu_time(pid)
            .expect("still alive on the second read");

        let _ = child.kill();
        let _ = child.wait();

        let delta = second.saturating_sub(first);
        assert!(
            delta > Duration::ZERO,
            "a process spinning for 200ms must have burned real CPU time, got {delta:?}"
        );
        assert!(
            delta < Duration::from_secs(10),
            "200ms of wall time cannot be 10s of CPU time on one busy process - a unit bug \
             (mach ticks read as nanoseconds, say) would show up exactly here, got {delta:?}"
        );
    }
}
