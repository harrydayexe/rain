# rain

**R**ust **A**utomated **I**ssue e**N**gine.

Give rain a bare clone and some issue numbers. It gives back draft pull
requests that pass CI and have already been through a round of automated
review.

```console
$ rain 12 14 15
```

Each issue gets its own worktree, its own branch, and its own headless Claude
Code session. rain opens the pull request, waits for CI, feeds failures back to
the agent, runs a second agent as a reviewer, and finishes with a summary of
what is ready for you and what is not.

> [!WARNING]
> rain writes code, pushes branches and opens pull requests without asking.
> Point it at repositories you own, and read what it produces before merging.
> Everything it opens is a draft by default, because it generates review load
> and that should be explicit.

---

## Requirements

| Tool | Why |
|---|---|
| [`git`](https://git-scm.com) ≥ 2.31 | worktrees, `--path-format=absolute` |
| [`gh`](https://cli.github.com) | issues, pull requests, CI status — must be authenticated (`gh auth login`) |
| [Claude Code](https://claude.com/claude-code) | the agent itself — must be logged in |

Build it with `cargo build --release`; the binary lands in `target/release/rain`.

## Running it

rain is designed to run from a **bare clone**, so it never touches a checkout
you are working in:

```console
$ git clone --bare https://github.com/you/project.git project.git
$ cd project.git
$ rain 12 14 15
```

Start with `--dry-run` to see the queue rain resolved without changing
anything:

```console
$ rain --dry-run 12 14 15

  Queue
    1. #14 Add the config loader
    2. #12 Wire the loader into startup
       after #14 (issue body)
    3. #15 Document the config format
       after #12 (github dependencies)
```

### Useful flags

```
--dry-run                  resolve the queue and print the plan, change nothing
--model <MODEL>            default: opus
--effort <LEVEL>           low | medium | high (default) | xhigh | max
--base <BRANCH>            default: the repository's default branch
--ready                    open pull requests ready for review instead of drafts
--no-review                skip the automated review pass
--max-ci-retries <N>       CI fix attempts per issue (default: 3)
--ci-timeout <DURATION>    how long to wait for CI (default: 25m)
--agent-timeout <DURATION> wall-clock budget per agent session (default: 60m)
--max-wait <DURATION>      total time rain may sleep on usage limits (default: 8h)
--no-wait-on-limit         stop on a usage limit instead of sleeping
--ignore-relationships     work the issues in the order given
--ignore-linked-branches   always cut a new branch, never reuse a linked one
--keep-worktrees           leave worktrees behind for debugging
-v, --verbose              log every git, gh and agent action
```

Durations are written `45s`, `25m`, `2h`, `1d`.

**Exit codes.** `0` when every issue ended ready for review, `1` when some did
not, `2` when rain itself could not run.

## What it does, per issue

```
worktree → implement → push → open draft PR → CI ⇄ fix (≤ N) → review → address → hand over
```

1. **Queue.** rain reads each issue and sorts them topologically. Relationships
   come from GitHub's issue dependencies where the repository has them, and from
   prose in the issue body either way — `Blocked by #3`, `Depends on #7`,
   `This blocks #9`. Ties break on the order you listed the issues, so the queue
   rain prints is the queue rain works. A dependency cycle is reported, not
   fatal.

2. **Branch.** If GitHub already has a branch linked to the issue, rain works on
   that one and targets the branch it was cut from — see [Branches rain did not
   create](#branches-rain-did-not-create). Otherwise a fresh worktree on
   `rain/issue-N`, cut from the base branch. A new branch is created with
   `--no-track`, so a bare `git push` inside it cannot land on the base branch.
   A leftover worktree from a crashed run is reclaimed rather than orphaned; a
   branch name already in use gets a suffix.

3. **Implement.** A headless Claude Code session reads the issue, explores the
   codebase, makes the change, adds tests, runs the project's own build and test
   commands, commits in Conventional Commits style, and pushes.

4. **Pull request.** rain opens it — not the agent — as a draft, with `Closes
   #N` and the agent's handover note in the body.

5. **CI.** rain polls until every check settles. On failure it hands the failing
   checks and a tail of the job log back to the agent, up to `--max-ci-retries`
   times. If an attempt produces no new commits, rain stops rather than looping.

6. **Review.** A second session, with write access denied, reviews the pull
   request and reports only substantive problems. Findings are actioned exactly
   once, both halves are posted to the PR as a comment, and CI is re-checked.

7. **Report.** A summary on the terminal, plus `run.json` and `summary.md`
   alongside every session transcript, under `<git-dir>/rain/runs/<timestamp>/`.

A failure at any stage is an outcome to report, not a reason to abandon the
remaining issues.

## Branches rain did not create

An issue often has a branch already. Someone used the **Development** panel on
the issue, or the `create a branch` link, and that branch is where everyone
involved expects the work to appear. rain uses it instead of cutting
`rain/issue-N` next to it, and continues whatever commits are already on it
rather than starting again.

Such a branch is not always cut from the default branch, and where it came from
decides where it goes back to. A branch cut from `v3-changes` gets a pull
request into `v3-changes`; opening it against `main` would put the whole of
`v3-changes` in the diff and, if merged, land far more than the issue asked
for.

Git records no such thing as a parent branch, so rain works it out in
descending order of confidence:

| Taken from | Because |
|---|---|
| `--base <BRANCH>` | An explicit flag is an instruction, and settles it. |
| The open pull request | If the branch already has one, its base was chosen by a person. |
| The history | The branch rain has fewest commits beyond — cut `feature` from `v3-changes` and it is three commits ahead of `v3-changes`, but three plus the whole of `v3-changes` ahead of `main`. Branches cut _from_ this one are excluded; they are downstream, not upstream. |
| The default branch | When nothing above answers, or when the history cannot separate two branches. |

When the history ties, rain prefers the run's base branch, and then a branch
whose name does not start with a digit. GitHub names the branch it creates for
an issue `<number>-<title>`, so when issues #1 and #2 both have a branch cut
from `baz` and neither has been worked on yet, all three tips are the same
commit: `2-buzbar` is a sibling of `1-foobar`, not its parent, and `baz` is the
target.

A pull request that targets a branch rain worked out for itself says so in its
own body, and every task records its branch and target in `summary.md` and
`run.json`.

Two situations stop the task rather than guess:

- **The branch is checked out elsewhere.** rain will not take a branch out from
  under another worktree.
- **The local and remote copies have diverged.** rain may not force-push, so
  work on a diverged local branch could never be pushed, and discarding either
  side silently is worse than stopping. A local copy that is merely behind is
  fast-forwarded.

`--ignore-linked-branches` turns all of this off and always cuts
`rain/issue-N`.

## Usage limits

rain runs on a Claude subscription, where the scarce resource is quota rather
than money. Claude Code reports its own quota state on the headless stream —
which window is binding, when it resets, and how much of the five-hour and
seven-day windows is gone — so when a session is cut off, rain sleeps until the
exact reset time and carries on where it left off. Without that telemetry it
falls back to an escalating backoff. `--max-wait` bounds the total.

The closing summary reports where the quota stands, which is the number that
matters the morning after:

```
quota: 5-hour window 20% used (resets 13:00 Fri 18 Sep), weekly 17% used (resets 20:00 Wed 23 Sep)
```

Claude Code shares your subscription quota with claude.ai and the desktop app,
so rain competes with you for it.

> [!NOTE]
> rain never runs on Fable models; `--model fable` is rejected. Weekly caps
> exist partly to curb round-the-clock automated use — an overnight run against
> your own repositories is ordinary usage, a process pinned at the cap
> permanently is not.

## Safety

The destructive operations are blocked by two independent mechanisms, because
one is a guardrail and two is a guarantee.

| Operation | Blocked by |
|---|---|
| Committing or pushing to the base branch | an upstream that is never the base branch — `--no-track` on a branch rain cut, the branch's own remote counterpart on one it did not — and rain verifying afterwards that none of the task's commits reached the base branch |
| Force push, history rewrite, remote branch deletion | denied tool patterns, and a fully-qualified push refspec |
| Merging a PR, tags, releases, changing remotes | denied tool patterns, and the system prompt |
| The reviewer changing code | `Edit`, `Write` and the git write commands denied for that session |

rain also never operates on your existing checkout, opens drafts by default,
and bounds every wait — agent sessions, CI, and usage limits all have hard
timeouts rather than an infinite wait.

## Run records

```
<git-dir>/rain/
  worktrees/rain-issue-12/          removed on completion (kept if left dirty)
  runs/20260918-021500/
    run.json                        the whole run, machine-readable
    summary.md                      the whole run, readable
    issue-12/
      implement.jsonl               raw stream-json transcripts
      ci-fix-1.jsonl
      review.jsonl
      review-fix.jsonl
```

Every transcript is the verbatim stream, so any decision the agent made can be
audited after the fact. `run.json` records each session's Claude Code session
ID, so you can reopen one with `claude --resume <id>`.

## Development

```console
$ cargo test              # unit tests, plus git integration against a real bare clone
$ cargo clippy --all-targets
```

The live agent tests are ignored by default because they spend quota and need a
Claude Code login:

```console
$ cargo test --test agent_smoke -- --ignored --nocapture
```

## Not in v0.1.0

Deliberately cut, to get one path working end to end first: multi-repo support,
concurrency above one issue at a time, scheduling to overnight windows, a
`rain.yaml` config file, a persistent state store that survives being killed
mid-issue, issue discovery by label (`rain sync`), a quota reserve floor that
halts new work, and no-progress detection beyond the CI loop's "no new commits"
check.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
