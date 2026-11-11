//! Who may approve a plan, and how.
//!
//! A plan can come from an agent, and the shell that made it could also run
//! `apply`. So an approval is either a person's answer on a terminal, or the
//! plan's full digest passed on purpose with `--approve`.

use codemode::Digest;

/// What `apply` does about approval before it asks anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Approval {
    /// `--approve` names this plan's digest.
    Given,
    /// Both ends are a terminal and no digest was passed: ask the person.
    Ask,
    /// Not approved, with what to tell the person.
    Refused(String),
}

/// Decide from `--approve`, the plan's digest, and whether a person is at a
/// terminal. `file` is the plan file as the person named it.
pub(crate) fn approval(
    approve: Option<&str>,
    digest: &Digest,
    terminal: bool,
    file: &str,
) -> Approval {
    match approve {
        Some(given) if given == digest.as_str() => Approval::Given,
        Some(given) => Approval::Refused(format!(
            "not approved. --approve {} is not this plan's digest:\n  {digest}\n\
             Review the plan again with `codemode status {file}`. Nothing was sent.",
            codemode::render::escape(given)
        )),
        None if terminal => Approval::Ask,
        None => Approval::Refused(format!(
            "not approved. A person must approve this plan:\n  \
             run `codemode apply {file}` in a terminal and answer the question, or\n  \
             pass `--approve {digest}`\n\
             Nothing was sent."
        )),
    }
}

/// Whether the line a person typed approves: only the exact answer `yes`,
/// without its line ending.
pub(crate) fn answer_approves(line: &str) -> bool {
    line.strip_suffix('\n')
        .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line))
        == "yes"
}

/// The whole decision with the person's answer supplied by `ask`, which is
/// called only when the answer is needed. Returns why the plan is not
/// approved, if it is not.
#[cfg(test)]
fn decide(
    approve: Option<&str>,
    digest: &Digest,
    terminal: bool,
    ask: impl FnOnce() -> String,
) -> Result<(), String> {
    match approval(approve, digest, terminal, "p.plan.json") {
        Approval::Given => Ok(()),
        Approval::Ask if answer_approves(&ask()) => Ok(()),
        Approval::Ask => Err(NOT_YES.into()),
        Approval::Refused(message) => Err(message),
    }
}

/// What `apply` says when the answer was not `yes`.
pub(crate) const NOT_YES: &str = "not approved. The answer was not \"yes\". Nothing was sent.";

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> Digest {
        Digest::of(b"plan")
    }

    fn never() -> String {
        panic!("no question may be asked here")
    }

    #[test]
    fn on_a_terminal_only_the_exact_answer_yes_approves() {
        let digest = digest();
        for answer in ["yes\n", "yes\r\n", "yes"] {
            assert_eq!(
                decide(None, &digest, true, || answer.into()),
                Ok(()),
                "{answer:?}"
            );
        }
        for answer in [
            "",
            "\n",
            "y\n",
            "Yes\n",
            "YES\n",
            " yes\n",
            "yes \n",
            "yes\n\n",
            "no\n",
            "yes please\n",
        ] {
            assert_eq!(
                decide(None, &digest, true, || answer.into()),
                Err(NOT_YES.to_string()),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn without_a_terminal_nobody_is_asked_and_the_full_digest_is_required() {
        let digest = digest();
        let refused = decide(None, &digest, false, never).expect_err("refused");
        assert!(
            refused.contains("A person must approve this plan"),
            "{refused}"
        );
        assert!(refused.contains("codemode apply p.plan.json"), "{refused}");
        assert!(
            refused.contains(&format!("--approve {digest}`")),
            "{refused}"
        );
        assert_eq!(decide(Some(digest.as_str()), &digest, false, never), Ok(()));
    }

    #[test]
    fn a_digest_is_accepted_on_a_terminal_without_a_question() {
        let digest = digest();
        assert_eq!(decide(Some(digest.as_str()), &digest, true, never), Ok(()));
    }

    #[test]
    fn a_wrong_or_short_digest_is_refused_everywhere() {
        let digest = digest();
        let other = Digest::of(b"another plan");
        let short = format!("sha256:{}", digest.short());
        for terminal in [false, true] {
            for given in [other.as_str(), short.as_str(), digest.short(), "", "yes"] {
                let refused = decide(Some(given), &digest, terminal, never).expect_err("refused");
                assert!(refused.contains("is not this plan's digest"), "{refused}");
                assert!(refused.contains("Nothing was sent."), "{refused}");
            }
        }
    }
}
