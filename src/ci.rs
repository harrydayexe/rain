//! Waiting on CI.
//!
//! Polling is bounded on both ends: a grace period at the start, because checks
//! take a moment to register after a PR is opened, and a hard timeout, because
//! an unattended run must never wait forever.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::github::{Check, ChecksSnapshot, Forge};
use crate::ui;
use crate::util;

#[derive(Debug, Clone)]
pub enum CiOutcome {
    Passed,
    Failed(Vec<Check>),
    /// No check suite ever reported — the repository has no CI on this branch.
    NoChecks,
    /// Checks were still running when the budget ran out.
    TimedOut,
}

const FIRST_POLL: Duration = Duration::from_secs(15);
const MAX_POLL: Duration = Duration::from_secs(60);

/// Poll `gh pr checks` until every check settles, or the budget expires.
///
/// `grace` is how long to keep waiting when nothing has reported yet before
/// concluding the repository simply has no CI.
pub fn wait(forge: &Forge, pr: u64, timeout: Duration, grace: Duration) -> Result<CiOutcome> {
    let started = Instant::now();
    let mut interval = FIRST_POLL;
    let mut last_report = String::new();

    loop {
        let snapshot = forge.pr_checks(pr)?;
        let elapsed = started.elapsed();

        match snapshot {
            ChecksSnapshot::NotReported => {
                if elapsed >= grace {
                    ui::warn(&format!(
                        "no CI checks reported on PR #{pr} after {} — treating the pull request as having no CI",
                        util::format_duration(elapsed)
                    ));
                    return Ok(CiOutcome::NoChecks);
                }
                ui::trace("no checks have reported yet");
            }
            ChecksSnapshot::Reported(checks) => {
                let report = tally(&checks);
                if report != last_report {
                    ui::info(&format!("checks: {report}"));
                    last_report = report;
                }

                if !checks.iter().any(is_pending) {
                    let failures: Vec<Check> =
                        checks.iter().filter(|c| is_failure(c)).cloned().collect();
                    return Ok(if failures.is_empty() {
                        CiOutcome::Passed
                    } else {
                        CiOutcome::Failed(failures)
                    });
                }
            }
        }

        if elapsed >= timeout {
            ui::warn(&format!(
                "CI on PR #{pr} did not finish within {}",
                util::format_duration(timeout)
            ));
            return Ok(CiOutcome::TimedOut);
        }

        let remaining = timeout.saturating_sub(elapsed);
        std::thread::sleep(interval.min(remaining));
        interval = (interval.mul_f32(1.5)).min(MAX_POLL);
    }
}

fn is_pending(check: &Check) -> bool {
    check.bucket == "pending"
}

fn is_failure(check: &Check) -> bool {
    matches!(check.bucket.as_str(), "fail" | "cancel")
}

/// `3 pass, 1 fail, 2 pending` — the shape of the suite, for the live log.
fn tally(checks: &[Check]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for check in checks {
        *counts.entry(check.bucket.as_str()).or_default() += 1;
    }
    // Report in a stable, meaningful order rather than alphabetically.
    ["fail", "cancel", "pending", "pass", "skipping"]
        .into_iter()
        .filter_map(|bucket| counts.get(bucket).map(|n| format!("{n} {bucket}")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, bucket: &str) -> Check {
        Check {
            name: name.into(),
            bucket: bucket.into(),
            state: String::new(),
            link: String::new(),
            description: String::new(),
            workflow: String::new(),
        }
    }

    #[test]
    fn counts_buckets_in_priority_order() {
        let checks = vec![
            check("a", "pass"),
            check("b", "fail"),
            check("c", "pending"),
            check("d", "pass"),
        ];
        assert_eq!(tally(&checks), "1 fail, 1 pending, 2 pass");
    }

    #[test]
    fn cancelled_checks_count_as_failures() {
        assert!(is_failure(&check("a", "cancel")));
        assert!(is_failure(&check("a", "fail")));
        assert!(!is_failure(&check("a", "skipping")));
        assert!(!is_failure(&check("a", "pass")));
    }

    #[test]
    fn only_pending_blocks_a_verdict() {
        assert!(is_pending(&check("a", "pending")));
        assert!(!is_pending(&check("a", "pass")));
    }
}
