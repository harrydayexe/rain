//! The prompts rain sends to Claude Code.
//!
//! These are the tool's actual interface to the model, so they live in one file
//! where they can be read end to end and changed deliberately.

use crate::github::{Check, Issue};
use crate::repo::RepoSlug;

/// Appended to the system prompt of every session rain starts.
///
/// The tooling already denies the dangerous commands outright; saying so here
/// means the agent plans around the restriction instead of discovering it.
pub const GUARDRAILS: &str = "\
You are running unattended as part of `rain`, an automated issue-to-PR tool. No human is \
watching this session, so never ask a question you could answer by reading the code, and never \
wait for confirmation.

Absolute rules:
- Work only on the branch you were given. Never commit to, push to, or check out the base branch.
- Never force-push, rewrite published history, amend a pushed commit, or delete a remote branch.
- Never merge a pull request, create a tag or release, change git remotes, or alter repository settings.
- Never edit CI workflow files to make a failing check pass. Fix the code the check is complaining about.
- Never weaken, skip, delete, or `#[ignore]` a test to make it pass unless the issue explicitly asks for that test to go away.
- Never commit secrets, credentials, or `.env` files.

If you genuinely cannot finish, commit and push whatever coherent partial work you have and \
explain precisely what is left in your final message. A clear account of an incomplete job is \
far more useful than a broken or invented one.";

/// The opening prompt: read the issue, implement it, commit, push.
pub fn implement(issue: &Issue, slug: &RepoSlug, branch: &str, base: &str) -> String {
    format!(
        "Implement GitHub issue #{number} in the repository {slug}.

Title: {title}
URL: {url}
Labels: {labels}

<issue-body>
{body}
</issue-body>

You are in a fresh git worktree on branch `{branch}`, cut from `{base}`. The working tree is \
clean and the branch has no commits of its own yet.

Do this:
1. Read the issue above carefully and decide what \"done\" means for it. The issue is the \
specification; if it states acceptance criteria, treat them as the contract.
2. Explore the codebase before changing anything. Match the conventions already there — naming, \
error handling, module layout, test style, comment density.
3. Implement the change. Keep it to the scope of the issue: no drive-by refactors, no unrelated \
formatting churn.
4. Add or update tests that would fail without your change. If the project has no test setup at \
all, say so in your final message rather than inventing one.
5. Run the project's own build, test and lint commands (check README, CI workflow files, \
Makefile, or the package manifest to find them) and get them passing.
6. Commit as you go, in logical commits, using Conventional Commits \
(`<type>(<scope>): <subject>`, imperative and lowercase). Reference the issue in a commit body or \
your final commit with `Refs #{number}`.
7. Push with `git push -u origin {branch}` when you are finished.

Do not open the pull request — rain opens it once you are done, and will run CI and a review pass \
after that.

Your final message is the handover note. State what you changed, which commands you ran to verify \
it, and anything a reviewer should look at closely.",
        number = issue.number,
        slug = slug,
        title = issue.title,
        url = issue.url,
        labels = if issue.labels.is_empty() {
            "(none)".to_string()
        } else {
            issue.labels.join(", ")
        },
        body = if issue.body.trim().is_empty() {
            "(the issue has no description)"
        } else {
            issue.body.trim()
        },
        branch = branch,
        base = base,
    )
}

/// Feed a CI failure back to the agent.
pub fn fix_ci(
    issue: &Issue,
    branch: &str,
    pr: u64,
    failures: &[Check],
    logs: Option<&str>,
    attempt: u32,
    max_attempts: u32,
) -> String {
    let failing = failures
        .iter()
        .map(|c| format!("- {}", c.describe()))
        .collect::<Vec<_>>()
        .join("\n");

    let log_section = match logs {
        Some(text) if !text.trim().is_empty() => format!(
            "\nThe tail of the failing job log:\n\n<ci-log>\n{}\n</ci-log>\n",
            text.trim()
        ),
        _ => String::new(),
    };

    format!(
        "CI is failing on pull request #{pr} for branch `{branch}` (issue #{number}). \
This is fix attempt {attempt} of {max_attempts}.

Failing checks:
{failing}
{log_section}
Do this:
1. Read the full failure yourself — `gh pr checks {pr}` lists the checks, and \
`gh run view <run-id> --log-failed` prints the failing job's log. Do not guess from the summary \
above.
2. Work out the actual cause. A test that fails in CI but passes locally usually means an \
environment, ordering, timing or fixture assumption — find it rather than papering over it.
3. Fix the underlying problem in the code. Do not edit the workflow, relax the check, or disable \
the test to get green.
4. Reproduce the failure locally first where you can, then confirm your fix.
5. Commit with a Conventional Commit message and push to `{branch}`.

If the failure is in code unrelated to your change and pre-exists on the base branch, say so \
clearly in your final message instead of trying to fix the whole repository.",
        pr = pr,
        branch = branch,
        number = issue.number,
        attempt = attempt,
        max_attempts = max_attempts,
        failing = failing,
        log_section = log_section,
    )
}

/// The reviewer pass: read the PR, report real problems, change nothing.
pub fn review(issue: &Issue, slug: &RepoSlug, pr: u64, pr_url: &str) -> String {
    format!(
        "Review pull request #{pr} in {slug} ({pr_url}). It was written by another agent to \
close issue #{number}, and CI is passing.

Title: {title}

<issue-body>
{body}
</issue-body>

Read the change with `gh pr diff {pr}`, then read the surrounding code for context — a diff alone \
hides most real bugs.

Judge it on:
- Correctness. Does it actually work? Look for off-by-one errors, unhandled errors, wrong \
conditionals, race conditions, resource leaks, and cases the code silently drops.
- The contract. Does it do what issue #{number} asked, including any acceptance criteria? Flag \
requirements that were missed, and scope that was added beyond the issue.
- Tests. Do the new tests actually fail without the change? Are the interesting cases covered, or \
only the happy path?
- Safety. Injection, unvalidated input, secrets in the diff, permission mistakes.

Do not comment on formatting, naming preferences, or anything a linter owns. Do not modify any \
files — you have read-only access and reporting is your whole job.

If the change is sound, reply with exactly:
NO ISSUES FOUND

Otherwise reply with a numbered list. Each item must give the file and line, what is wrong, why it \
matters, and the concrete fix. Report only problems you are confident about; a speculative finding \
costs more than it saves, because the next agent will act on everything you write.",
        pr = pr,
        slug = slug,
        pr_url = pr_url,
        number = issue.number,
        title = issue.title,
        body = if issue.body.trim().is_empty() {
            "(the issue has no description)"
        } else {
            issue.body.trim()
        },
    )
}

/// Action the reviewer's findings, once.
pub fn address_review(issue: &Issue, branch: &str, pr: u64, feedback: &str) -> String {
    format!(
        "A reviewer looked at pull request #{pr} (issue #{number}, branch `{branch}`) and raised \
the following.

<review>
{feedback}
</review>

Work through each point in order:
- If the point is correct, fix it properly rather than minimally.
- If you disagree, do not change the code. Explain your reasoning in your final message so the \
human reviewer can judge between you.

This is the only pass you get on this feedback, so finish everything you intend to do. Re-run the \
project's tests and lints afterwards, commit with a Conventional Commit message, and push to \
`{branch}`.

In your final message, list each review point and say whether you fixed it or declined it, and why.",
        pr = pr,
        number = issue.number,
        branch = branch,
        feedback = feedback.trim(),
    )
}

/// The body of the pull request rain opens.
pub fn pr_body(issue: &Issue, handover: &str) -> String {
    let notes = handover.trim();
    let notes = if notes.is_empty() {
        "_The agent did not leave a handover note._"
    } else {
        notes
    };
    format!(
        "Closes #{number}

> [!NOTE]
> This pull request was written by [rain](https://github.com/harrydayexe/rain) running Claude \
Code. It has not been reviewed by a human. CI and an automated review pass run before it is \
handed over.

## Issue

[#{number}: {title}]({url})

## What the agent reports

{notes}
",
        number = issue.number,
        title = issue.title,
        url = issue.url,
        notes = notes,
    )
}

/// The title of the pull request rain opens.
pub fn pr_title(issue: &Issue) -> String {
    let title = issue.title.trim();
    // GitHub's own limit is generous, but a title that wraps in every list view
    // helps nobody.
    let clipped: String = if title.chars().count() > 100 {
        format!("{}…", title.chars().take(99).collect::<String>())
    } else {
        title.to_string()
    };
    format!("{clipped} (#{})", issue.number)
}

/// The review transcript rain posts to the PR, so the human reviewer can see
/// what the automated pass already caught.
pub fn review_comment(findings: &str, addressed: Option<&str>) -> String {
    let mut body = String::from("## Automated review\n\n");
    body.push_str(findings.trim());
    body.push('\n');
    if let Some(response) = addressed {
        body.push_str("\n## Agent response\n\n");
        body.push_str(response.trim());
        body.push('\n');
    }
    body.push_str("\n---\n_Posted by rain. Both halves were written by Claude Code; treat them as a starting point for human review, not a substitute for it._\n");
    body
}

/// Whether a reviewer reply means "nothing to do".
pub fn review_is_clean(text: &str) -> bool {
    let normalised: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_whitespace())
        .collect();
    let normalised = normalised.split_whitespace().collect::<Vec<_>>().join(" ");
    // The agent reliably ends with the sentinel but sometimes prefaces it.
    normalised.is_empty() || normalised.ends_with("no issues found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue() -> Issue {
        Issue {
            number: 12,
            title: "Add retry to the uploader".into(),
            body: "It should retry three times.".into(),
            url: "https://github.com/o/n/issues/12".into(),
            state: "OPEN".into(),
            labels: vec!["bug".into()],
        }
    }

    #[test]
    fn implement_prompt_carries_the_issue() {
        let slug = RepoSlug::parse("o/n").unwrap();
        let p = implement(&issue(), &slug, "rain/issue-12", "main");
        assert!(p.contains("#12"));
        assert!(p.contains("It should retry three times."));
        assert!(p.contains("rain/issue-12"));
        assert!(p.contains("git push -u origin rain/issue-12"));
    }

    #[test]
    fn implement_prompt_handles_an_empty_body() {
        let slug = RepoSlug::parse("o/n").unwrap();
        let mut i = issue();
        i.body = "  ".into();
        assert!(implement(&i, &slug, "b", "main").contains("no description"));
    }

    #[test]
    fn pr_title_appends_the_issue_number() {
        assert_eq!(pr_title(&issue()), "Add retry to the uploader (#12)");
    }

    #[test]
    fn pr_title_is_clipped() {
        let mut i = issue();
        i.title = "x".repeat(200);
        assert!(pr_title(&i).chars().count() <= 106);
    }

    #[test]
    fn pr_body_closes_the_issue() {
        let body = pr_body(&issue(), "Added a retry loop.");
        assert!(body.contains("Closes #12"));
        assert!(body.contains("Added a retry loop."));
    }

    #[test]
    fn recognises_a_clean_review() {
        assert!(review_is_clean("NO ISSUES FOUND"));
        assert!(review_is_clean("no issues found"));
        assert!(review_is_clean(
            "I read the diff and the surrounding code.\n\nNO ISSUES FOUND"
        ));
        assert!(review_is_clean("   "));
    }

    #[test]
    fn recognises_a_review_with_findings() {
        assert!(!review_is_clean(
            "1. src/lib.rs:20 — off-by-one in the loop bound."
        ));
        assert!(!review_is_clean(
            "NO ISSUES FOUND with the tests, but the parser is wrong."
        ));
    }
}
