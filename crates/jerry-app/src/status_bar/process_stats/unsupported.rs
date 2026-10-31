//! The fallback [`ProcessSampler`](super::ProcessSampler) backend for a target with no real
//! per-process sampling implementation - everything that is not Linux, macOS or Windows.

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

/// See [`super::system_memory_bytes`] - honestly `None`, like every other reading here. The
/// Resources popover's memory meter renders no fill at all rather than one against a guessed
/// denominator.
pub fn system_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reading_is_an_honest_none() {
        let pid = std::process::id();
        assert_eq!(Sampler.cpu_time(pid), None);
        assert_eq!(Sampler.resident_bytes(pid), None);
        assert_eq!(super::system_memory_bytes(), None);
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
