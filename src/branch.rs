//! Which branch a task works on, and which branch its pull request targets.
//!
//! Two decisions, both made before the worktree exists:
//!
//! 1. **The branch.** If GitHub already has a branch linked to the issue — the
//!    Development sidebar, or the `create a branch` link on the issue — that is
//!    the branch a human expects the work to appear on, so rain uses it instead
//!    of cutting `rain/issue-N` alongside it.
//! 2. **The base.** A linked branch was cut from somewhere, and not necessarily
//!    from the default branch. A branch cut from `v3-changes` belongs back in
//!    `v3-changes`; opening its pull request against `main` would put the whole
//!    of `v3-changes` in the diff.
//!
//! Git does not record where a branch came from, so the base is worked out in
//! descending order of confidence: an explicit `--base`, then the base of a
//! pull request already open for the branch (authoritative — someone chose it),
//! then the branch whose history the branch most plausibly grew out of, then
//! the repository's default.

use serde::Serialize;

use crate::git;
use crate::github::Forge;
use crate::repo::RepoContext;
use crate::ui;

/// How many remote branches to consider as the parent of a branch.
///
/// Every candidate costs a `git rev-list`, and a branch is cut from something
/// that was current at the time, so the most recently updated branches are
/// where the answer is. The base branch is always considered regardless.
const MAX_CANDIDATES: usize = 100;

/// How the base branch for one task was decided.
///
/// Recorded because "why is this pull request targeting `v3-changes`?" is the
/// first thing a human asks when it is not targeting `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseSource {
    /// The run's base branch: `--base`, or the repository default.
    Default,
    /// The base of the pull request already open for this branch.
    OpenPullRequest,
    /// Inferred from where the branch's history diverges.
    Inferred,
}

impl BaseSource {
    pub fn label(self) -> &'static str {
        match self {
            BaseSource::Default => "the run's base branch",
            BaseSource::OpenPullRequest => "the base of the open pull request",
            BaseSource::Inferred => "the branch it was cut from",
        }
    }
}

/// The branch one task works on and the branch its pull request targets.
#[derive(Debug, Clone)]
pub struct BranchPlan {
    pub branch: String,
    pub base_branch: String,
    pub base_source: BaseSource,
    /// True when `branch` already existed and is linked to the issue on GitHub.
    /// A linked branch may already carry commits, and is not rain's to rename.
    pub linked: bool,
}

/// Decide the branch and base for `issue`.
///
/// Never fails: every unanswerable question falls back to rain's own branch on
/// the run's base branch, which is what rain did before any of this existed.
pub fn plan(ctx: &RepoContext, forge: &Forge, issue: u64, use_linked: bool) -> BranchPlan {
    let fresh = |ctx: &RepoContext| BranchPlan {
        branch: pick_branch_name(ctx, issue),
        base_branch: ctx.base_branch.clone(),
        base_source: BaseSource::Default,
        linked: false,
    };

    if !use_linked {
        return fresh(ctx);
    }

    let Some(branch) = linked_branch(ctx, forge, issue) else {
        return fresh(ctx);
    };

    let (base_branch, base_source) = resolve_base(ctx, forge, &branch);
    // Everything downstream measures against the base, so a base rain cannot
    // resolve locally is worse than no inference at all.
    let (base_branch, base_source) =
        if base_branch == ctx.base_branch || exists_on_remote(ctx, &base_branch) {
            (base_branch, base_source)
        } else {
            ui::warn(&format!(
                "`{base_branch}` does not exist on `{}`; targeting `{}` instead",
                ctx.remote, ctx.base_branch
            ));
            (ctx.base_branch.clone(), BaseSource::Default)
        };

    ui::info(&format!(
        "issue #{issue} already has branch `{branch}`; using it, targeting `{base_branch}` ({})",
        base_source.label()
    ));
    BranchPlan {
        branch,
        base_branch,
        base_source,
        linked: true,
    }
}

/// The linked branch rain can actually work on, if there is one.
fn linked_branch(ctx: &RepoContext, forge: &Forge, issue: u64) -> Option<String> {
    let linked = forge.linked_branches(issue)?;
    if linked.is_empty() {
        return None;
    }

    let repo = ctx.slug.to_string();
    let mut usable = Vec::new();
    for branch in linked {
        if !branch.repo.is_empty() && !branch.repo.eq_ignore_ascii_case(&repo) {
            ui::warn(&format!(
                "issue #{issue} is linked to `{}` in {}, which is not the repository rain is working on; ignoring it",
                branch.name, branch.repo
            ));
            continue;
        }
        if !exists_on_remote(ctx, &branch.name) {
            ui::warn(&format!(
                "issue #{issue} is linked to `{}`, which does not exist on `{}`; ignoring it",
                branch.name, ctx.remote
            ));
            continue;
        }
        usable.push(branch.name);
    }

    let mut usable = usable.into_iter();
    let first = usable.next()?;
    let rest: Vec<String> = usable.collect();
    if !rest.is_empty() {
        ui::warn(&format!(
            "issue #{issue} has more than one linked branch ({}); working on `{first}`",
            rest.join(", ")
        ));
    }
    Some(first)
}

/// Refresh one branch's remote-tracking ref, in case it was created after the
/// run's initial fetch, then report whether it exists.
fn exists_on_remote(ctx: &RepoContext, branch: &str) -> bool {
    let refspec = format!(
        "+refs/heads/{branch}:refs/remotes/{remote}/{branch}",
        remote = ctx.remote
    );
    let _ = git::try_run(&ctx.root, &["fetch", &ctx.remote, &refspec]);
    let full = format!("refs/remotes/{}/{branch}", ctx.remote);
    git::probe(&ctx.root, &["show-ref", "--verify", "--quiet", &full])
}

/// Work out which branch an existing branch should merge back into.
fn resolve_base(ctx: &RepoContext, forge: &Forge, branch: &str) -> (String, BaseSource) {
    let default = || (ctx.base_branch.clone(), BaseSource::Default);

    // An explicit --base is an instruction, not a guess to be improved on.
    if ctx.base_explicit {
        return default();
    }

    match forge.find_pr_for_branch(branch) {
        Ok(Some(pr)) if !pr.base_ref.is_empty() && pr.base_ref != branch => {
            return (pr.base_ref, BaseSource::OpenPullRequest);
        }
        Ok(_) => {}
        Err(e) => ui::trace(&format!(
            "could not check for an open pull request on `{branch}`: {e:#}"
        )),
    }

    match infer_parent(ctx, branch) {
        // Landing on the run's base branch is the ordinary case, and saying
        // "inferred" about it would invite doubt where there is none.
        Some(parent) if parent == ctx.base_branch => default(),
        Some(parent) => (parent, BaseSource::Inferred),
        None => default(),
    }
}

/// One branch `branch` might have been cut from, and how their histories
/// differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    /// Commits on `branch` that the candidate does not have.
    pub ahead: usize,
    /// Commits on the candidate that `branch` does not have.
    pub behind: usize,
}

/// The branch `branch` most plausibly grew out of, by comparing histories.
pub fn infer_parent(ctx: &RepoContext, branch: &str) -> Option<String> {
    let head = ctx.remote_ref(branch);
    let candidates: Vec<Candidate> = candidate_branches(ctx, branch)
        .into_iter()
        .filter_map(|name| {
            let (ahead, behind) = git::divergence(&ctx.root, &head, &ctx.remote_ref(&name)).ok()?;
            Some(Candidate {
                name,
                ahead,
                behind,
            })
        })
        .collect();

    let winner = rank(&candidates, &ctx.base_branch)?;

    // Two branches with no common ancestor are not parent and child, however
    // the counts come out.
    if !git::probe(
        &ctx.root,
        &["merge-base", &head, &ctx.remote_ref(&winner.name)],
    ) {
        ui::trace(&format!(
            "`{branch}` shares no history with `{}`; not treating it as the parent",
            winner.name
        ));
        return None;
    }

    ui::trace(&format!(
        "`{branch}` looks like it was cut from `{}` ({} commit(s) ahead of it)",
        winner.name, winner.ahead
    ));
    Some(winner.name.clone())
}

/// Pick the likeliest parent from the measured candidates.
///
/// The parent of a branch is the branch it has fewest commits of its own
/// relative to: cut `feature` from `v3-changes` and `feature` is three commits
/// ahead of `v3-changes`, but three plus the whole of `v3-changes` ahead of
/// `main`. Ties go to the run's base branch, because when the history cannot
/// tell two branches apart the conservative target is the one the user named,
/// and then to a branch whose name does not start with a digit: GitHub names
/// the branch it creates for an issue `<number>-<title>`, so a tied candidate
/// like `2-buzbar` is another issue's untouched branch sitting on the same
/// commit, never the branch this one was cut from.
pub fn rank<'a>(candidates: &'a [Candidate], base_branch: &str) -> Option<&'a Candidate> {
    candidates
        .iter()
        // A candidate that already contains every commit we have is downstream
        // of us, not upstream: a pull request into it would be empty.
        .filter(|c| !(c.ahead == 0 && c.behind > 0))
        .min_by_key(|c| {
            (
                c.ahead,
                c.name != base_branch,
                looks_like_an_issue_branch(&c.name),
                c.behind,
                c.name.as_str(),
            )
        })
}

/// Whether a branch name looks like one GitHub cut for an issue, i.e. `1-foo`.
fn looks_like_an_issue_branch(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_digit())
}

/// Remote branches worth measuring, most recently updated first.
fn candidate_branches(ctx: &RepoContext, branch: &str) -> Vec<String> {
    let prefix = format!("{}/", ctx.remote);
    let listed = git::run(
        &ctx.root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            &format!("refs/remotes/{}/", ctx.remote),
        ],
    )
    .unwrap_or_default();

    let mut names: Vec<String> = listed
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .filter(|name| !name.is_empty() && *name != "HEAD" && *name != branch)
        .map(str::to_string)
        .collect();

    if names.len() > MAX_CANDIDATES {
        let base_in_tail = names[MAX_CANDIDATES..].contains(&ctx.base_branch);
        names.truncate(MAX_CANDIDATES);
        if base_in_tail {
            names.push(ctx.base_branch.clone());
        }
        ui::trace(&format!(
            "considering the {MAX_CANDIDATES} most recently updated branches as the parent of `{branch}`"
        ));
    }
    names
}

/// `rain/issue-N`, suffixed if that name is already taken locally or remotely.
pub fn pick_branch_name(ctx: &RepoContext, issue: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, ahead: usize, behind: usize) -> Candidate {
        Candidate {
            name: name.to_string(),
            ahead,
            behind,
        }
    }

    #[test]
    fn prefers_the_branch_we_have_fewest_commits_beyond() {
        // Three commits on top of `v3-changes`, which is itself five commits
        // ahead of `main`.
        let candidates = [candidate("main", 8, 0), candidate("v3-changes", 3, 0)];
        assert_eq!(rank(&candidates, "main").unwrap().name, "v3-changes");
    }

    #[test]
    fn a_branch_with_no_commits_yet_still_finds_its_parent() {
        // Freshly created from the issue page against `v3-changes`: the tips are
        // the same commit, and only `main` is behind.
        let candidates = [candidate("main", 5, 0), candidate("v3-changes", 0, 0)];
        assert_eq!(rank(&candidates, "main").unwrap().name, "v3-changes");
    }

    #[test]
    fn ignores_branches_cut_from_us() {
        // Someone branched off our branch and added two commits: it contains
        // everything we have, so it is downstream, not upstream.
        let candidates = [
            candidate("downstream", 0, 2),
            candidate("v3-changes", 3, 0),
            candidate("main", 8, 0),
        ];
        assert_eq!(rank(&candidates, "main").unwrap().name, "v3-changes");
    }

    #[test]
    fn a_tie_goes_to_the_base_branch() {
        // `release` and `main` point at the same commit; neither history can
        // tell us anything the other cannot.
        let candidates = [candidate("release", 3, 0), candidate("main", 3, 0)];
        assert_eq!(rank(&candidates, "main").unwrap().name, "main");
    }

    #[test]
    fn a_tie_between_two_non_base_branches_prefers_the_closer_one() {
        let candidates = [candidate("far", 3, 40), candidate("near", 3, 1)];
        assert_eq!(rank(&candidates, "main").unwrap().name, "near");
        // …and is deterministic when even that ties.
        let level = [candidate("beta", 3, 1), candidate("alpha", 3, 1)];
        assert_eq!(rank(&level, "main").unwrap().name, "alpha");
    }

    #[test]
    fn a_tie_prefers_a_branch_that_is_not_another_issues_branch() {
        // Issue #1's branch and issue #2's branch were both cut from `baz` and
        // neither has been worked on, so all three tips are the same commit.
        // `2-buzbar` is a sibling, not a parent.
        let candidates = [
            candidate("2-buzbar", 0, 0),
            candidate("baz", 0, 0),
            candidate("main", 5, 0),
        ];
        assert_eq!(rank(&candidates, "main").unwrap().name, "baz");
    }

    #[test]
    fn an_issue_branch_still_wins_when_the_history_says_so() {
        // Only the digit-led name is a plausible parent: it is the one we have
        // fewest commits beyond, which outranks the naming heuristic.
        let candidates = [candidate("3-stacked-on", 2, 0), candidate("main", 9, 0)];
        assert_eq!(rank(&candidates, "main").unwrap().name, "3-stacked-on");
    }

    #[test]
    fn a_digit_led_base_branch_still_wins_its_ties() {
        // The user named it, so it outranks the naming heuristic.
        let candidates = [candidate("2024-release", 3, 0), candidate("other", 3, 0)];
        assert_eq!(
            rank(&candidates, "2024-release").unwrap().name,
            "2024-release"
        );
    }

    #[test]
    fn an_ordinary_branch_lands_on_the_base_branch() {
        let candidates = [candidate("main", 3, 0), candidate("v3-changes", 12, 0)];
        assert_eq!(rank(&candidates, "main").unwrap().name, "main");
    }

    #[test]
    fn nothing_to_rank_is_no_answer() {
        assert!(rank(&[], "main").is_none());
        // Every candidate already contains our work.
        assert!(rank(&[candidate("main", 0, 4)], "main").is_none());
    }
}
