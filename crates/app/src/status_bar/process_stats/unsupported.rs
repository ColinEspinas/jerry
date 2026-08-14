//! The fallback [`ProcessSampler`](super::ProcessSampler) backend for a target with no real
//! per-process sampling implementation - everything that is not Linux, macOS or Windows.
//!
//! In practice that is FreeBSD, the one remaining target this workspace can genuinely be built
//! for (`gpui_linux` is a dependency under `cfg(any(target_os = "linux", target_os =
//! "freebsd"))` - see `crates/app/Cargo.toml`'s own comment on why that predicate is deliberately
//! wider than Linux alone). FreeBSD has a real `/compat/linux`-style procfs and a `kvm`/`sysctl`
//! route to the same numbers, so this is a "not written yet" backend rather than an
//! "impossible" one; it exists so that adding a target can never silently produce a build that
//! reads a path that cannot exist there.
//!
//! Both readings are honestly `None`, always. That makes [`SUPPORTED`] `false`, which is what
//! `crate::status_bar::render` reads to omit the cpu/memory fields entirely on such a build - see
//! the parent module's docs on why a permanently-unresolvable `...% cpu` is the one `None` that
//! must not reach the screen.

use super::ProcessSampler;
use std::time::Duration;

/// The no-op backend - a stateless unit struct, per [`ProcessSampler`]'s docs.
pub struct Sampler;

/// This platform has no real backend, and the status bar must omit the cpu/memory fields rather
/// than show a placeholder that can never resolve.
pub const SUPPORTED: bool = false;

impl ProcessSampler for Sampler {
    fn backend_name(&self) -> &'static str {
        "unsupported"
    }

    fn cpu_time(&self, _pid: u32) -> Option<Duration> {
        None
    }

    fn resident_bytes(&self, _pid: u32) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Honestly `None` even for this very process, which certainly exists - there is genuinely no
    /// implementation here, and that is reported rather than guessed at.
    #[test]
    fn every_reading_is_an_honest_none() {
        let pid = std::process::id();
        assert_eq!(Sampler.cpu_time(pid), None);
        assert_eq!(Sampler.resident_bytes(pid), None);
        assert_eq!(Sampler.backend_name(), "unsupported");
        // [`SUPPORTED`] is what the status bar reads to omit the cpu/memory fields entirely, so
        // what actually matters is that it agrees with what this backend can really produce -
        // asserted against a genuine reading of a genuinely-existing process rather than as a
        // bare `assert!(!SUPPORTED)`, which is a constant-valued assertion clippy rejects (and
        // which would prove nothing about the backend anyway).
        assert_eq!(
            SUPPORTED,
            Sampler.cpu_time(pid).is_some(),
            "SUPPORTED must match whether this backend can genuinely sample anything"
        );
    }
}
