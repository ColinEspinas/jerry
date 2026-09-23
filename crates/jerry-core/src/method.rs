//! JSON-RPC method names, namespaced by kind: `command/<name>`, `validate/<name>`,
//! `query/<name>`, `event/<name>`, and the bare `hook`.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Command(String),
    Validate(String),
    Query(String),
    Event(String),
    Hook,
    /// The one client-to-host request in the `event/` namespace, spelled `event/subscribe` -
    /// every other `Event(_)` name is host-to-client only (`Request::from_wire` refuses them as
    /// incoming). Registers the connection as a fanout sink (`docs/architecture/decisions.md`
    /// §24); never carries a payload.
    Subscribe,
}

impl Method {
    /// `None` for anything outside the namespaces or with a name that is not kebab-case.
    pub fn parse(method: &str) -> Option<Method> {
        if method == "hook" {
            return Some(Method::Hook);
        }
        if method == "event/subscribe" {
            return Some(Method::Subscribe);
        }
        let (kind, name) = method.split_once('/')?;
        if !is_kebab(name) {
            return None;
        }
        let name = name.to_owned();
        Some(match kind {
            "command" => Method::Command(name),
            "validate" => Method::Validate(name),
            "query" => Method::Query(name),
            "event" => Method::Event(name),
            _ => return None,
        })
    }
}

fn is_kebab(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Method::Command(name) => write!(f, "command/{name}"),
            Method::Validate(name) => write!(f, "validate/{name}"),
            Method::Query(name) => write!(f, "query/{name}"),
            Method::Event(name) => write!(f, "event/{name}"),
            Method::Hook => f.write_str("hook"),
            Method::Subscribe => f.write_str("event/subscribe"),
        }
    }
}

#[cfg(test)]
mod method_name_tests {
    use super::Method;

    #[test]
    fn every_namespace_parses_and_prints_back_to_itself() {
        for text in [
            "command/merge-attempt",
            "validate/merge-attempt",
            "query/status",
            "event/session-exited",
            "hook",
            "event/subscribe",
        ] {
            let method = Method::parse(text).unwrap_or_else(|| panic!("{text} must parse"));
            assert_eq!(method.to_string(), text);
        }
    }

    /// `event/subscribe` is the one name in the `event/` namespace a client may send as a
    /// request - every other `event/*` name is `Method::Event`, host-to-client only.
    #[test]
    fn only_the_literal_subscribe_name_gets_its_own_variant() {
        assert_eq!(Method::parse("event/subscribe"), Some(Method::Subscribe));
        assert_eq!(
            Method::parse("event/session-exited"),
            Some(Method::Event("session-exited".into()))
        );
    }

    #[test]
    fn unknown_namespaces_and_non_kebab_names_are_rejected() {
        for text in [
            "",
            "merge-attempt",
            "hook/x",
            "rpc/discover",
            "command/",
            "command/Merge",
            "command/merge_attempt",
            "command/-x",
            "query/a/b",
        ] {
            assert_eq!(Method::parse(text), None, "{text:?} must not parse");
        }
    }
}
