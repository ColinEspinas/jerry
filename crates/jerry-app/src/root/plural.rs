//! The one pluralisation helper every user-visible count in this window goes through.

/// Picks the wording that matches `count`: `singular` at exactly one, `plural` at everything
/// else (**including zero** - see this module's docs).
pub(crate) fn form<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

/// The count and its noun, agreeing: `"0 files"`, `"1 file"`, `"2 files"`.
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

    #[test]
    fn explicit_plural_is_not_derived_from_the_singular() {
        assert_eq!(count(1, "person", Some("people")), "1 person");
        assert_eq!(count(3, "person", Some("people")), "3 people");
    }

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
