//! Discovering which GitHub repository we are pointed at.
//!
//! rain is designed to be run from a bare clone — the clone exists purely as a
//! source of worktrees and has no checkout of its own to disturb. It also works
//! from inside a worktree of such a clone, because that is how you end up
//! testing it.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::git;
use crate::ui;

/// `owner/name` on github.com.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSlug {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl RepoSlug {
    /// Parse `owner/name`, rejecting anything with extra path segments.
    pub fn parse(spec: &str) -> Result<Self> {
        let spec = spec.trim().trim_end_matches('/');
        let mut parts = spec.split('/');
        let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            bail!("`{spec}` is not an owner/name repository slug");
        };
        if owner.is_empty() || name.is_empty() {
            bail!("`{spec}` is not an owner/name repository slug");
        }
        Ok(Self {
            owner: owner.to_string(),
            name: name.trim_end_matches(".git").to_string(),
        })
    }
}

/// Everything rain needs to know about the local clone.
#[derive(Debug, Clone)]
pub struct RepoContext {
    /// Directory every git command is run against.
    pub root: PathBuf,
    /// The shared git directory — where worktree metadata lives.
    pub common_dir: PathBuf,
    pub is_bare: bool,
    pub remote: String,
    pub slug: RepoSlug,
    /// The branch pull requests target unless a task works out a better one.
    pub base_branch: String,
    /// Whether `base_branch` came from `--base`. An explicit flag is an
    /// instruction, so it overrides anything a task infers for itself.
    pub base_explicit: bool,
}

impl RepoContext {
    /// `origin/main` — the ref new worktrees branch from.
    pub fn base_ref(&self) -> String {
        self.remote_ref(&self.base_branch)
    }

    /// `origin/<branch>` — the remote-tracking ref for any branch.
    pub fn remote_ref(&self, branch: &str) -> String {
        format!("{}/{}", self.remote, branch)
    }
}

/// Pull `owner/name` out of any of the URL shapes git accepts for GitHub.
pub fn parse_remote_url(url: &str) -> Result<RepoSlug> {
    let url = url.trim();
    let rest = if let Some(rest) = url.strip_prefix("git@") {
        // git@github.com:owner/repo.git
        rest.split_once(':').map(|(_host, path)| path).unwrap_or("")
    } else if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("ssh://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        // Strip any userinfo, then the host.
        let rest = rest.rsplit('@').next().unwrap_or(rest);
        match rest.split_once('/') {
            Some((_host, path)) => path,
            // ssh://git@github.com:owner/repo style
            None => rest.split_once(':').map(|(_host, path)| path).unwrap_or(""),
        }
    } else {
        bail!("cannot parse remote URL `{url}`");
    };

    let path = rest.trim_matches('/').trim_end_matches(".git");
    if path.is_empty() {
        bail!("remote URL `{url}` has no repository path");
    }
    if !url.contains("github.com") {
        bail!("remote URL `{url}` does not point at github.com — rain v0.1 only supports GitHub");
    }
    RepoSlug::parse(path).with_context(|| format!("remote URL `{url}`"))
}

/// Work out the clone layout, remote and GitHub slug from `cwd`.
pub fn discover(
    cwd: &Path,
    remote_override: Option<&str>,
    slug_override: Option<&str>,
) -> Result<RepoContext> {
    let inside = git::try_run(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    let common = git::try_run(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if !common.success() && !inside.success() {
        bail!(
            "{} is not inside a git repository — run rain from a bare clone of the repo you want to work on",
            cwd.display()
        );
    }
    let common_dir = PathBuf::from(common.stdout.trim());

    let is_bare = git::try_run(cwd, &["config", "--bool", "--get", "core.bare"])
        .map(|o| o.stdout.trim() == "true")
        .unwrap_or(false);

    // In a bare clone there is no worktree to stand in; drive git from the git
    // directory itself. In a regular clone use the top level so relative paths
    // behave the way the user expects.
    let root = if is_bare {
        common_dir.clone()
    } else {
        match git::try_run(cwd, &["rev-parse", "--show-toplevel"]) {
            Ok(o) if o.success() && !o.stdout.is_empty() => PathBuf::from(o.stdout.trim()),
            _ => cwd.to_path_buf(),
        }
    };

    if !is_bare {
        ui::warn(
            "this clone is not bare — rain is intended to run from a bare clone so it never touches a checkout you are using",
        );
    }

    let remote = pick_remote(&root, remote_override)?;

    let slug = match slug_override {
        Some(spec) => RepoSlug::parse(spec)?,
        None => {
            let url = git::run(&root, &["remote", "get-url", &remote])
                .with_context(|| format!("reading the URL of remote `{remote}`"))?;
            parse_remote_url(&url)
                .with_context(|| format!("remote `{remote}` of {}", root.display()))?
        }
    };

    Ok(RepoContext {
        root,
        common_dir,
        is_bare,
        remote,
        slug,
        base_branch: String::new(), // filled in by `resolve_base_branch`
        base_explicit: false,
    })
}

fn pick_remote(root: &Path, requested: Option<&str>) -> Result<String> {
    let listed = git::run(root, &["remote"])?;
    let remotes: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if remotes.is_empty() {
        bail!("this clone has no remotes — rain needs one pointing at GitHub");
    }
    match requested {
        Some(name) => {
            if remotes.contains(&name) {
                Ok(name.to_string())
            } else {
                Err(anyhow!(
                    "no remote named `{name}` (found: {})",
                    remotes.join(", ")
                ))
            }
        }
        None if remotes.contains(&"origin") => Ok("origin".to_string()),
        None if remotes.len() == 1 => Ok(remotes[0].to_string()),
        None => Err(anyhow!(
            "several remotes and none called `origin` ({}) — pick one with --remote",
            remotes.join(", ")
        )),
    }
}

/// Decide which branch PRs target, preferring an explicit flag, then the
/// remote's own idea of its default branch.
pub fn resolve_base_branch(
    ctx: &RepoContext,
    explicit: Option<&str>,
    from_forge: Option<String>,
) -> String {
    if let Some(base) = explicit {
        return base.to_string();
    }
    let head_ref = format!("refs/remotes/{}/HEAD", ctx.remote);
    if let Ok(out) = git::try_run(&ctx.root, &["symbolic-ref", "--short", &head_ref])
        && out.success()
        && let Some(branch) = out.stdout.trim().strip_prefix(&format!("{}/", ctx.remote))
        && !branch.is_empty()
    {
        return branch.to_string();
    }
    if let Some(branch) = from_forge
        && !branch.is_empty()
    {
        return branch;
    }
    ui::warn("could not determine the default branch; assuming `main`");
    "main".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_remote() {
        let slug = parse_remote_url("https://github.com/harrydayexe/rain.git").unwrap();
        assert_eq!(slug.to_string(), "harrydayexe/rain");
    }

    #[test]
    fn parses_https_remote_without_suffix() {
        let slug = parse_remote_url("https://github.com/harrydayexe/rain").unwrap();
        assert_eq!(slug.to_string(), "harrydayexe/rain");
    }

    #[test]
    fn parses_scp_style_remote() {
        let slug = parse_remote_url("git@github.com:harrydayexe/rain.git").unwrap();
        assert_eq!(slug.to_string(), "harrydayexe/rain");
    }

    #[test]
    fn parses_ssh_url_remote() {
        let slug = parse_remote_url("ssh://git@github.com/harrydayexe/rain.git").unwrap();
        assert_eq!(slug.to_string(), "harrydayexe/rain");
    }

    #[test]
    fn parses_remote_with_token_userinfo() {
        let slug = parse_remote_url("https://x-access-token:abc123@github.com/o/n.git").unwrap();
        assert_eq!(slug.to_string(), "o/n");
    }

    #[test]
    fn rejects_non_github_remote() {
        assert!(parse_remote_url("https://gitlab.com/o/n.git").is_err());
    }

    #[test]
    fn rejects_slug_with_extra_segments() {
        assert!(RepoSlug::parse("a/b/c").is_err());
        assert!(RepoSlug::parse("solo").is_err());
    }
}
