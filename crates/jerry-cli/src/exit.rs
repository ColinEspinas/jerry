//! The exit-code contract: two codes are distinct only when the caller must do two different
//! things. JSON goes to stdout, diagnostics to stderr, and the code says what happened.

use jerry_core::{Report, RpcError};

/// Finished; nothing to do.
pub const DONE: u8 = 0;
/// Succeeded, but the caller has work to do next (a merge with conflicts lists the files).
pub const ACTION_REQUIRED: u8 = 1;
/// A command-line mistake; fix the invocation.
pub const USAGE: u8 = 2;
/// Refused; nothing happened, so do not retry, escalate to a human.
pub const DENIED: u8 = 3;
/// No Jerry reachable when one was required: not in a repository a live Jerry serves.
pub const NO_INSTANCE: u8 = 4;
/// Execution failed; the state may be intermediate.
pub const FAILED: u8 = 5;

pub fn for_report(report: &Report) -> u8 {
    match report {
        Report::Ok { .. } => DONE,
        Report::Denied { .. } => DENIED,
        Report::Error { .. } => FAILED,
    }
}

/// Implemented on `RpcError` in the crate root, next to the code table it reads.
pub trait ForRpc {
    fn exit_code(&self) -> u8;
}

pub fn for_rpc_error(error: &RpcError) -> u8 {
    error.exit_code()
}

#[cfg(test)]
mod exit_code_tests {
    use super::{for_report, for_rpc_error, DENIED, DONE, FAILED, NO_INSTANCE, USAGE};
    use jerry_core::wire::rpc_code;
    use jerry_core::{Error, Report, RpcError};

    #[test]
    fn every_report_status_and_rpc_code_maps_onto_the_contract() {
        assert_eq!(
            for_report(&Report::Ok {
                outcome: serde_json::Value::Null
            }),
            DONE
        );
        assert_eq!(
            for_report(&Report::Denied {
                code: "x".into(),
                reason: "y".into()
            }),
            DENIED
        );
        assert_eq!(
            for_report(&Report::Error {
                error: Error::new("x", "y")
            }),
            FAILED
        );
        let cases = [
            (rpc_code::FORBIDDEN, DENIED),
            (rpc_code::CONFINED, DENIED),
            (rpc_code::NEEDS_HOST, NO_INSTANCE),
            (rpc_code::SHUTTING_DOWN, NO_INSTANCE),
            (rpc_code::UNSUPPORTED_VERSION, NO_INSTANCE),
            (rpc_code::METHOD_NOT_FOUND, USAGE),
            (rpc_code::INVALID_PARAMS, USAGE),
            (rpc_code::INTERNAL_ERROR, FAILED),
            (rpc_code::TIMED_OUT, FAILED),
        ];
        for (code, expected) in cases {
            assert_eq!(for_rpc_error(&RpcError::new(code, "")), expected, "{code}");
        }
    }
}
