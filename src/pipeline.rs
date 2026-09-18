//! The per-issue pipeline: issue in, reviewed pull request out.
//!
//! ```text
//! worktree → implement → push → open PR → CI ⇄ fix (≤ N) → review → address → hand over
//! ```
//!
//! Every stage records what it did on the task's report, including the stages
//! that go wrong, because the run's only output is that report.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::agent::{AgentRun, AgentSpec, Effort, Outcome, Role};
use crate::ci::{self, CiOutcome};
use crate::git;
use crate::github::{Check, Forge, Issue};
use crate::limits::Governor;
use crate::prompts;
use crate::repo::RepoContext;
use crate::report::{SessionRecord, TaskReport, TaskStatus};
use crate::ui;
use crate::util;
use crate::worktree::Worktree;

/// Everything the pipeline is configured with for a whole run.
#[derive(Debug, Clone)]
pub struct Settings {
    pub model: String,
    pub effort: Effort,
    pub agent_timeout: Duration,
    pub ci_timeout: Duration,
    pub ci_grace: Duration,
    pub max_ci_retries: u32,
    pub draft: bool,
    pub review: bool,
    pub worktree_root: PathBuf,
    pub run_dir: PathBuf,
    pub keep_worktrees: bool,
}

/// After pushing, give the forge a moment to register checks against the new
/// head commit so we do not read the previous commit's verdict.
const PUSH_SETTLE: Duration = Duration::from_secs(20);

pub struct Pipeline<'a> {
    pub ctx: &'a RepoContext,
    pub forge: &'a Forge,
    pub governor: &'a mut Governor,
    pub settings: &'a Settings,
}

impl<'a> Pipeline<'a> {
    /// Take one issue as far through the pipeline as it will go.
    ///
    /// Never returns an error: a failure is an outcome to report, not a reason
    /// to abandon the remaining issues.
    pub fn run_issue(&mut self, issue: &Issue) -> TaskReport {
        let started = Instant::now();
        let mut report = TaskReport::new(issue.number, issue.title.clone(), issue.url.clone());

        ui::rule(&format!("#{} {}", issue.number, ui::clip(&issue.title, 58)));

        if !issue.is_open() {
            ui::warn(&format!("issue #{} is {}", issue.number, issue.state));
            report.finish(
                TaskStatus::Skipped,
                format!("the issue is {} on GitHub", issue.state.to_lowercase()),
            );
            report.duration_secs = started.elapsed().as_secs();
            return report;
        }

        if let Err(e) = self.execute(issue, &mut report) {
            ui::error(&format!("#{}: {e:#}", issue.number));
            if report.detail.is_empty() {
                let status = if report.pr.is_some() {
                    TaskStatus::NeedsAttention
                } else {
                    TaskStatus::Failed
                };
                report.finish(status, format!("{e:#}"));
            } else {
                report.note(format!("{e:#}"));
            }
        }

        report.duration_secs = started.elapsed().as_secs();
        report
    }

    fn execute(&mut self, issue: &Issue, report: &mut TaskReport) -> Result<()> {
        let worktree = Worktree::create(
            self.ctx,
            issue.number,
            &self.settings.worktree_root,
            self.settings.keep_worktrees,
        )
        .with_context(|| format!("preparing a worktree for issue #{}", issue.number))?;
        report.branch = Some(worktree.branch.clone());

        let result = self.work(issue, report, &worktree);

        // Uncommitted changes would vanish with the worktree; keep it instead so
        // the human can look at what the agent left behind.
        let leave_worktree = match worktree.is_dirty() {
            Ok(true) => {
                report.note(format!(
                    "the agent left uncommitted changes; the worktree is preserved at {}",
                    worktree.path.display()
                ));
                true
            }
            Ok(false) => false,
            Err(e) => {
                ui::warn(&format!("could not check the worktree for changes: {e:#}"));
                true
            }
        };
        if !leave_worktree {
            worktree.cleanup();
        }

        result
    }

    fn work(&mut self, issue: &Issue, report: &mut TaskReport, wt: &Worktree) -> Result<()> {
        // ── implement ────────────────────────────────────────────────────────
        ui::step(&format!("implementing #{}", issue.number));
        let prompt =
            prompts::implement(issue, self.forge.slug(), &wt.branch, &self.ctx.base_branch);
        let run = self.session(report, "implement", &wt.path, prompt, Role::Author)?;
        let handover = run.result_text.clone();

        let commits = wt.commits_ahead(&wt.base_sha).unwrap_or(0);
        match &run.outcome {
            Outcome::Success => ui::ok(&format!(
                "agent finished with {commits} commit{}",
                if commits == 1 { "" } else { "s" }
            )),
            other if commits == 0 => {
                let detail = describe_failure(other);
                report.finish(
                    TaskStatus::Failed,
                    format!("the agent produced no commits: {detail}"),
                );
                return Ok(());
            }
            other => {
                report.note(format!(
                    "the implementation session did not finish cleanly ({}), but left {commits} commit{}",
                    describe_failure(other),
                    if commits == 1 { "" } else { "s" }
                ));
            }
        }

        if commits == 0 {
            report.finish(
                TaskStatus::Failed,
                "the agent reported success but made no commits".to_string(),
            );
            return Ok(());
        }

        self.assert_base_untouched(wt)?;

        // ── push ─────────────────────────────────────────────────────────────
        self.ensure_pushed(wt)?;

        // ── open the pull request ────────────────────────────────────────────
        ui::step("opening the pull request");
        let pr = match self.forge.find_pr_for_branch(&wt.branch)? {
            Some(existing) => {
                ui::info(&format!("reusing open PR #{}", existing.number));
                existing
            }
            None => self.forge.create_pr(
                &wt.branch,
                &self.ctx.base_branch,
                &prompts::pr_title(issue),
                &prompts::pr_body(issue, &handover),
                self.settings.draft,
            )?,
        };
        report.pr = Some(pr.number);
        report.pr_url = Some(pr.url.clone());
        ui::ok(&format!("PR #{} — {}", pr.number, pr.url));

        // Provisional verdict, so an error from here on still reports usefully.
        report.finish(
            TaskStatus::NeedsAttention,
            "the pull request is open but the run did not complete its checks".to_string(),
        );

        // ── CI, with a bounded number of fix attempts ────────────────────────
        let ci_ok = self.settle_ci(issue, report, wt, pr.number)?;
        if !ci_ok {
            return Ok(());
        }

        // ── review ───────────────────────────────────────────────────────────
        if !self.settings.review {
            report.finish(
                TaskStatus::ReadyForReview,
                "CI is green; the automated review pass was disabled".to_string(),
            );
            return Ok(());
        }

        self.review_pass(issue, report, wt, pr.number, &pr.url)?;
        Ok(())
    }

    /// Wait for CI, feeding failures back to the agent up to the retry cap.
    /// Returns whether the pull request ended up green.
    fn settle_ci(
        &mut self,
        issue: &Issue,
        report: &mut TaskReport,
        wt: &Worktree,
        pr: u64,
    ) -> Result<bool> {
        let max = self.settings.max_ci_retries;
        loop {
            ui::step(&format!("waiting for CI on PR #{pr}"));
            let outcome = ci::wait(
                self.forge,
                pr,
                self.settings.ci_timeout,
                self.settings.ci_grace,
            )?;

            match outcome {
                CiOutcome::Passed => {
                    ui::ok("CI is green");
                    return Ok(true);
                }
                CiOutcome::NoChecks => {
                    report.note("no CI is configured for this repository, so nothing was verified");
                    return Ok(true);
                }
                CiOutcome::TimedOut => {
                    report.finish(
                        TaskStatus::NeedsAttention,
                        format!(
                            "CI had not finished after {} — the pull request is open but unverified",
                            util::format_duration(self.settings.ci_timeout)
                        ),
                    );
                    return Ok(false);
                }
                CiOutcome::Failed(failures) => {
                    let names = failures
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    ui::warn(&format!("CI failed: {names}"));

                    if report.ci_fix_attempts >= max {
                        report.finish(
                            TaskStatus::NeedsAttention,
                            format!(
                                "CI is still failing after {max} fix attempt{} ({names})",
                                if max == 1 { "" } else { "s" }
                            ),
                        );
                        return Ok(false);
                    }

                    report.ci_fix_attempts += 1;
                    let attempt = report.ci_fix_attempts;
                    ui::step(&format!("CI fix attempt {attempt} of {max}"));

                    let logs = self.collect_logs(&failures);
                    let prompt = prompts::fix_ci(
                        issue,
                        &wt.branch,
                        pr,
                        &failures,
                        logs.as_deref(),
                        attempt,
                        max,
                    );
                    let label = format!("ci-fix-{attempt}");
                    let before = wt.head_sha().unwrap_or_default();
                    let run = self.session(report, &label, &wt.path, prompt, Role::Author)?;

                    if !run.outcome.is_success() {
                        report.note(format!(
                            "CI fix attempt {attempt} did not finish cleanly: {}",
                            describe_failure(&run.outcome)
                        ));
                    }

                    let after = wt.head_sha().unwrap_or_default();
                    if after == before {
                        report.finish(
                            TaskStatus::NeedsAttention,
                            format!(
                                "the agent made no further commits while CI was failing ({names}); stopping rather than looping"
                            ),
                        );
                        return Ok(false);
                    }

                    self.assert_base_untouched(wt)?;
                    self.ensure_pushed(wt)?;
                    std::thread::sleep(PUSH_SETTLE);
                }
            }
        }
    }

    /// Review the pull request, and action the findings exactly once.
    fn review_pass(
        &mut self,
        issue: &Issue,
        report: &mut TaskReport,
        wt: &Worktree,
        pr: u64,
        pr_url: &str,
    ) -> Result<()> {
        ui::step(&format!("reviewing PR #{pr}"));
        let prompt = prompts::review(issue, self.forge.slug(), pr, pr_url);
        let run = self.session(report, "review", &wt.path, prompt, Role::Reviewer)?;
        report.reviewed = true;

        if !run.outcome.is_success() {
            report.note(format!(
                "the review pass did not finish cleanly: {}",
                describe_failure(&run.outcome)
            ));
            report.finish(
                TaskStatus::NeedsAttention,
                "CI is green, but the automated review pass failed to complete".to_string(),
            );
            return Ok(());
        }

        let findings = run.result_text.trim().to_string();
        if prompts::review_is_clean(&findings) {
            ui::ok("review found nothing to change");
            if let Err(e) = self
                .forge
                .comment_pr(pr, &prompts::review_comment(&findings, None))
            {
                ui::warn(&format!("could not post the review comment: {e:#}"));
            }
            report.finish(
                TaskStatus::ReadyForReview,
                "CI is green and the automated review found nothing to change".to_string(),
            );
            return Ok(());
        }

        ui::info("review raised findings; addressing them once");
        let prompt = prompts::address_review(issue, &wt.branch, pr, &findings);
        let before = wt.head_sha().unwrap_or_default();
        let fix = self.session(report, "review-fix", &wt.path, prompt, Role::Author)?;
        let response = fix.result_text.trim().to_string();

        if let Err(e) = self
            .forge
            .comment_pr(pr, &prompts::review_comment(&findings, Some(&response)))
        {
            ui::warn(&format!("could not post the review comment: {e:#}"));
        }

        if !fix.outcome.is_success() {
            report.note(format!(
                "the review follow-up did not finish cleanly: {}",
                describe_failure(&fix.outcome)
            ));
        }

        let after = wt.head_sha().unwrap_or_default();
        if after == before {
            report.note(
                "the review follow-up made no commits — the agent either declined the findings or could not act on them",
            );
            report.finish(
                TaskStatus::NeedsAttention,
                "the automated review raised findings that were not acted on".to_string(),
            );
            return Ok(());
        }

        self.assert_base_untouched(wt)?;
        self.ensure_pushed(wt)?;
        std::thread::sleep(PUSH_SETTLE);

        // The review changed the code, so the previous green verdict no longer
        // applies. Re-check once; failures now are for the human, not another
        // round of automated fixing.
        ui::step("re-checking CI after the review changes");
        match ci::wait(
            self.forge,
            pr,
            self.settings.ci_timeout,
            self.settings.ci_grace,
        )? {
            CiOutcome::Passed | CiOutcome::NoChecks => {
                report.finish(
                    TaskStatus::ReadyForReview,
                    "CI is green and the automated review findings were addressed".to_string(),
                );
            }
            CiOutcome::Failed(failures) => {
                let names = failures
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                report.finish(
                    TaskStatus::NeedsAttention,
                    format!("CI broke after the review changes were applied ({names})"),
                );
            }
            CiOutcome::TimedOut => {
                report.finish(
                    TaskStatus::NeedsAttention,
                    "CI had not finished after the review changes were applied".to_string(),
                );
            }
        }
        Ok(())
    }

    /// Run one agent session through the governor and record it on the report.
    fn session(
        &mut self,
        report: &mut TaskReport,
        label: &str,
        cwd: &std::path::Path,
        prompt: String,
        role: Role,
    ) -> Result<AgentRun> {
        let task_dir = self
            .settings
            .run_dir
            .join(format!("issue-{}", report.issue));
        let spec = AgentSpec {
            label: label.to_string(),
            cwd: cwd.to_path_buf(),
            prompt,
            model: self.settings.model.clone(),
            effort: self.settings.effort,
            timeout: self.settings.agent_timeout,
            role,
            transcript: crate::agent::transcript_path(&task_dir, label),
        };

        let run = self.governor.run(&spec)?;
        ui::info(&run.summary_line());

        report.cost_usd += run.cost_usd;
        report.sessions.push(SessionRecord {
            label: run.label.clone(),
            outcome: describe_failure(&run.outcome),
            turns: run.num_turns,
            cost_usd: run.cost_usd,
            duration_secs: run.duration.as_secs(),
            transcript: run.transcript.clone(),
            session_id: run.session_id.clone(),
        });
        Ok(run)
    }

    /// Push the branch if the remote is behind local HEAD.
    fn ensure_pushed(&self, wt: &Worktree) -> Result<()> {
        if wt.is_pushed(&self.ctx.remote).unwrap_or(false) {
            ui::trace("branch is already up to date on the remote");
            return Ok(());
        }
        ui::info(&format!("pushing {}", wt.branch));
        wt.push(&self.ctx.remote)
    }

    /// The second half of the base-branch guard.
    ///
    /// The agent is denied the commands that would let it push to the base
    /// branch; this checks that the base branch did not in fact grow any of our
    /// commits, in case it found a way around them.
    fn assert_base_untouched(&self, wt: &Worktree) -> Result<()> {
        let base = self.ctx.base_branch.clone();
        if let Err(e) = crate::worktree::fetch_base(self.ctx) {
            ui::trace(&format!(
                "could not refresh the base branch for the safety check: {e:#}"
            ));
            return Ok(());
        }

        let base_ref = self.ctx.base_ref();
        let Ok(now) = git::run(&self.ctx.root, &["rev-parse", &base_ref]) else {
            return Ok(());
        };
        if now == wt.base_sha {
            return Ok(());
        }

        let ours: HashSet<String> = wt
            .new_commits(&wt.base_sha)
            .unwrap_or_default()
            .into_iter()
            .collect();
        if ours.is_empty() {
            return Ok(());
        }

        let range = format!("{}..{}", wt.base_sha, base_ref);
        let added = git::run(&self.ctx.root, &["rev-list", &range]).unwrap_or_default();
        let leaked: Vec<&str> = added
            .lines()
            .map(str::trim)
            .filter(|sha| ours.contains(*sha))
            .collect();

        if leaked.is_empty() {
            ui::trace(&format!(
                "`{base}` moved on the remote while we worked; none of our commits are on it"
            ));
            return Ok(());
        }

        bail!(
            "safety violation: {} commit(s) from `{}` reached `{base}` directly ({}). rain stopped this task; inspect the branch before doing anything else",
            leaked.len(),
            wt.branch,
            leaked
                .iter()
                .map(|s| &s[..s.len().min(8)])
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    /// Best-effort log tail from the first failing check, for the run report and
    /// the fix prompt.
    fn collect_logs(&self, failures: &[Check]) -> Option<String> {
        failures
            .iter()
            .find_map(|check| self.forge.failed_run_log(check, 150))
    }
}

fn describe_failure(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Success => "success".to_string(),
        Outcome::Timeout => "timed out".to_string(),
        Outcome::UsageLimit(limit) => format!("stopped by the {}", limit.kind.label()),
        Outcome::Failed(reason) => format!("failed: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{LimitKind, UsageLimit};

    #[test]
    fn describes_each_outcome() {
        assert_eq!(describe_failure(&Outcome::Success), "success");
        assert_eq!(describe_failure(&Outcome::Timeout), "timed out");
        assert_eq!(
            describe_failure(&Outcome::Failed("boom".into())),
            "failed: boom"
        );
        assert_eq!(
            describe_failure(&Outcome::UsageLimit(UsageLimit {
                kind: LimitKind::Weekly,
                resets_at: None,
                message: String::new(),
            })),
            "stopped by the weekly limit"
        );
    }
}
