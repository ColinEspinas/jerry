//! The one pluralisation helper every user-visible count in this window goes through.
//!
//! ## The rule
//!
//! > **Every count goes through the pluralisation helper; never inline a ternary.**
//!
//! (Revision 6, `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §7 rule 9,
//! restating §6 and `REVISION-2026-08-13.md` §8a.)
//!
//! Before this module existed, every counter in the app conjugated itself: a dozen copies of
//! `if count == 1 { "" } else { "s" }`, plus a second, worse population of labels that simply
//! hardcoded the plural noun (`format!("{agent_count} agents")`) and so read `1 agents` for the
//! single-agent case that is in fact the *common* one. The design review caught exactly this in
//! its own mock - "the two that were written by hand both said `1 files`" - which is the point:
//! a rule spread across thirty call sites is a rule that drifts. There is one decision point
//! here ([`form`]) and everything else is built on it.
//!
//! ## Using it
//!
//! For a count plus its noun, call [`count`]:
//!
//! ```ignore
//! plural::count(n, "file", None)            // "0 files" / "1 file" / "2 files"
//! plural::count(n, "match", Some("matches")) // irregular plural spelled out
//! ```
//!
//! For anything else that has to agree with a count - a verb, an auxiliary, a pronoun - call
//! [`form`] directly, so the sentence still has exactly one place that looks at the number:
//!
//! ```ignore
//! format!("{} {} input", plural::count(n, "agent", None), plural::form(n, "needs", "need"))
//! // "1 agent needs input" / "2 agents need input"
//! ```
//!
//! [`count`] is itself implemented in terms of [`form`], so noun agreement and verb agreement
//! are genuinely one code path and cannot disagree about where the boundary is.
//!
//! ## Zero is plural
//!
//! English pluralises zero (`0 files`, not `0 file`), so [`form`] keys off `count == 1` alone,
//! not off `count > 1`. Call sites that want to *hide* a counter at zero, or say something
//! qualitatively different there (`"nothing unreviewed"`, `"nothing to prune"`), still do that
//! themselves - that is a content decision, not a grammar one, and this helper deliberately
//! does not try to own it.
//!
//! ## What does *not* belong here
//!
//! Labels with no noun to agree with (`"3 wt"`, `"12 shown"`, `"hunk 2 of 5"`, the
//! `changes-header-{n}-files` debug selectors) have nothing to conjugate and must stay as they
//! are - routing them through here would only invent a plural where the UI deliberately has
//! none.

/// Picks the wording that matches `count`: `singular` at exactly one, `plural` at everything
/// else (**including zero** - see this module's docs).
///
/// This is the single place in the app that decides singular versus plural. [`count`] is built
/// on it, and sentence-level agreement (verbs, auxiliaries) calls it directly rather than
/// re-deriving the same `== 1` test at the call site.
pub(crate) fn form<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

/// The count and its noun, agreeing: `"0 files"`, `"1 file"`, `"2 files"`.
///
/// `plural` is `None` for the regular English plural (`singular` with `s` appended) and
/// `Some(..)` for anything that does not form its plural that way (`"matches"`, `"people"`,
/// `"branches"`). Passing `None` is not a shortcut for "probably fine" - it is a claim that the
/// noun really is regular, so spell out irregulars rather than letting one read wrong.
pub(crate) fn count(count: usize, singular: &str, plural: Option<&str>) -> String {
    let regular_plural = format!("{singular}s");
    format!(
        "{count} {}",
        form(count, singular, plural.unwrap_or(&regular_plural))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_picks_singular_only_at_exactly_one() {
        assert_eq!(form(0, "needs", "need"), "need");
        assert_eq!(form(1, "needs", "need"), "needs");
        assert_eq!(form(2, "needs", "need"), "need");
        assert_eq!(form(7, "needs", "need"), "need");
    }

    #[test]
    fn count_conjugates_a_regular_noun_at_zero_one_and_two() {
        assert_eq!(count(0, "file", None), "0 files");
        assert_eq!(count(1, "file", None), "1 file");
        assert_eq!(count(2, "file", None), "2 files");
        assert_eq!(count(37, "file", None), "37 files");
    }

    #[test]
    fn count_uses_the_explicit_plural_when_one_is_given() {
        assert_eq!(count(0, "match", Some("matches")), "0 matches");
        assert_eq!(count(1, "match", Some("matches")), "1 match");
        assert_eq!(count(2, "match", Some("matches")), "2 matches");
    }

    /// The explicit plural is used verbatim - it is not `singular` with something appended - so
    /// a genuinely irregular word (no shared stem at all) survives the round trip.
    #[test]
    fn explicit_plural_is_not_derived_from_the_singular() {
        assert_eq!(count(1, "person", Some("people")), "1 person");
        assert_eq!(count(3, "person", Some("people")), "3 people");
    }

    /// The sentence-conjugation path the title bar needs, exercised through the same two
    /// functions every other call site uses - there is no second code path for verbs.
    #[test]
    fn count_and_form_compose_into_an_agreeing_sentence() {
        let sentence = |n: usize| {
            format!(
                "{} {} input",
                count(n, "agent", None),
                form(n, "needs", "need")
            )
        };
        assert_eq!(sentence(0), "0 agents need input");
        assert_eq!(sentence(1), "1 agent needs input");
        assert_eq!(sentence(2), "2 agents need input");
    }
}
