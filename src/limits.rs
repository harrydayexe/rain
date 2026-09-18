//! Surviving subscription limits.
//!
//! rain runs on a Claude subscription, where the scarce resource is quota
//! rather than money. Hitting a cap is expected, not exceptional: the right
//! response is to sleep until the window resets and carry on where we left off.

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;

use crate::agent::{self, AgentRun, AgentSpec, LimitKind, Outcome, UsageLimit};
use crate::ui;
use crate::util;

/// Backoff when Claude Code does not tell us when the limit resets.
const BLIND_BACKOFF: &[Duration] = &[
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
];

/// A minute past the stated reset, so we never race the boundary.
const RESET_BUFFER: Duration = Duration::from_secs(60);

/// Never sleep for less than this, even if the reset time has nominally passed.
const MIN_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Policy {
    /// When false, a limit ends the run instead of pausing it.
    pub wait_on_limit: bool,
    /// Total time rain is allowed to spend asleep across the whole run.
    pub max_total_wait: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            wait_on_limit: true,
            max_total_wait: Duration::from_secs(8 * 60 * 60),
        }
    }
}

/// Runs agent sessions, absorbing usage limits on their behalf.
pub struct Governor {
    policy: Policy,
    slept: Duration,
    spend: f64,
    sessions: u32,
}

impl Governor {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            slept: Duration::ZERO,
            spend: 0.0,
            sessions: 0,
        }
    }

    /// Total notional cost across every session this run — rain's best proxy
    /// for quota consumed, even though no money changes hands on a subscription.
    pub fn spend(&self) -> f64 {
        self.spend
    }

    pub fn sessions(&self) -> u32 {
        self.sessions
    }

    pub fn slept(&self) -> Duration {
        self.slept
    }

    /// Run one agent session, sleeping through any usage limit it hits.
    ///
    /// Returns the last run. If the outcome is still `UsageLimit`, rain has
    /// given up waiting and the caller should record the task as blocked.
    pub fn run(&mut self, spec: &AgentSpec) -> Result<AgentRun> {
        let mut blind_attempt = 0usize;
        loop {
            let run = agent::run(spec)?;
            self.sessions += 1;
            self.spend += run.cost_usd;

            let Outcome::UsageLimit(limit) = &run.outcome else {
                return Ok(run);
            };

            ui::warn(&format!("{} hit: {}", limit.kind.label(), limit.message));

            if !self.policy.wait_on_limit {
                ui::warn("--no-wait-on-limit is set; not sleeping");
                return Ok(run);
            }

            let wait = plan_wait(limit, blind_attempt);
            if limit.resets_at.is_none() {
                blind_attempt += 1;
            }

            let remaining_budget = self.policy.max_total_wait.saturating_sub(self.slept);
            if wait > remaining_budget {
                ui::error(&format!(
                    "waiting {} would exceed this run's {} sleep budget — stopping here",
                    util::format_duration(wait),
                    util::format_duration(self.policy.max_total_wait)
                ));
                return Ok(run);
            }

            sleep_visibly(wait, limit);
            self.slept += wait;
        }
    }
}

/// How long to wait for a limit to clear.
fn plan_wait(limit: &UsageLimit, blind_attempt: usize) -> Duration {
    if let Some(reset) = limit.resets_at {
        let delta = reset.signed_duration_since(Utc::now());
        let secs = delta.num_seconds().max(0) as u64;
        return Duration::from_secs(secs)
            .saturating_add(RESET_BUFFER)
            .max(MIN_WAIT);
    }

    // No reset time. Escalate, and lean longer for a weekly cap, which cannot
    // possibly clear in five minutes.
    let idx = blind_attempt.min(BLIND_BACKOFF.len() - 1);
    let base = BLIND_BACKOFF[idx];
    if limit.kind == LimitKind::Weekly {
        base.max(Duration::from_secs(60 * 60))
    } else {
        base
    }
}

/// Sleep, reporting progress, so a paused run never looks like a hung one.
fn sleep_visibly(total: Duration, limit: &UsageLimit) {
    let until = match limit.resets_at {
        Some(reset) => format!(
            " (resets {})",
            reset
                .with_timezone(&chrono::Local)
                .format("%H:%M on %a %e %b")
        ),
        None => String::new(),
    };
    ui::step(&format!(
        "sleeping {} for the {}{until}",
        util::format_duration(total),
        limit.kind.label()
    ));

    let tick = Duration::from_secs(60);
    let mut left = total;
    let mut since_report = Duration::ZERO;
    while !left.is_zero() {
        let chunk = tick.min(left);
        std::thread::sleep(chunk);
        left -= chunk;
        since_report += chunk;
        if since_report >= Duration::from_secs(5 * 60) && !left.is_zero() {
            ui::info(&format!("{} left to wait", util::format_duration(left)));
            since_report = Duration::ZERO;
        }
    }
    ui::ok("resuming");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn limit(kind: LimitKind, resets_in: Option<i64>) -> UsageLimit {
        UsageLimit {
            kind,
            resets_at: resets_in.map(|s| Utc::now() + ChronoDuration::seconds(s)),
            message: String::new(),
        }
    }

    #[test]
    fn waits_until_the_stated_reset_plus_a_buffer() {
        let wait = plan_wait(&limit(LimitKind::Session, Some(1800)), 0);
        assert!(wait >= Duration::from_secs(1800));
        assert!(wait <= Duration::from_secs(1800 + 70));
    }

    #[test]
    fn never_waits_less_than_a_minute() {
        // A reset time already in the past still gets a floor.
        let wait = plan_wait(&limit(LimitKind::Session, Some(-10_000)), 0);
        assert!(wait >= MIN_WAIT);
    }

    #[test]
    fn escalates_when_blind() {
        let l = limit(LimitKind::Unknown, None);
        assert_eq!(plan_wait(&l, 0), BLIND_BACKOFF[0]);
        assert_eq!(plan_wait(&l, 1), BLIND_BACKOFF[1]);
        assert_eq!(plan_wait(&l, 99), *BLIND_BACKOFF.last().unwrap());
    }

    #[test]
    fn blind_weekly_limits_wait_at_least_an_hour() {
        let wait = plan_wait(&limit(LimitKind::Weekly, None), 0);
        assert!(wait >= Duration::from_secs(3600));
    }
}
