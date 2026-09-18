//! Per-issue worktrees.
//!
//! Every issue gets a disposable checkout of its own — a fresh branch cut from
//! the base branch, or the branch GitHub already has linked to the issue. The
//! agent only ever sees this directory, so nothing it does can reach a checkout
//! the user is working in.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::branch::BranchPlan;
use crate::git;
use crate::repo::RepoContext;
use crate::ui;

/// A checked-out branch rain owns for the duration of one task.
#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// The branch this task's pull request targets.
    pub base_branch: String,
    /// The base branch's commit when the task started. Used afterwards to prove
    /// the agent never pushed to the base branch.
    pub base_sha: String,
    /// The branch's own tip when rain took it over. Equal to `base_sha` for a
    /// branch rain cut itself, and ahead of it for a linked branch that already
    /// carried work — which is what makes "did the agent commit anything?"
    /// answerable in both cases.
    pub start_sha: String,
    repo_root: PathBuf,
    keep: bool,
}

/// Fetch every branch into remote-tracking refs.
///
/// The refspec is explicit rather than inherited from the clone's config for
/// two reasons. A `git clone --bare` has no fetch refspec at all and keeps
/// branches in `refs/heads/*`, so `origin/main` would not resolve. And writing
/// only to `refs/remotes/*` means a fetch can never move a `refs/heads/*`
/// branch that one of our worktrees has checked out.
pub fn fetch(ctx: &RepoContext) -> Result<()> {
    ui::info(&format!("fetching {}", ctx.remote));
    let refspec = format!("+refs/heads/*:refs/remotes/{}/*", ctx.remote);
    git::run(&ctx.root, &["fetch", "--prune", &ctx.remote, &refspec])
        .with_context(|| format!("fetching from remote `{}`", ctx.remote))?;

    let base_ref = ctx.base_ref();
    git::run(&ctx.root, &["rev-parse", "--verify", "--quiet", &base_ref]).with_context(|| {
        format!(
            "`{}` does not exist on remote `{}` — check --base",
            ctx.base_branch, ctx.remote
        )
    })?;
    Ok(())
}

/// Refresh one branch's remote-tracking ref, for the base-branch safety check.
pub fn fetch_base(ctx: &RepoContext, base: &str) -> Result<()> {
    let refspec = format!(
        "+refs/heads/{base}:refs/remotes/{remote}/{base}",
        remote = ctx.remote
    );
    git::run(&ctx.root, &["fetch", &ctx.remote, &refspec])?;
    Ok(())
}

impl Worktree {
    /// Check out the branch `plan` settled on, reclaiming anything a previous
    /// crashed run left behind.
    pub fn create(ctx: &RepoContext, plan: &BranchPlan, root: &Path, keep: bool) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("creating worktree directory {}", root.display()))?;

        // Tidy up administrative records for directories that no longer exist
        // before asking whether a branch or path is free.
        let _ = git::try_run(&ctx.root, &["worktree", "prune"]);

        let branch = plan.branch.clone();
        let base_ref = ctx.remote_ref(&plan.base_branch);
        let base_sha = git::run(&ctx.root, &["rev-parse", &base_ref]).with_context(|| {
            format!("`{base_ref}` does not exist — has the remote been fetched?")
        })?;

        let path = root.join(branch.replace('/', "-"));
        if path.exists() {
            ui::warn(&format!(
                "reclaiming a leftover worktree at {}",
                path.display()
            ));
            remove_worktree(&ctx.root, &path)?;
        }

        if plan.linked {
            ui::info(&format!(
                "worktree {} on existing branch {branch} (merging into {base_ref})",
                path.display()
            ));
            check_out_existing(ctx, &branch, &path)
                .with_context(|| format!("checking out the existing branch `{branch}`"))?;
        } else {
            ui::info(&format!(
                "worktree {} on {branch} (from {base_ref} @ {})",
                path.display(),
                &base_sha[..base_sha.len().min(8)]
            ));
            // `--no-track` matters: without it the new branch's upstream is the
            // base branch, and a bare `git push` inside the worktree would
            // target the base branch directly.
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
            .with_context(|| format!("creating a worktree on `{branch}`"))?;
        }

        let start_sha = git::run(&path, &["rev-parse", "HEAD"])
            .with_context(|| format!("reading the tip of `{branch}`"))?;

        Ok(Self {
            path,
            branch,
            base_branch: plan.base_branch.clone(),
            base_sha,
            start_sha,
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

/// Put an existing branch — one GitHub linked to the issue — into a worktree.
///
/// Unlike a branch rain cut itself, this one may exist locally, remotely, or
/// both, and the two copies may disagree. Tracking the same-named remote branch
/// is safe here in a way tracking the base branch never is: a bare `git push`
/// goes back where the branch came from.
fn check_out_existing(ctx: &RepoContext, branch: &str, path: &Path) -> Result<()> {
    let remote_ref = ctx.remote_ref(branch);
    let has_local = git::probe(
        &ctx.root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    );
    let has_remote = git::probe(
        &ctx.root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{}/{branch}", ctx.remote),
        ],
    );

    if let Some(other) = checked_out_at(&ctx.root, branch) {
        bail!(
            "`{branch}` is already checked out at {} — rain will not take a branch out from under another worktree",
            other.display()
        );
    }

    if has_local {
        git::run(
            &ctx.root,
            &["worktree", "add", &path.to_string_lossy(), branch],
        )?;
        if has_remote {
            reconcile_with_remote(path, branch, &remote_ref)?;
        }
    } else {
        if !has_remote {
            bail!("`{branch}` exists neither locally nor on `{}`", ctx.remote);
        }
        git::run(
            &ctx.root,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                branch,
                &path.to_string_lossy(),
                &remote_ref,
            ],
        )?;
    }

    if has_remote {
        set_upstream(ctx, branch);
    }
    Ok(())
}

/// Point a branch at its own remote counterpart.
///
/// `git branch --set-upstream-to` refuses to do this in a bare clone: there is
/// no fetch refspec — rain supplies one on each fetch — so git will not accept
/// `origin/<branch>` as a remote-tracking branch. Writing the two configuration
/// keys says exactly the same thing without that check. It is a convenience
/// rather than a guarantee, since rain pushes with an explicit refspec either
/// way, so a failure here is not worth stopping for.
fn set_upstream(ctx: &RepoContext, branch: &str) {
    let _ = git::try_run(
        &ctx.root,
        &["config", &format!("branch.{branch}.remote"), &ctx.remote],
    );
    let _ = git::try_run(
        &ctx.root,
        &[
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        ],
    );
}

/// Bring a local branch up to its remote, or refuse to guess.
///
/// Fast-forwarding is the only safe reconciliation available: rain may not
/// force-push, so work left on a diverged local branch could never be pushed,
/// and discarding either side silently is worse than stopping.
fn reconcile_with_remote(path: &Path, branch: &str, remote_ref: &str) -> Result<()> {
    let range = format!("HEAD...{remote_ref}");
    let counts = git::run(path, &["rev-list", "--left-right", "--count", &range])?;
    let mut fields = counts.split_whitespace();
    let ahead: usize = fields.next().unwrap_or("0").parse().unwrap_or(0);
    let behind: usize = fields.next().unwrap_or("0").parse().unwrap_or(0);

    match (ahead, behind) {
        (_, 0) => Ok(()),
        (0, _) => {
            ui::info(&format!(
                "fast-forwarding `{branch}` to {remote_ref} ({behind} commit(s))"
            ));
            git::run(path, &["merge", "--ff-only", remote_ref])
                .with_context(|| format!("fast-forwarding `{branch}` to {remote_ref}"))?;
            Ok(())
        }
        _ => bail!(
            "the local and remote copies of `{branch}` have diverged ({ahead} commit(s) here, {behind} on {remote_ref}) — reconcile them before rain works this issue"
        ),
    }
}

/// Which worktree, if any, already has `branch` checked out.
fn checked_out_at(repo_root: &Path, branch: &str) -> Option<PathBuf> {
    let listed = git::run(repo_root, &["worktree", "list", "--porcelain"]).ok()?;
    let wanted = format!("refs/heads/{branch}");
    let mut current: Option<&str> = None;
    for line in listed.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(path.trim());
        } else if let Some(name) = line.strip_prefix("branch ")
            && name.trim() == wanted
        {
            return current.map(PathBuf::from);
        }
    }
    None
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
