//! What comes back from a Command or Query: the serializable projection every client consumes.
//! `denied` means nothing happened; `error` may have left intermediate state behind.

use crate::error::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Report {
    Ok { outcome: Value },
    Denied { code: String, reason: String },
    Error { error: Error },
}

impl Report {
    /// `ok` carrying `outcome`; an outcome that cannot serialize becomes an `internal` error,
    /// never a panic on the dispatch path.
    pub fn ok<T: Serialize>(outcome: &T) -> Report {
        match serde_json::to_value(outcome) {
            Ok(outcome) => Report::Ok { outcome },
            Err(error) => Report::Error {
                error: Error::from(error),
            },
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Report::Ok { .. })
    }
}

#[cfg(test)]
mod report_wire_tests {
    use super::Report;
    use crate::error::Error;

    #[test]
    fn every_status_round_trips_and_is_tagged() {
        let reports = [
            Report::Ok {
                outcome: serde_json::json!({ "branch": "main" }),
            },
            Report::Denied {
                code: "dirty-worktree".into(),
                reason: "the base worktree has uncommitted changes".into(),
            },
            Report::Error {
                error: Error::new("merge-files-still-conflicted", "2 file(s) still unmerged")
                    .with_data(serde_json::json!({ "paths": ["src/a.rs"] })),
            },
        ];
        for report in reports {
            let json = serde_json::to_value(&report).expect("serializable");
            assert!(json.get("status").is_some(), "{json}");
            let back: Report = serde_json::from_value(json).expect("deserializable");
            assert_eq!(back, report);
        }
    }
}
