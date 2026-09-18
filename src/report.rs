//! What happened, in a form a human can act on.
//!
//! The point of the whole run is the summary at the end: which pull requests
//! are waiting for review, and which issues need a person.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;

use crate::agent::Quota;
use crate::ui;
use crate::util;

/// Where a task ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// PR open, CI green (or absent), review pass complete.
    ReadyForReview,
    /// PR exists but something needs a person — failing CI, an unactioned
    /// review, a timeout.
    NeedsAttention,
    /// No pull request was produced.
    Failed,
    /// Deliberately not attempted.
    Skipped,
}

impl TaskStatus {
    fn heading(self) -> &'static str {
        match self {
            TaskStatus::ReadyForReview => "Ready for review",
            TaskStatus::NeedsAttention => "Needs attention",
            TaskStatus::Failed => "Not completed",
            TaskStatus::Skipped => "Skipped",
        }
    }

    fn marker(self) -> String {
        match self {
            TaskStatus::ReadyForReview => ui::green("✓"),
            TaskStatus::NeedsAttention => ui::yellow("!"),
            TaskStatus::Failed => ui::red("✗"),
            TaskStatus::Skipped => ui::dim("–"),
        }
    }
}

/// One Claude Code session, as recorded in the run report.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRecord {
    pub label: String,
    pub outcome: String,
    pub turns: u64,
    pub cost_usd: f64,
    pub duration_secs: u64,
    pub transcript: PathBuf,
    /// Claude Code's own session ID, so a run can be resumed or inspected with
    /// `claude --resume`.
    pub session_id: Option<String>,
}

/// One issue's journey through the pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct TaskReport {
    pub issue: u64,
    pub title: String,
    pub issue_url: String,
    pub status: TaskStatus,
    /// One line explaining the status.
    pub detail: String,
    pub branch: Option<String>,
    /// The branch this task's pull request targets, which is not always the
    /// run's base branch.
    pub base_branch: Option<String>,
    /// Whether `branch` was already linked to the issue on GitHub.
    pub branch_was_linked: bool,
    pub pr: Option<u64>,
    pub pr_url: Option<String>,
    pub ci_fix_attempts: u32,
    pub reviewed: bool,
    pub notes: Vec<String>,
    pub sessions: Vec<SessionRecord>,
    pub cost_usd: f64,
    pub duration_secs: u64,
}

impl TaskReport {
    pub fn new(issue: u64, title: impl Into<String>, issue_url: impl Into<String>) -> Self {
        Self {
            issue,
            title: title.into(),
            issue_url: issue_url.into(),
            status: TaskStatus::Failed,
            detail: String::new(),
            branch: None,
            base_branch: None,
            branch_was_linked: false,
            pr: None,
            pr_url: None,
            ci_fix_attempts: 0,
            reviewed: false,
            notes: Vec::new(),
            sessions: Vec::new(),
            cost_usd: 0.0,
            duration_secs: 0,
        }
    }

    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn finish(&mut self, status: TaskStatus, detail: impl Into<String>) {
        self.status = status;
        self.detail = detail.into();
    }
}

/// The whole run.
#[derive(Debug, Serialize)]
pub struct RunReport {
    pub repo: String,
    pub base_branch: String,
    pub model: String,
    pub effort: String,
    pub started_at: DateTime<Local>,
    pub finished_at: DateTime<Local>,
    pub queue: Vec<u64>,
    pub queue_warnings: Vec<String>,
    pub tasks: Vec<TaskReport>,
    pub total_cost_usd: f64,
    pub total_sessions: u32,
    pub slept_secs: u64,
    /// The last quota reading of the run — what rain left for the human.
    pub quota: Option<Quota>,
    pub run_dir: PathBuf,
}

impl RunReport {
    pub fn tasks_with(&self, status: TaskStatus) -> Vec<&TaskReport> {
        self.tasks.iter().filter(|t| t.status == status).collect()
    }

    /// Exit code: zero only when everything landed ready for review.
    pub fn exit_code(&self) -> i32 {
        if self
            .tasks
            .iter()
            .all(|t| t.status == TaskStatus::ReadyForReview)
        {
            0
        } else {
            1
        }
    }

    /// Print the closing summary to the terminal.
    pub fn print(&self) {
        ui::rule("Summary");

        for status in [
            TaskStatus::ReadyForReview,
            TaskStatus::NeedsAttention,
            TaskStatus::Failed,
            TaskStatus::Skipped,
        ] {
            let tasks = self.tasks_with(status);
            if tasks.is_empty() {
                continue;
            }
            ui::blank();
            ui::info(&ui::bold(&format!(
                "{} ({})",
                status.heading(),
                tasks.len()
            )));
            for task in tasks {
                let target = match (&task.pr, &task.pr_url) {
                    (Some(n), Some(url)) => format!("PR #{n} — {url}"),
                    _ => task.issue_url.clone(),
                };
                ui::info(&format!(
                    "  {} #{} {}",
                    status.marker(),
                    task.issue,
                    ui::clip(&task.title, 68)
                ));
                ui::info(&format!("      {}", ui::dim(&target)));
                if !task.detail.is_empty() {
                    ui::info(&format!("      {}", task.detail));
                }
                for note in &task.notes {
                    ui::info(&format!("      {}", ui::dim(&format!("· {note}"))));
                }
            }
        }

        ui::blank();
        ui::info(&format!(
            "{} sessions across {} issues, {} of notional cost, {} spent waiting on limits",
            self.total_sessions,
            self.tasks.len(),
            ui::bold(&format!("${:.2}", self.total_cost_usd)),
            util::format_duration(Duration::from_secs(self.slept_secs)),
        ));
        if let Some(line) = self.quota.as_ref().and_then(Quota::summary_line) {
            ui::info(&format!("quota: {line}"));
        }
        ui::info(&format!("full run record: {}", self.run_dir.display()));
        ui::blank();
    }

    /// Persist `run.json` and `summary.md` next to the transcripts.
    pub fn persist(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let json = serde_json::to_string_pretty(self).context("serialising the run report")?;
        std::fs::write(dir.join("run.json"), json).context("writing run.json")?;
        std::fs::write(dir.join("summary.md"), self.markdown()).context("writing summary.md")?;
        Ok(())
    }

    pub fn markdown(&self) -> String {
        let mut md = String::new();
        md.push_str(&format!("# rain run — {}\n\n", self.repo));
        md.push_str(&format!(
            "- Started: {}\n- Finished: {}\n- Base branch: `{}`\n- Model: `{}` (effort `{}`)\n- Sessions: {}\n- Notional cost: ${:.2}\n- Time asleep on limits: {}\n\n",
            self.started_at.format("%Y-%m-%d %H:%M:%S %Z"),
            self.finished_at.format("%Y-%m-%d %H:%M:%S %Z"),
            self.base_branch,
            self.model,
            self.effort,
            self.total_sessions,
            self.total_cost_usd,
            util::format_duration(Duration::from_secs(self.slept_secs)),
        ));

        if let Some(line) = self.quota.as_ref().and_then(Quota::summary_line) {
            md.push_str(&format!("Quota after this run: {line}\n\n"));
        }

        md.push_str(&format!(
            "Queue: {}\n\n",
            self.queue
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" → ")
        ));
        if !self.queue_warnings.is_empty() {
            md.push_str("Queue warnings:\n\n");
            for warning in &self.queue_warnings {
                md.push_str(&format!("- {warning}\n"));
            }
            md.push('\n');
        }

        for status in [
            TaskStatus::ReadyForReview,
            TaskStatus::NeedsAttention,
            TaskStatus::Failed,
            TaskStatus::Skipped,
        ] {
            let tasks = self.tasks_with(status);
            if tasks.is_empty() {
                continue;
            }
            md.push_str(&format!("## {} ({})\n\n", status.heading(), tasks.len()));
            for task in tasks {
                md.push_str(&format!(
                    "### [#{}]({}) {}\n\n",
                    task.issue, task.issue_url, task.title
                ));
                if !task.detail.is_empty() {
                    md.push_str(&format!("{}\n\n", task.detail));
                }
                if let (Some(n), Some(url)) = (&task.pr, &task.pr_url) {
                    md.push_str(&format!("- Pull request: [#{n}]({url})\n"));
                }
                if let Some(branch) = &task.branch {
                    md.push_str(&format!(
                        "- Branch: `{branch}`{}\n",
                        if task.branch_was_linked {
                            " (already linked to the issue)"
                        } else {
                            ""
                        }
                    ));
                }
                if let Some(base) = &task.base_branch {
                    md.push_str(&format!("- Merges into: `{base}`\n"));
                }
                md.push_str(&format!("- CI fix attempts: {}\n", task.ci_fix_attempts));
                md.push_str(&format!(
                    "- Automated review: {}\n",
                    if task.reviewed { "done" } else { "not run" }
                ));
                for note in &task.notes {
                    md.push_str(&format!("- {note}\n"));
                }
                if !task.sessions.is_empty() {
                    md.push_str("\n<details><summary>Agent sessions</summary>\n\n");
                    for s in &task.sessions {
                        md.push_str(&format!(
                            "- `{}` — {}, {} turns, {}, ${:.4} — `{}`\n",
                            s.label,
                            s.outcome,
                            s.turns,
                            util::format_duration(Duration::from_secs(s.duration_secs)),
                            s.cost_usd,
                            s.transcript.display()
                        ));
                    }
                    md.push_str("\n</details>\n");
                }
                md.push('\n');
            }
        }

        md
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn report(statuses: &[TaskStatus]) -> RunReport {
        let at = Local.with_ymd_and_hms(2026, 9, 18, 2, 0, 0).unwrap();
        RunReport {
            repo: "o/n".into(),
            base_branch: "main".into(),
            model: "opus".into(),
            effort: "high".into(),
            started_at: at,
            finished_at: at,
            queue: vec![1, 2],
            queue_warnings: vec![],
            tasks: statuses
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let mut t = TaskReport::new(i as u64 + 1, "thing", "https://x/1");
                    t.status = *s;
                    t
                })
                .collect(),
            total_cost_usd: 1.0,
            total_sessions: 2,
            slept_secs: 0,
            quota: None,
            run_dir: PathBuf::from("/tmp/run"),
        }
    }

    #[test]
    fn exits_zero_only_when_everything_is_ready() {
        assert_eq!(
            report(&[TaskStatus::ReadyForReview, TaskStatus::ReadyForReview]).exit_code(),
            0
        );
        assert_eq!(
            report(&[TaskStatus::ReadyForReview, TaskStatus::NeedsAttention]).exit_code(),
            1
        );
        assert_eq!(report(&[TaskStatus::Failed]).exit_code(), 1);
        assert_eq!(report(&[TaskStatus::Skipped]).exit_code(), 1);
    }

    #[test]
    fn empty_run_exits_zero() {
        assert_eq!(report(&[]).exit_code(), 0);
    }

    #[test]
    fn markdown_groups_by_status() {
        let md = report(&[TaskStatus::ReadyForReview, TaskStatus::Failed]).markdown();
        assert!(md.contains("## Ready for review (1)"));
        assert!(md.contains("## Not completed (1)"));
        assert!(md.contains("#1 → #2") || md.contains("#1 → #2"));
    }
}
