//! Thin wrapper around the `git` binary.
//!
//! rain shells out rather than linking libgit2: worktree creation, credential
//! helpers and push all behave exactly as they do for the user at the terminal,
//! which matters more here than type safety over plumbing.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};

use crate::ui;

/// Outcome of a git invocation, including a failed one.
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Run git in `dir`, returning the outcome whether or not it succeeded.
pub fn try_run(dir: &Path, args: &[&str]) -> Result<Output> {
    ui::trace(&format!("git -C {} {}", dir.display(), args.join(" ")));
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("failed to run git (is it installed and on PATH?): {e}"))?;
    Ok(Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_string(),
    })
}

/// Run git in `dir`, returning stdout and failing loudly on a non-zero exit.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = try_run(dir, args)?;
    if !out.success() {
        bail!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            if out.stderr.is_empty() {
                out.stdout.clone()
            } else {
                out.stderr.clone()
            }
        );
    }
    Ok(out.stdout)
}

/// Whether a git invocation exits zero. Used for existence probes where the
/// failure mode is "no" rather than an error worth reporting.
pub fn probe(dir: &Path, args: &[&str]) -> bool {
    try_run(dir, args).map(|o| o.success()).unwrap_or(false)
}

/// How many commits each of two refs has that the other does not.
///
/// One `rev-list` answers both halves: commits on `ours` that `theirs` lacks,
/// then commits on `theirs` that `ours` lacks. Refs with no common ancestor are
/// not an error — every commit on each side counts as its own.
pub fn divergence(dir: &Path, ours: &str, theirs: &str) -> Result<(usize, usize)> {
    let range = format!("{ours}...{theirs}");
    let out = run(dir, &["rev-list", "--left-right", "--count", &range])?;
    parse_divergence(&out)
        .ok_or_else(|| anyhow!("could not read `git rev-list --left-right --count`: {out}"))
}

fn parse_divergence(out: &str) -> Option<(usize, usize)> {
    let mut fields = out.split_whitespace();
    let ours = fields.next()?.parse().ok()?;
    let theirs = fields.next()?.parse().ok()?;
    Some((ours, theirs))
}

/// Stream a git invocation's output straight through to the terminal. Used for
/// `push`, where progress lines are the only sign anything is happening.
pub fn run_streaming(dir: &Path, args: &[&str]) -> Result<()> {
    ui::trace(&format!("git -C {} {}", dir.display(), args.join(" ")));
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| anyhow!("failed to run git: {e}"))?;
    if !status.success() {
        bail!("git {} failed ({})", args.join(" "), status);
    }
    Ok(())
}

pub fn version() -> Result<String> {
    let out = Command::new("git")
        .arg("--version")
        .output()
        .map_err(|e| anyhow!("git is not installed or not on PATH: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rev_list_counts() {
        assert_eq!(parse_divergence("3\t7"), Some((3, 7)));
        assert_eq!(parse_divergence("0       0\n"), Some((0, 0)));
        assert_eq!(parse_divergence(""), None);
        assert_eq!(parse_divergence("3"), None);
        assert_eq!(parse_divergence("a\tb"), None);
    }
}
