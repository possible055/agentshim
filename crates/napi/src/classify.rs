use std::collections::HashSet;

use napi_derive::napi;

/// One runner-failure evidence rule, mirroring the DSH sandbox contract: an
/// optional exit-code gate, informational lines excluded by case-insensitive
/// full-line equality, then case-insensitive fatal signatures matched within
/// each remaining stderr line.
#[napi(object)]
pub struct RunnerFailureRule {
    pub allowed_exit_codes: Option<Vec<u32>>,
    pub fatal_signatures: Vec<String>,
    pub informational_lines: Option<Vec<String>>,
}

/// Sandbox classification inputs for one confined spawn: the backend's denial
/// dialect and runner-failure rules, exactly as the host's
/// `SandboxProvider.confine` produced them for the wrapped argv.
#[napi(object)]
pub struct SandboxAttribution {
    pub denial_signatures: Option<Vec<String>>,
    pub runner_failure_rules: Option<Vec<RunnerFailureRule>>,
}

pub(crate) struct Classification {
    pub(crate) denied: bool,
    pub(crate) runner_failed: bool,
}

/// `Number.parseInt(exit, 10)` semantics for core exit labels: leading decimal
/// digits, `None` for anything without them ("signal 9" never passes a gate).
fn parse_exit(exit: &str) -> Option<u32> {
    let digits: String = exit.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn runner_failure(exit: Option<&str>, stderr: &str, rules: &[RunnerFailureRule]) -> bool {
    if exit == Some("0") {
        return false;
    }
    rules.iter().any(|rule| {
        if let Some(allowed) = &rule.allowed_exit_codes {
            match exit.and_then(parse_exit) {
                Some(code) if allowed.contains(&code) => {}
                _ => return false,
            }
        }
        let informational: HashSet<String> = rule
            .informational_lines
            .as_ref()
            .map_or_else(HashSet::new, |lines| {
                lines.iter().map(|line| line.to_lowercase()).collect()
            });
        stderr
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .any(|line| {
                let lowered = line.to_lowercase();
                !informational.contains(&lowered)
                    && rule
                        .fatal_signatures
                        .iter()
                        .any(|signature| lowered.contains(&signature.to_lowercase()))
            })
    })
}

fn denial(exit: Option<&str>, stderr: &str, signatures: &[String]) -> bool {
    if exit == Some("0") {
        return false;
    }
    let lowered = stderr.to_lowercase();
    signatures
        .iter()
        .any(|signature| lowered.contains(&signature.to_lowercase()))
}

/// Classify one settled spawn against the confinement attribution. `exit` is
/// `None` when the process never settled (launch failure, timeout); rules
/// without an exit-code gate may still match the DSH sandbox attribution contract.
pub(crate) fn classify(
    exit: Option<&str>,
    stderr: &str,
    attribution: &SandboxAttribution,
) -> Classification {
    let runner_failed = attribution
        .runner_failure_rules
        .as_ref()
        .is_some_and(|rules| runner_failure(exit, stderr, rules));
    Classification {
        denied: !runner_failed
            && attribution
                .denial_signatures
                .as_ref()
                .is_some_and(|signatures| denial(exit, stderr, signatures)),
        runner_failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gated_rule() -> RunnerFailureRule {
        RunnerFailureRule {
            allowed_exit_codes: Some(vec![70]),
            fatal_signatures: vec!["runner failed".to_owned()],
            informational_lines: Some(vec!["notice".to_owned()]),
        }
    }

    fn gated_attribution() -> SandboxAttribution {
        SandboxAttribution {
            denial_signatures: Some(vec!["permission denied".to_owned()]),
            runner_failure_rules: Some(vec![gated_rule()]),
        }
    }

    #[test]
    fn runner_failure_requires_gate_then_informational_exclusion() {
        assert!(runner_failure(
            Some("70"),
            "notice\r\nRUNNER FAILED to start",
            &[gated_rule()],
        ));
        assert!(!runner_failure(Some("1"), "runner failed", &[gated_rule()]));
        assert!(!runner_failure(Some("70"), "notice", &[gated_rule()]));
        assert!(!runner_failure(
            Some("signal 9"),
            "runner failed",
            &[gated_rule()],
        ));
    }

    #[test]
    fn runner_failure_without_gate_matches_any_nonzero_exit() {
        let ungated = || RunnerFailureRule {
            allowed_exit_codes: None,
            fatal_signatures: vec!["bwrap: setuid".to_owned()],
            informational_lines: None,
        };
        assert!(runner_failure(
            Some("signal 9"),
            "bwrap: setuid failed",
            &[ungated()],
        ));
        assert!(!runner_failure(
            Some("0"),
            "bwrap: setuid failed",
            &[ungated()]
        ));
    }

    #[test]
    fn denial_matches_substrings_but_never_exit_zero() {
        assert!(denial(
            Some("1"),
            "write: Permission denied",
            &["permission denied".to_owned()],
        ));
        assert!(!denial(
            Some("1"),
            "ordinary command error",
            &["permission denied".to_owned()],
        ));
        assert!(!denial(
            Some("0"),
            "permission denied in harmless prose",
            &["permission denied".to_owned()],
        ));
    }

    #[test]
    fn classify_keeps_failure_denial_and_ordinary_nonzero_independent() {
        let attribution = gated_attribution();
        let runner = classify(Some("70"), "notice\nRUNNER FAILED to start", &attribution);
        assert!(runner.runner_failed && !runner.denied);
        let denied = classify(Some("1"), "write: Permission denied", &attribution);
        assert!(denied.denied && !denied.runner_failed);
        let ordinary = classify(Some("1"), "ordinary command error", &attribution);
        assert!(!ordinary.denied && !ordinary.runner_failed);
        let framed = classify(Some("1"), "runner failed", &attribution);
        assert!(!framed.denied && !framed.runner_failed);
    }

    #[test]
    fn classify_treats_an_unsettlable_process_as_gated_unknown() {
        let attribution = gated_attribution();
        let settled_never = classify(None, "runner failed", &attribution);
        assert!(!settled_never.denied && !settled_never.runner_failed);
        let ungated = SandboxAttribution {
            denial_signatures: None,
            runner_failure_rules: Some(vec![RunnerFailureRule {
                allowed_exit_codes: None,
                fatal_signatures: vec!["runner failed".to_owned()],
                informational_lines: None,
            }]),
        };
        assert!(classify(None, "RUNNER FAILED", &ungated).runner_failed);
    }

    #[test]
    fn parse_exit_takes_leading_digits_only() {
        assert_eq!(parse_exit("70"), Some(70));
        assert_eq!(parse_exit("7x"), Some(7));
        assert_eq!(parse_exit("signal 9"), None);
        assert_eq!(parse_exit(""), None);
    }
}
