//! GitHub access, via the `gh` CLI.
//!
//! Shelling out to `gh` means auth, enterprise hosts, pagination and rate-limit
//! retries are already solved and already match whatever the user has
//! configured. The whole surface rain needs is behind [`Forge`], so swapping in
//! an HTTP client later is a contained change.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::repo::RepoSlug;
use crate::ui;

/// One GitHub issue, reduced to what rain acts on.
#[derive(Debug, Clone)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub labels: Vec<String>,
}

impl Issue {
    pub fn is_open(&self) -> bool {
        self.state.eq_ignore_ascii_case("open")
    }
}

/// A pull request rain opened.
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

/// A single CI check on a pull request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub name: String,
    /// `pass`, `fail`, `pending`, `skipping` or `cancel`.
    pub bucket: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub workflow: String,
}

impl Check {
    /// `CI / test [fail, FAILURE] — 2 failing (https://…)`, for prompts and logs.
    pub fn describe(&self) -> String {
        let mut line = String::new();
        if !self.workflow.is_empty() {
            line.push_str(&format!("{} / ", self.workflow));
        }
        line.push_str(&self.name);
        if self.state.is_empty() || self.state.eq_ignore_ascii_case(&self.bucket) {
            line.push_str(&format!(" [{}]", self.bucket));
        } else {
            line.push_str(&format!(" [{}, {}]", self.bucket, self.state));
        }
        if !self.description.is_empty() {
            line.push_str(&format!(" — {}", self.description));
        }
        if !self.link.is_empty() {
            line.push_str(&format!(" ({})", self.link));
        }
        line
    }
}

/// What `gh pr checks` reported this poll.
#[derive(Debug, Clone)]
pub enum ChecksSnapshot {
    /// No check suite has reported yet — either CI is not configured or it has
    /// not started. The caller decides which by waiting.
    NotReported,
    Reported(Vec<Check>),
}

#[derive(Deserialize)]
struct RawIssue {
    number: u64,
    title: String,
    #[serde(default)]
    body: String,
    url: String,
    state: String,
    #[serde(default)]
    labels: Vec<RawLabel>,
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
}

#[derive(Deserialize)]
struct RawRef {
    number: u64,
}

#[derive(Deserialize)]
struct RawPr {
    number: u64,
    url: String,
}

#[derive(Deserialize)]
struct RawDefaultBranch {
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<RawBranchRef>,
}

#[derive(Deserialize)]
struct RawBranchRef {
    name: String,
}

/// The `gh`-backed forge client.
pub struct Forge {
    slug: RepoSlug,
}

struct GhOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

impl GhOutput {
    fn success(&self) -> bool {
        self.status == 0
    }
    fn message(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.trim().to_string()
        } else {
            self.stderr.trim().to_string()
        }
    }
}

impl Forge {
    pub fn new(slug: RepoSlug) -> Self {
        Self { slug }
    }

    pub fn slug(&self) -> &RepoSlug {
        &self.slug
    }

    fn repo_arg(&self) -> String {
        self.slug.to_string()
    }

    fn try_gh(&self, args: &[&str], stdin: Option<&str>) -> Result<GhOutput> {
        ui::trace(&format!("gh {}", args.join(" ")));
        let mut cmd = Command::new("gh");
        cmd.args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("failed to run gh (is the GitHub CLI installed?): {e}"))?;
        if let Some(body) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            pipe.write_all(body.as_bytes())
                .context("writing to gh stdin")?;
        }
        let out = child.wait_with_output().context("waiting for gh")?;
        Ok(GhOutput {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }

    fn gh(&self, args: &[&str]) -> Result<String> {
        let out = self.try_gh(args, None)?;
        if !out.success() {
            bail!(
                "gh {} failed ({}): {}",
                args.join(" "),
                out.status,
                out.message()
            );
        }
        Ok(out.stdout)
    }

    /// Confirm `gh` is installed and authenticated before any work starts.
    pub fn preflight(&self) -> Result<()> {
        let version = self
            .try_gh(&["--version"], None)
            .context("gh is required but could not be run")?;
        if !version.success() {
            bail!("`gh --version` failed: {}", version.message());
        }

        let auth = self.try_gh(&["auth", "status"], None)?;
        if !auth.success() {
            bail!(
                "gh is not authenticated — run `gh auth login` first.\n{}",
                auth.message()
            );
        }

        let repo = self.try_gh(&["repo", "view", &self.repo_arg(), "--json", "name"], None)?;
        if !repo.success() {
            bail!(
                "cannot read {} with the current gh credentials: {}",
                self.slug,
                repo.message()
            );
        }
        Ok(())
    }

    pub fn default_branch(&self) -> Option<String> {
        let out = self
            .try_gh(
                &[
                    "repo",
                    "view",
                    &self.repo_arg(),
                    "--json",
                    "defaultBranchRef",
                ],
                None,
            )
            .ok()?;
        if !out.success() {
            return None;
        }
        serde_json::from_str::<RawDefaultBranch>(&out.stdout)
            .ok()?
            .default_branch_ref
            .map(|r| r.name)
    }

    pub fn issue(&self, number: u64) -> Result<Issue> {
        let n = number.to_string();
        let json = self
            .gh(&[
                "issue",
                "view",
                &n,
                "--repo",
                &self.repo_arg(),
                "--json",
                "number,title,body,url,state,labels",
            ])
            .with_context(|| format!("fetching issue #{number} from {}", self.slug))?;
        let raw: RawIssue =
            serde_json::from_str(&json).with_context(|| format!("parsing issue #{number} JSON"))?;
        Ok(Issue {
            number: raw.number,
            title: raw.title,
            body: raw.body,
            url: raw.url,
            state: raw.state,
            labels: raw.labels.into_iter().map(|l| l.name).collect(),
        })
    }

    /// Issues that must land before `number`, per GitHub's native issue
    /// dependencies. Returns `None` when the API is unavailable — the feature is
    /// not enabled on every repo or plan, and its absence is not an error.
    pub fn blocked_by(&self, number: u64) -> Option<Vec<u64>> {
        self.dependency_edge(number, "blocked_by")
    }

    /// Issues that cannot start until `number` lands.
    pub fn blocking(&self, number: u64) -> Option<Vec<u64>> {
        self.dependency_edge(number, "blocking")
    }

    fn dependency_edge(&self, number: u64, edge: &str) -> Option<Vec<u64>> {
        let path = format!(
            "repos/{}/{}/issues/{}/dependencies/{}",
            self.slug.owner, self.slug.name, number, edge
        );
        let out = self.try_gh(&["api", "--paginate", &path], None).ok()?;
        if !out.success() {
            ui::trace(&format!(
                "issue dependencies API unavailable for #{number} ({edge}): {}",
                ui::clip(&out.message(), 120)
            ));
            return None;
        }
        // `--paginate` concatenates arrays; parse leniently.
        let refs: Vec<RawRef> = serde_json::from_str(&out.stdout).ok()?;
        Some(refs.into_iter().map(|r| r.number).collect())
    }

    /// The open PR for `branch`, if rain (or anyone) already opened one.
    pub fn find_pr_for_branch(&self, branch: &str) -> Result<Option<PullRequest>> {
        let json = self.gh(&[
            "pr",
            "list",
            "--repo",
            &self.repo_arg(),
            "--head",
            branch,
            "--state",
            "open",
            "--limit",
            "1",
            "--json",
            "number,url",
        ])?;
        let prs: Vec<RawPr> = serde_json::from_str(&json).context("parsing PR list JSON")?;
        Ok(prs.into_iter().next().map(|p| PullRequest {
            number: p.number,
            url: p.url,
        }))
    }

    pub fn create_pr(
        &self,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<PullRequest> {
        let repo = self.repo_arg();
        let mut args = vec![
            "pr",
            "create",
            "--repo",
            &repo,
            "--base",
            base,
            "--head",
            branch,
            "--title",
            title,
            "--body-file",
            "-",
        ];
        if draft {
            args.push("--draft");
        }
        let out = self.try_gh(&args, Some(body))?;
        if !out.success() {
            bail!(
                "could not open a pull request for `{branch}`: {}",
                out.message()
            );
        }
        // `gh pr create` prints the PR URL, but re-reading it is more robust than
        // scraping stdout and also gives us the number.
        self.find_pr_for_branch(branch)?
            .ok_or_else(|| anyhow!("gh reported success but no open PR exists for `{branch}`"))
    }

    pub fn pr_checks(&self, pr: u64) -> Result<ChecksSnapshot> {
        let n = pr.to_string();
        let out = self.try_gh(
            &[
                "pr",
                "checks",
                &n,
                "--repo",
                &self.repo_arg(),
                "--json",
                "name,state,bucket,link,description,workflow",
            ],
            None,
        )?;

        // gh exits 8 while checks are pending and 1 both for failures and for
        // "no checks reported" — the payload is what distinguishes them.
        let body = out.stdout.trim();
        if body.is_empty() {
            let msg = out.message();
            if out.success() || msg.contains("no checks reported") || msg.contains("no checks") {
                return Ok(ChecksSnapshot::NotReported);
            }
            bail!("`gh pr checks {pr}` failed ({}): {msg}", out.status);
        }

        let checks: Vec<Check> =
            serde_json::from_str(body).context("parsing `gh pr checks` JSON")?;
        if checks.is_empty() {
            return Ok(ChecksSnapshot::NotReported);
        }
        Ok(ChecksSnapshot::Reported(checks))
    }

    pub fn comment_pr(&self, pr: u64, body: &str) -> Result<()> {
        let n = pr.to_string();
        let out = self.try_gh(
            &[
                "pr",
                "comment",
                &n,
                "--repo",
                &self.repo_arg(),
                "--body-file",
                "-",
            ],
            Some(body),
        )?;
        if !out.success() {
            bail!("could not comment on PR #{pr}: {}", out.message());
        }
        Ok(())
    }

    /// Best-effort tail of the failing job logs, to put in the run report.
    /// The agent fetches its own logs; this is for the human reading the summary.
    pub fn failed_run_log(&self, check: &Check, lines: usize) -> Option<String> {
        let run_id = extract_run_id(&check.link)?;
        let out = self
            .try_gh(
                &[
                    "run",
                    "view",
                    &run_id,
                    "--repo",
                    &self.repo_arg(),
                    "--log-failed",
                ],
                None,
            )
            .ok()?;
        if !out.success() {
            return None;
        }
        let tail: Vec<&str> = out
            .stdout
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if tail.is_empty() {
            None
        } else {
            Some(tail.join("\n"))
        }
    }
}

/// Pull the Actions run ID out of a check's details URL.
fn extract_run_id(link: &str) -> Option<String> {
    let after = link.split("/actions/runs/").nth(1)?;
    let id: String = after.chars().take_while(char::is_ascii_digit).collect();
    if id.is_empty() { None } else { Some(id) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_actions_run_id() {
        assert_eq!(
            extract_run_id("https://github.com/o/n/actions/runs/123456789/job/987").as_deref(),
            Some("123456789")
        );
        assert_eq!(
            extract_run_id("https://github.com/o/n/actions/runs/42").as_deref(),
            Some("42")
        );
        assert_eq!(extract_run_id("https://example.com/build/7"), None);
        assert_eq!(extract_run_id(""), None);
    }

    #[test]
    fn describes_a_check() {
        let check = Check {
            name: "test".into(),
            bucket: "fail".into(),
            state: "FAILURE".into(),
            link: "https://github.com/o/n/actions/runs/1".into(),
            description: "2 failing".into(),
            workflow: "CI".into(),
        };
        assert_eq!(
            check.describe(),
            "CI / test [fail, FAILURE] — 2 failing (https://github.com/o/n/actions/runs/1)"
        );
    }
}
