//! Exercises the git layer against a real bare clone.
//!
//! The bare-clone path is the one rain is actually run from, and it differs
//! from an ordinary clone in ways that unit tests cannot catch: a bare clone
//! has no `refs/remotes/*` and no fetch refspec until rain supplies one.

use std::path::{Path, PathBuf};
use std::process::Command;

use rain::branch::{self, BaseSource, BranchPlan};
use rain::repo::{self, RepoContext};
use rain::worktree::{self, Worktree};

/// A throwaway origin repository, a bare clone of it, and a working clone the
/// tests push from so the origin's branches get there the way a person's would.
struct Scratch {
    root: PathBuf,
    origin: PathBuf,
    bare: PathBuf,
    seed: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

impl Scratch {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "rain-it-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();

        // A bare origin, seeded through a short-lived working clone, so pushes
        // from rain's worktrees behave exactly as they do against a real forge.
        let origin = root.join("origin.git");
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&origin)
            .output()
            .unwrap();

        let seed = root.join("seed");
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .arg(&seed)
            .output()
            .unwrap();
        git(&seed, &["config", "user.email", "rain@example.invalid"]);
        git(&seed, &["config", "user.name", "rain test"]);
        std::fs::write(seed.join("README.md"), "scratch\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-m", "chore: seed"]);
        git(
            &seed,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git(&seed, &["push", "-u", "origin", "main"]);

        let bare = root.join("clone.git");
        let out = Command::new("git")
            .arg("clone")
            .arg("--bare")
            .arg(&origin)
            .arg(&bare)
            .output()
            .unwrap();
        assert!(out.status.success(), "bare clone failed");
        // Commits made inside worktrees need an identity.
        git(&bare, &["config", "user.email", "rain@example.invalid"]);
        git(&bare, &["config", "user.name", "rain test"]);

        Self {
            root,
            origin,
            bare,
            seed,
        }
    }

    /// The context rain would build for this clone.
    fn context(&self) -> RepoContext {
        let mut ctx = repo::discover(&self.bare, None, Some("scratch/repo"))
            .expect("discovering the bare clone");
        ctx.base_branch = repo::resolve_base_branch(&ctx, Some("main"), None);
        ctx
    }

    fn worktree_root(&self) -> PathBuf {
        self.bare.join("rain/worktrees")
    }

    /// Push a branch to the origin, cut from `from`, with one commit per file.
    fn push_branch(&self, branch: &str, from: &str, files: &[&str]) {
        git(&self.seed, &["fetch", "origin"]);
        git(
            &self.seed,
            &["checkout", "-B", branch, &format!("origin/{from}")],
        );
        for file in files {
            std::fs::write(self.seed.join(file), format!("{file}\n")).unwrap();
            git(&self.seed, &["add", "-A"]);
            git(&self.seed, &["commit", "-m", &format!("feat: add {file}")]);
        }
        git(&self.seed, &["push", "-u", "origin", branch]);
    }

    fn origin_sha(&self, branch: &str) -> String {
        git(
            &self.origin,
            &["rev-parse", &format!("refs/heads/{branch}")],
        )
    }
}

/// What `branch::plan` produces for an issue with no linked branch.
fn fresh_plan(ctx: &RepoContext, issue: u64) -> BranchPlan {
    BranchPlan {
        branch: branch::pick_branch_name(ctx, issue),
        base_branch: ctx.base_branch.clone(),
        base_source: BaseSource::Default,
        linked: false,
    }
}

/// What `branch::plan` produces for an issue GitHub has linked to `branch`.
fn linked_plan(branch: &str, base: &str) -> BranchPlan {
    BranchPlan {
        branch: branch.to_string(),
        base_branch: base.to_string(),
        base_source: BaseSource::Inferred,
        linked: true,
    }
}

fn commit_in(worktree: &Worktree, file: &str, contents: &str, message: &str) {
    std::fs::write(worktree.path.join(file), contents).unwrap();
    git(&worktree.path, &["add", "-A"]);
    git(&worktree.path, &["commit", "-m", message]);
}

#[test]
fn discovers_a_bare_clone() {
    let scratch = Scratch::new("discover");
    let ctx = scratch.context();

    assert!(ctx.is_bare, "the clone should be recognised as bare");
    assert_eq!(ctx.remote, "origin");
    assert_eq!(ctx.slug.to_string(), "scratch/repo");
    assert_eq!(ctx.base_branch, "main");
    assert_eq!(ctx.root, scratch.bare.canonicalize().unwrap());
}

#[test]
fn fetch_creates_remote_tracking_refs_a_bare_clone_lacks() {
    let scratch = Scratch::new("fetch");
    let ctx = scratch.context();

    // The defining quirk of a bare clone: no remote-tracking refs at all.
    let before = git(&scratch.bare, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        !before.contains("refs/remotes/origin/main"),
        "a fresh bare clone should have no remote-tracking refs, got:\n{before}"
    );

    worktree::fetch(&ctx).expect("fetch should create the refs it needs");

    let after = git(&scratch.bare, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        after.contains("refs/remotes/origin/main"),
        "fetch should have created origin/main, got:\n{after}"
    );
    assert_eq!(ctx.base_ref(), "origin/main");
    git(&scratch.bare, &["rev-parse", "--verify", "origin/main"]);
}

#[test]
fn fetch_rejects_a_base_branch_that_does_not_exist() {
    let scratch = Scratch::new("badbase");
    let mut ctx = scratch.context();
    ctx.base_branch = "nonexistent".to_string();

    let err = worktree::fetch(&ctx).expect_err("a missing base branch must fail loudly");
    let message = format!("{err:#}");
    assert!(
        message.contains("nonexistent") && message.contains("--base"),
        "the error should name the branch and the flag, got: {message}"
    );
}

#[test]
fn creates_a_worktree_that_cannot_push_to_the_base_branch_by_default() {
    let scratch = Scratch::new("notrack");
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    let wt = Worktree::create(&ctx, &fresh_plan(&ctx, 1), &scratch.worktree_root(), false).unwrap();

    assert_eq!(wt.branch, "rain/issue-1");
    assert!(wt.path.is_dir());

    // `--no-track` is what stops a bare `git push` in the worktree landing on
    // the base branch.
    let upstream = Command::new("git")
        .arg("-C")
        .arg(&wt.path)
        .args(["config", "--get", "branch.rain/issue-1.merge"])
        .output()
        .unwrap();
    assert!(
        !upstream.status.success(),
        "the branch must not track the base branch, but it tracks {}",
        String::from_utf8_lossy(&upstream.stdout)
    );

    wt.cleanup();
    assert!(!wt.path.exists());
}

#[test]
fn tracks_commits_and_dirtiness() {
    let scratch = Scratch::new("commits");
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();
    let wt = Worktree::create(&ctx, &fresh_plan(&ctx, 2), &scratch.worktree_root(), false).unwrap();

    assert_eq!(wt.commits_ahead(&wt.base_sha).unwrap(), 0);
    assert!(!wt.is_dirty().unwrap());

    commit_in(&wt, "one.txt", "one\n", "feat: add one");
    assert_eq!(wt.commits_ahead(&wt.base_sha).unwrap(), 1);

    commit_in(&wt, "two.txt", "two\n", "feat: add two");
    assert_eq!(wt.commits_ahead(&wt.base_sha).unwrap(), 2);
    assert_eq!(wt.new_commits(&wt.base_sha).unwrap().len(), 2);

    std::fs::write(wt.path.join("scratch.txt"), "uncommitted\n").unwrap();
    assert!(wt.is_dirty().unwrap(), "an untracked file counts as dirty");

    wt.cleanup();
}

#[test]
fn pushes_only_its_own_branch() {
    let scratch = Scratch::new("push");
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();
    let wt = Worktree::create(&ctx, &fresh_plan(&ctx, 3), &scratch.worktree_root(), false).unwrap();

    let base_before = git(&scratch.origin, &["rev-parse", "refs/heads/main"]);

    commit_in(&wt, "feature.txt", "work\n", "feat: do the thing");
    assert!(!wt.is_pushed("origin").unwrap());

    wt.push("origin").expect("pushing the task branch");
    assert!(wt.is_pushed("origin").unwrap());

    let pushed = git(&scratch.origin, &["rev-parse", "refs/heads/rain/issue-3"]);
    assert_eq!(pushed, wt.head_sha().unwrap());

    let base_after = git(&scratch.origin, &["rev-parse", "refs/heads/main"]);
    assert_eq!(base_before, base_after, "the base branch must not move");

    wt.cleanup();
}

#[test]
fn suffixes_the_branch_when_the_name_is_taken() {
    let scratch = Scratch::new("collision");
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    let first =
        Worktree::create(&ctx, &fresh_plan(&ctx, 4), &scratch.worktree_root(), false).unwrap();
    assert_eq!(first.branch, "rain/issue-4");
    // Keep the branch, drop the worktree — the state left behind by a finished
    // task whose PR is still open.
    first.cleanup();

    let second =
        Worktree::create(&ctx, &fresh_plan(&ctx, 4), &scratch.worktree_root(), false).unwrap();
    assert_eq!(second.branch, "rain/issue-4-2");
    assert_ne!(first.path, second.path);
    second.cleanup();
}

#[test]
fn reclaims_a_worktree_left_by_a_crashed_run() {
    let scratch = Scratch::new("reclaim");
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    let abandoned =
        Worktree::create(&ctx, &fresh_plan(&ctx, 5), &scratch.worktree_root(), false).unwrap();
    let path = abandoned.path.clone();
    commit_in(&abandoned, "half.txt", "half done\n", "wip: half");
    // Simulate a crash: the directory and the administrative record survive,
    // but nothing cleans them up.
    std::mem::forget(abandoned);

    assert!(path.is_dir());

    // Branch `rain/issue-5` is taken, so the next attempt takes the next name
    // and must not trip over the orphaned directory.
    let fresh =
        Worktree::create(&ctx, &fresh_plan(&ctx, 5), &scratch.worktree_root(), false).unwrap();
    assert_eq!(fresh.branch, "rain/issue-5-2");
    assert_eq!(fresh.commits_ahead(&fresh.base_sha).unwrap(), 0);
    fresh.cleanup();
}

#[test]
fn reuses_a_path_left_behind_without_its_branch() {
    let scratch = Scratch::new("stalepath");
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    // A directory sitting where the worktree belongs, with no git record of it:
    // what a `kill -9` between `mkdir` and `worktree add` leaves.
    let stale = scratch.worktree_root().join("rain-issue-6");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("junk.txt"), "debris\n").unwrap();

    let wt = Worktree::create(&ctx, &fresh_plan(&ctx, 6), &scratch.worktree_root(), false).unwrap();
    assert_eq!(wt.branch, "rain/issue-6");
    assert!(
        !wt.path.join("junk.txt").exists(),
        "the stale directory should have been cleared"
    );
    wt.cleanup();
}

// ── existing branches ────────────────────────────────────────────────────────

#[test]
fn works_on_an_existing_branch_without_creating_one() {
    let scratch = Scratch::new("existing");
    scratch.push_branch("v3-changes", "main", &["v3.txt"]);
    scratch.push_branch("feature/config", "v3-changes", &["config.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    let wt = Worktree::create(
        &ctx,
        &linked_plan("feature/config", "v3-changes"),
        &scratch.worktree_root(),
        false,
    )
    .expect("checking out the branch already linked to the issue");

    assert_eq!(wt.branch, "feature/config");
    assert_eq!(wt.base_branch, "v3-changes");
    assert_eq!(wt.start_sha, scratch.origin_sha("feature/config"));
    assert_eq!(wt.base_sha, scratch.origin_sha("v3-changes"));
    // The commit the branch arrived with is not the agent's work…
    assert_eq!(wt.commits_ahead(&wt.start_sha).unwrap(), 0);
    // …but it is still part of what the pull request will contain.
    assert_eq!(wt.commits_ahead(&wt.base_sha).unwrap(), 1);
    assert!(wt.path.join("config.txt").exists());

    commit_in(&wt, "more.txt", "more\n", "feat: add more");
    assert_eq!(wt.commits_ahead(&wt.start_sha).unwrap(), 1);
    assert_eq!(wt.commits_ahead(&wt.base_sha).unwrap(), 2);

    // Tracking the same-named remote branch is safe: a bare push goes back
    // where the branch came from, never to the base branch.
    let upstream = git(
        &wt.path,
        &["config", "--get", "branch.feature/config.merge"],
    );
    assert_eq!(upstream, "refs/heads/feature/config");

    let base_before = scratch.origin_sha("v3-changes");
    wt.push("origin").expect("pushing the existing branch");
    assert_eq!(scratch.origin_sha("feature/config"), wt.head_sha().unwrap());
    assert_eq!(
        scratch.origin_sha("v3-changes"),
        base_before,
        "the base branch must not move"
    );
    wt.cleanup();
}

#[test]
fn fast_forwards_a_stale_local_copy_of_an_existing_branch() {
    let scratch = Scratch::new("stalelocal");
    scratch.push_branch("feature/late", "main", &["one.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    // A local branch left behind by an earlier run, pinned to the older tip…
    let old = git(&scratch.bare, &["rev-parse", "origin/feature/late"]);
    git(&scratch.bare, &["branch", "feature/late", &old]);
    // …while the remote moved on.
    scratch.push_branch("feature/late", "feature/late", &["two.txt"]);
    worktree::fetch(&ctx).unwrap();

    let wt = Worktree::create(
        &ctx,
        &linked_plan("feature/late", "main"),
        &scratch.worktree_root(),
        false,
    )
    .expect("a local branch that is merely behind should be fast-forwarded");

    assert_eq!(wt.start_sha, scratch.origin_sha("feature/late"));
    assert!(wt.path.join("two.txt").exists());
    wt.cleanup();
}

#[test]
fn refuses_an_existing_branch_whose_copies_have_diverged() {
    let scratch = Scratch::new("diverged");
    scratch.push_branch("feature/split", "main", &["one.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();
    git(
        &scratch.bare,
        &["branch", "feature/split", "origin/feature/split"],
    );

    // The remote branch is rewritten under us, leaving the local copy holding a
    // commit that is no longer on the remote.
    git(&scratch.seed, &["checkout", "feature/split"]);
    git(&scratch.seed, &["reset", "--hard", "origin/main"]);
    std::fs::write(scratch.seed.join("other.txt"), "other\n").unwrap();
    git(&scratch.seed, &["add", "-A"]);
    git(&scratch.seed, &["commit", "-m", "feat: other"]);
    git(
        &scratch.seed,
        &["push", "--force", "origin", "feature/split"],
    );
    worktree::fetch(&ctx).unwrap();

    let err = Worktree::create(
        &ctx,
        &linked_plan("feature/split", "main"),
        &scratch.worktree_root(),
        false,
    )
    .expect_err("rain cannot force-push, so it must not pretend it can reconcile this");
    let message = format!("{err:#}");
    assert!(
        message.contains("diverged") && message.contains("feature/split"),
        "the error should say what diverged, got: {message}"
    );
}

#[test]
fn refuses_an_existing_branch_checked_out_somewhere_else() {
    let scratch = Scratch::new("borrowed");
    scratch.push_branch("feature/busy", "main", &["one.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    let elsewhere = scratch.root.join("someone-elses-worktree");
    git(
        &scratch.bare,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            "feature/busy",
            &elsewhere.to_string_lossy(),
            "origin/feature/busy",
        ],
    );

    let err = Worktree::create(
        &ctx,
        &linked_plan("feature/busy", "main"),
        &scratch.worktree_root(),
        false,
    )
    .expect_err("a branch in use elsewhere must not be taken over");
    let message = format!("{err:#}");
    assert!(
        message.contains("already checked out"),
        "the error should say the branch is in use, got: {message}"
    );
}

// ── where a branch came from ─────────────────────────────────────────────────

#[test]
fn infers_the_branch_an_existing_branch_was_cut_from() {
    let scratch = Scratch::new("parent");
    scratch.push_branch("v3-changes", "main", &["v3a.txt", "v3b.txt"]);
    scratch.push_branch("feature/config", "v3-changes", &["config.txt"]);
    // A decoy: cut from main, entirely unrelated to the branch we are asking
    // about, and more recently updated.
    scratch.push_branch("other/work", "main", &["other.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    assert_eq!(
        branch::infer_parent(&ctx, "feature/config").as_deref(),
        Some("v3-changes"),
        "a branch cut from v3-changes belongs back in v3-changes"
    );
}

#[test]
fn a_linked_branch_with_no_commits_still_finds_its_parent() {
    let scratch = Scratch::new("emptylinked");
    scratch.push_branch("v3-changes", "main", &["v3.txt"]);
    // What GitHub's `create a branch` link leaves: a branch at the tip of the
    // one it was cut from, with nothing on it yet.
    scratch.push_branch("issue-9-fix", "v3-changes", &[]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    assert_eq!(
        branch::infer_parent(&ctx, "issue-9-fix").as_deref(),
        Some("v3-changes")
    );
}

#[test]
fn an_ordinary_branch_infers_the_default_branch() {
    let scratch = Scratch::new("parentmain");
    scratch.push_branch("v3-changes", "main", &["v3.txt"]);
    scratch.push_branch("feature/plain", "main", &["plain.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    assert_eq!(
        branch::infer_parent(&ctx, "feature/plain").as_deref(),
        Some("main")
    );
}

#[test]
fn a_branch_that_others_were_cut_from_is_not_mistaken_for_a_child() {
    let scratch = Scratch::new("downstream");
    scratch.push_branch("v3-changes", "main", &["v3.txt"]);
    scratch.push_branch("feature/config", "v3-changes", &["config.txt"]);
    // Someone branched off the branch we are asking about and carried on.
    scratch.push_branch("feature/config-extra", "feature/config", &["extra.txt"]);
    let ctx = scratch.context();
    worktree::fetch(&ctx).unwrap();

    assert_eq!(
        branch::infer_parent(&ctx, "feature/config").as_deref(),
        Some("v3-changes"),
        "a branch downstream of ours cannot be the one we merge into"
    );
}
