//! The application-layer unit: a `Command` mutates, a `Query` reads. Both are typed values with
//! a typed, serializable outcome, so the GUI, the CLI and the host dispatch the same thing.

use crate::ctx::{Caller, Ctx};
use crate::error::Error;
use crate::report::Report;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Whether an `Agent` caller may ask for this at all. A `Human` caller always may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Invocability {
    Allowed,
    Denied,
}

/// Where this can execute. `Git` touches only the shared on-disk repository and may run in any
/// process; `Session` needs the host's session table and never runs standalone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Locality {
    Git,
    Session,
}

/// A refusal from `validate`: the request was legitimate but the current state does not allow
/// it, and nothing happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denied {
    /// Stable machine identifier, kebab-case. Callers branch on this, never on `reason`.
    pub code: String,
    pub reason: String,
}

impl Denied {
    pub fn new(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: reason.into(),
        }
    }
}

/// A mutation. `execute` consumes it: a mutation is used once.
pub trait Command {
    /// Serializable because the wire `Report` is the only thing any client, GUI included,
    /// ever consumes.
    type Outcome: Serialize + DeserializeOwned;
    /// The kebab-case name after `command/` on the wire.
    const NAME: &'static str;
    fn invocability(&self) -> Invocability;
    fn locality(&self) -> Locality;
    /// Answers "could this run right now, and if not why" without running it.
    fn validate(&self, ctx: &Ctx) -> Result<(), Denied>;
    fn execute(self, ctx: &Ctx) -> Result<Self::Outcome, Error>;
}

/// A read. `run` borrows: a read is replayable.
pub trait Query {
    type Outcome: Serialize + DeserializeOwned;
    /// The kebab-case name after `query/` on the wire.
    const NAME: &'static str;
    fn invocability(&self) -> Invocability {
        Invocability::Allowed
    }
    fn locality(&self) -> Locality;
    fn run(&self, ctx: &Ctx) -> Result<Self::Outcome, Error>;
}

/// The authority rule: a human may ask anything; an agent only what is `Allowed`.
pub fn permits(caller: &Caller, invocability: Invocability) -> bool {
    match caller {
        Caller::Human => true,
        Caller::Agent { .. } => invocability == Invocability::Allowed,
    }
}

/// The one execution path for a `Command`: validate, then execute, projected into a `Report`.
pub fn run_command<C: Command>(command: C, ctx: &Ctx) -> Report {
    if let Err(denied) = command.validate(ctx) {
        return Report::Denied {
            code: denied.code,
            reason: denied.reason,
        };
    }
    match command.execute(ctx) {
        Ok(outcome) => Report::ok(&outcome),
        Err(error) => Report::Error { error },
    }
}

/// `validate` alone, as a `Report`: `ok` with a null outcome, or `denied`.
pub fn validate_command<C: Command>(command: &C, ctx: &Ctx) -> Report {
    match command.validate(ctx) {
        Ok(()) => Report::Ok {
            outcome: serde_json::Value::Null,
        },
        Err(denied) => Report::Denied {
            code: denied.code,
            reason: denied.reason,
        },
    }
}

pub fn run_query<Q: Query>(query: &Q, ctx: &Ctx) -> Report {
    match query.run(ctx) {
        Ok(outcome) => Report::ok(&outcome),
        Err(error) => Report::Error { error },
    }
}

/// A JSON Schema for `T`, as the MCP `tools/list` `inputSchema` (`docs/architecture/decisions.md`
/// §22).
pub fn schema_of<T: schemars::JsonSchema>() -> Value {
    schemars::SchemaGenerator::default()
        .into_root_schema_for::<T>()
        .to_value()
}

#[cfg(test)]
mod command_dispatch_tests {
    use super::{
        permits, run_command, run_query, validate_command, Command, Denied, Invocability, Locality,
        Query,
    };
    use crate::ctx::{AgentId, Caller, Ctx};
    use crate::error::Error;
    use crate::report::Report;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;

    fn ctx(caller: Caller) -> Ctx {
        Ctx {
            repo_path: PathBuf::from("/repo/.git"),
            worktree_path: PathBuf::from("/repo"),
            caller,
        }
    }

    #[derive(Serialize, Deserialize)]
    struct Echoed {
        value: u32,
    }

    /// Refuses when `value` is zero, fails when it is odd, succeeds when it is even.
    struct Probe {
        value: u32,
    }

    impl Command for Probe {
        type Outcome = Echoed;
        const NAME: &'static str = "probe";
        fn invocability(&self) -> Invocability {
            Invocability::Denied
        }
        fn locality(&self) -> Locality {
            Locality::Git
        }
        fn validate(&self, _ctx: &Ctx) -> Result<(), Denied> {
            if self.value == 0 {
                return Err(Denied::new("probe-zero", "zero is refused"));
            }
            Ok(())
        }
        fn execute(self, _ctx: &Ctx) -> Result<Echoed, Error> {
            if self.value % 2 == 1 {
                return Err(Error::new("probe-odd", "odd values fail"));
            }
            Ok(Echoed { value: self.value })
        }
    }

    impl Query for Probe {
        type Outcome = Echoed;
        const NAME: &'static str = "probe";
        fn locality(&self) -> Locality {
            Locality::Git
        }
        fn run(&self, _ctx: &Ctx) -> Result<Echoed, Error> {
            Ok(Echoed { value: self.value })
        }
    }

    #[test]
    fn a_human_may_ask_anything_and_an_agent_only_what_is_allowed() {
        let agent = Caller::Agent {
            id: AgentId::from("a1"),
        };
        assert!(permits(&Caller::Human, Invocability::Denied));
        assert!(permits(&Caller::Human, Invocability::Allowed));
        assert!(permits(&agent, Invocability::Allowed));
        assert!(!permits(&agent, Invocability::Denied));
    }

    #[test]
    fn a_refused_validation_means_nothing_happened() {
        let report = run_command(Probe { value: 0 }, &ctx(Caller::Human));
        assert_eq!(
            report,
            Report::Denied {
                code: "probe-zero".into(),
                reason: "zero is refused".into()
            }
        );
        assert_eq!(
            validate_command(&Probe { value: 0 }, &ctx(Caller::Human)),
            report
        );
    }

    #[test]
    fn execute_failures_and_successes_project_into_the_report() {
        let failed = run_command(Probe { value: 3 }, &ctx(Caller::Human));
        assert!(matches!(failed, Report::Error { ref error } if error.code == "probe-odd"));

        let ok = run_command(Probe { value: 4 }, &ctx(Caller::Human));
        assert_eq!(
            ok,
            Report::Ok {
                outcome: serde_json::json!({ "value": 4 })
            }
        );

        let valid = validate_command(&Probe { value: 4 }, &ctx(Caller::Human));
        assert_eq!(
            valid,
            Report::Ok {
                outcome: serde_json::Value::Null
            }
        );
    }

    #[test]
    fn a_query_runs_without_validation() {
        let report = run_query(&Probe { value: 0 }, &ctx(Caller::Human));
        assert_eq!(
            report,
            Report::Ok {
                outcome: serde_json::json!({ "value": 0 })
            }
        );
    }
}
