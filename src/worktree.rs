//! Per-issue worktrees.
//!
//! Every issue gets a fresh, disposable checkout branched from the remote's
//! base branch. The agent only ever sees this directory, so nothing it does can
//! reach a checkout the user is working in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;
use crate::repo::RepoContext;
use crate::ui;

/// A checked-out branch rain owns for the duration of one task.
#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// The base branch commit this worktree was cut from. Used afterwards to
    /// prove the agent never pushed to the base branch.
    pub base_sha: String,
    repo_root: PathBuf,
    keep: bool,
}

/// Fetch the remote so worktrees branch from current refs.
pub fn fetch(ctx: &RepoContext) -> Result<()> {
    ui::info(&format!("fetching {}", ctx.remote));
    git::run(&ctx.root, &["fetch", "--prune", &ctx.remote])
        .with_context(|| format!("fetching from remote `{}`", ctx.remote))?;
    Ok(())
}

impl Worktree {
    /// Create a worktree for `issue`, reclaiming anything a previous crashed run
    /// left behind.
    pub fn create(ctx: &RepoContext, issue: u64, root: &Path, keep: bool) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating worktree directory {}", root.display()))?;

        // Tidy up administrative records for directories that no longer exist
        // before asking whether a branch or path is free.
        let _ = git::try_run(&ctx.root, &["worktree", "prune"]);

        let base_ref = ctx.base_ref();
        let base_sha = git::run(&ctx.root, &["rev-parse", &base_ref]).with_context(|| {
            format!("`{base_ref}` does not exist — has the remote been fetched?")
        })?;

        let branch = pick_branch_name(ctx, issue);
        let path = root.join(branch.replace('/', "-"));

        if path.exists() {
            ui::warn(&format!(
                "reclaiming a leftover worktree at {}",
                path.display()
            ));
            remove_worktree(&ctx.root, &path)?;
        }

        ui::info(&format!(
            "worktree {} on {branch} (from {base_ref} @ {})",
            path.display(),
            &base_sha[..base_sha.len().min(8)]
        ));

        // `--no-track` matters: without it the new branch's upstream is the base
        // branch, and a bare `git push` inside the worktree would target the
        // base branch directly.
        git::run(
            &ctx.root,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                &branch,
                &path.to_string_lossy(),
                &base_ref,
            ],
        )
        .with_context(|| format!("creating a worktree for issue #{issue}"))?;

        Ok(Self {
            path,
            branch,
            base_sha,
            repo_root: ctx.root.clone(),
            keep,
        })
    }

    /// Commits on this branch that are not on the base branch.
    pub fn commits_ahead(&self, base_ref: &str) -> Result<usize> {
        let range = format!("{base_ref}..HEAD");
        let count = git::run(&self.path, &["rev-list", "--count", &range])?;
        Ok(count.trim().parse().unwrap_or(0))
    }

    /// Every commit SHA introduced by this branch.
    pub fn new_commits(&self, base_ref: &str) -> Result<Vec<String>> {
        let range = format!("{base_ref}..HEAD");
        let out = git::run(&self.path, &["rev-list", &range])?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// True when the worktree has uncommitted changes the agent left behind.
    pub fn is_dirty(&self) -> Result<bool> {
        let out = git::run(&self.path, &["status", "--porcelain"])?;
        Ok(!out.trim().is_empty())
    }

    pub fn head_sha(&self) -> Result<String> {
        git::run(&self.path, &["rev-parse", "HEAD"])
    }

    /// Push this branch, and only this branch, to the remote.
    ///
    /// The refspec is fully qualified on both sides so no local configuration
    /// can redirect the push at another ref.
    pub fn push(&self, remote: &str) -> Result<()> {
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        git::run_streaming(&self.path, &["push", "--set-upstream", remote, &refspec])
            .with_context(|| format!("pushing `{}` to {remote}", self.branch))
    }

    /// Whether the remote branch matches local HEAD.
    pub fn is_pushed(&self, remote: &str) -> Result<bool> {
        let local = self.head_sha()?;
        let remote_ref = format!("refs/heads/{}", self.branch);
        let out = git::try_run(&self.path, &["ls-remote", remote, &remote_ref])?;
        if !out.success() {
            return Ok(false);
        }
        Ok(out
            .stdout
            .split_whitespace()
            .next()
            .is_some_and(|sha| sha == local))
    }

    /// Discard the worktree, keeping the branch (the PR needs it).
    pub fn cleanup(&self) {
        if self.keep {
            ui::info(&format!("keeping worktree {}", self.path.display()));
            return;
        }
        match remove_worktree(&self.repo_root, &self.path) {
            Ok(()) => ui::trace(&format!("removed worktree {}", self.path.display())),
            Err(e) => ui::warn(&format!(
                "could not remove worktree {}: {e}",
                self.path.display()
            )),
        }
    }
}

fn remove_worktree(repo_root: &Path, path: &Path) -> Result<()> {
    let out = git::try_run(
        repo_root,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )?;
    if out.success() {
        return Ok(());
    }
    // The administrative record and the directory can disagree after a crash;
    // clear both rather than leaving the path permanently unusable.
    if path.exists() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    let pruned = git::try_run(repo_root, &["worktree", "prune"])?;
    if !pruned.success() {
        bail!("{}", out.stderr);
    }
    Ok(())
}

/// `rain/issue-N`, suffixed if that name is already taken locally or remotely.
fn pick_branch_name(ctx: &RepoContext, issue: u64) -> String {
    let base = format!("rain/issue-{issue}");
    if !branch_exists(ctx, &base) {
        return base;
    }
    for suffix in 2..=50u32 {
        let candidate = format!("{base}-{suffix}");
        if !branch_exists(ctx, &candidate) {
            ui::warn(&format!("`{base}` already exists; using `{candidate}`"));
            return candidate;
        }
    }
    // Fifty collisions means something is badly wrong, but a timestamped name is
    // still better than failing the run outright.
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S");
    format!("{base}-{stamp}")
}

fn branch_exists(ctx: &RepoContext, branch: &str) -> bool {
    let local = format!("refs/heads/{branch}");
    if git::probe(&ctx.root, &["show-ref", "--verify", "--quiet", &local]) {
        return true;
    }
    let remote = format!("refs/remotes/{}/{branch}", ctx.remote);
    git::probe(&ctx.root, &["show-ref", "--verify", "--quiet", &remote])
}
