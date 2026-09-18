//! Command-line surface.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;

use crate::agent::Effort;
use crate::util;

/// rain — Rust Automated Issue eNgine.
///
/// Turns GitHub issues into reviewed pull requests using Claude Code. Run it
/// from a bare clone of the repository you want worked on.
#[derive(Debug, Parser)]
#[command(name = "rain", version, about, long_about = None)]
pub struct Cli {
    /// Issue numbers to work on, in the order you would like them attempted.
    /// Declared dependencies between them override this order.
    #[arg(value_name = "ISSUE", required = true)]
    pub issues: Vec<u64>,

    /// Repository to work on, as owner/name. Defaults to the clone's remote.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Git remote to use. Defaults to `origin`.
    #[arg(long, value_name = "NAME")]
    pub remote: Option<String>,

    /// Branch pull requests target. Defaults to the repository's default branch.
    #[arg(long, value_name = "BRANCH")]
    pub base: Option<String>,

    /// Model for every agent session.
    #[arg(long, default_value = "opus", value_name = "MODEL")]
    pub model: String,

    /// Reasoning effort for every agent session.
    #[arg(long, default_value = "high", value_enum)]
    pub effort: Effort,

    /// How many times to send a CI failure back to the agent before giving up.
    #[arg(long, default_value_t = 3, value_name = "N")]
    pub max_ci_retries: u32,

    /// How long to wait for CI on one pull request.
    #[arg(long, default_value = "25m", value_name = "DURATION")]
    pub ci_timeout: String,

    /// How long to wait for any check to report before deciding the repository
    /// has no CI.
    #[arg(long, default_value = "3m", value_name = "DURATION")]
    pub ci_grace: String,

    /// Wall-clock budget for a single agent session.
    #[arg(long, default_value = "60m", value_name = "DURATION")]
    pub agent_timeout: String,

    /// Total time rain may spend asleep waiting for usage limits to reset.
    #[arg(long, default_value = "8h", value_name = "DURATION")]
    pub max_wait: String,

    /// Stop instead of sleeping when a usage limit is hit.
    #[arg(long)]
    pub no_wait_on_limit: bool,

    /// Where to create per-issue worktrees.
    /// Defaults to `<git-dir>/rain/worktrees`.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Where to write transcripts and the run report.
    /// Defaults to `<git-dir>/rain/runs/<timestamp>`.
    #[arg(long, value_name = "DIR")]
    pub run_dir: Option<PathBuf>,

    /// Open pull requests ready for review rather than as drafts.
    #[arg(long)]
    pub ready: bool,

    /// Skip the automated code-review pass.
    #[arg(long)]
    pub no_review: bool,

    /// Ignore declared relationships and work the issues in the order given.
    #[arg(long)]
    pub ignore_relationships: bool,

    /// Keep worktrees after each issue, for debugging.
    #[arg(long)]
    pub keep_worktrees: bool,

    /// Resolve the queue and print the plan without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Log every git, gh and agent action.
    #[arg(short, long)]
    pub verbose: bool,
}

/// The validated, parsed form of the command line.
pub struct Options {
    pub issues: Vec<u64>,
    pub repo: Option<String>,
    pub remote: Option<String>,
    pub base: Option<String>,
    pub model: String,
    pub effort: Effort,
    pub max_ci_retries: u32,
    pub ci_timeout: Duration,
    pub ci_grace: Duration,
    pub agent_timeout: Duration,
    pub max_wait: Duration,
    pub wait_on_limit: bool,
    pub work_dir: Option<PathBuf>,
    pub run_dir: Option<PathBuf>,
    pub draft: bool,
    pub review: bool,
    pub ignore_relationships: bool,
    pub keep_worktrees: bool,
    pub dry_run: bool,
    pub verbose: bool,
}

impl Cli {
    /// Validate the raw arguments. Everything that can be wrong about a run is
    /// caught here, before any repository or network access.
    pub fn validate(self) -> Result<Options> {
        let issues = dedupe(&self.issues);
        if issues.is_empty() {
            bail!("no issue numbers given");
        }
        if let Some(bad) = issues.iter().find(|n| **n == 0) {
            bail!("`{bad}` is not a valid issue number");
        }

        let model = validate_model(&self.model)?;

        if let Some(spec) = &self.repo {
            crate::repo::RepoSlug::parse(spec)?;
        }

        let ci_timeout = util::parse_duration(&self.ci_timeout)?;
        let ci_grace = util::parse_duration(&self.ci_grace)?;
        let agent_timeout = util::parse_duration(&self.agent_timeout)?;
        let max_wait = util::parse_duration(&self.max_wait)?;

        if ci_grace > ci_timeout {
            bail!("--ci-grace must not exceed --ci-timeout");
        }

        Ok(Options {
            issues,
            repo: self.repo,
            remote: self.remote,
            base: self.base,
            model,
            effort: self.effort,
            max_ci_retries: self.max_ci_retries,
            ci_timeout,
            ci_grace,
            agent_timeout,
            max_wait,
            wait_on_limit: !self.no_wait_on_limit,
            work_dir: self.work_dir,
            run_dir: self.run_dir,
            draft: !self.ready,
            review: !self.no_review,
            ignore_relationships: self.ignore_relationships,
            keep_worktrees: self.keep_worktrees,
            dry_run: self.dry_run,
            verbose: self.verbose,
        })
    }
}

/// rain does not run on Fable, whichever way the name is spelled.
fn validate_model(model: &str) -> Result<String> {
    let model = model.trim();
    if model.is_empty() {
        bail!("--model cannot be empty");
    }
    if model.to_lowercase().contains("fable") {
        bail!("rain does not run on Fable models — use opus (the default), sonnet or haiku");
    }
    Ok(model.to_string())
}

/// Preserve the order the user gave while dropping repeats.
fn dedupe(issues: &[u64]) -> Vec<u64> {
    let mut seen = std::collections::HashSet::new();
    issues.iter().copied().filter(|n| seen.insert(*n)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Options> {
        Cli::try_parse_from(std::iter::once("rain").chain(args.iter().copied()))
            .map_err(anyhow::Error::from)?
            .validate()
    }

    #[test]
    fn defaults_to_opus_at_high_effort() {
        let opts = parse(&["1"]).unwrap();
        assert_eq!(opts.model, "opus");
        assert_eq!(opts.effort, Effort::High);
    }

    #[test]
    fn rejects_fable() {
        assert!(validate_model("fable").is_err());
        assert!(validate_model("Fable").is_err());
        assert!(validate_model("claude-fable-5-1").is_err());
        assert!(parse(&["--model", "fable", "1"]).is_err());
    }

    #[test]
    fn allows_the_other_models() {
        assert_eq!(validate_model("opus").unwrap(), "opus");
        assert_eq!(validate_model("sonnet").unwrap(), "sonnet");
        assert_eq!(validate_model(" haiku ").unwrap(), "haiku");
        assert_eq!(validate_model("claude-opus-5").unwrap(), "claude-opus-5");
    }

    #[test]
    fn defaults_to_draft_prs_with_review() {
        let opts = parse(&["1"]).unwrap();
        assert!(opts.draft);
        assert!(opts.review);
        assert!(opts.wait_on_limit);
        assert_eq!(opts.max_ci_retries, 3);
    }

    #[test]
    fn flags_flip_the_defaults() {
        let opts = parse(&["--ready", "--no-review", "--no-wait-on-limit", "1"]).unwrap();
        assert!(!opts.draft);
        assert!(!opts.review);
        assert!(!opts.wait_on_limit);
    }

    #[test]
    fn keeps_issue_order_and_drops_repeats() {
        let opts = parse(&["7", "3", "7", "5"]).unwrap();
        assert_eq!(opts.issues, vec![7, 3, 5]);
    }

    #[test]
    fn rejects_issue_zero() {
        assert!(parse(&["0"]).is_err());
    }

    #[test]
    fn rejects_a_grace_longer_than_the_timeout() {
        assert!(parse(&["--ci-timeout", "1m", "--ci-grace", "5m", "1"]).is_err());
    }

    #[test]
    fn rejects_a_malformed_repo_slug() {
        assert!(parse(&["--repo", "not-a-slug", "1"]).is_err());
    }

    #[test]
    fn parses_durations() {
        let opts = parse(&["--ci-timeout", "40m", "--agent-timeout", "2h", "1"]).unwrap();
        assert_eq!(opts.ci_timeout, Duration::from_secs(2400));
        assert_eq!(opts.agent_timeout, Duration::from_secs(7200));
    }

    #[test]
    fn requires_at_least_one_issue() {
        assert!(parse(&[]).is_err());
    }
}
