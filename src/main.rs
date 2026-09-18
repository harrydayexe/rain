//! rain — Rust Automated Issue eNgine.
//!
//! Give it issue numbers; it gives back pull requests that pass CI and have
//! already been through one round of automated review.

mod agent;
mod ci;
mod cli;
mod deps;
mod git;
mod github;
mod limits;
mod pipeline;
mod prompts;
mod repo;
mod report;
mod ui;
mod util;
mod worktree;

use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;

use crate::cli::{Cli, Options};
use crate::github::{Forge, Issue};
use crate::limits::{Governor, Policy};
use crate::pipeline::{Pipeline, Settings};
use crate::report::RunReport;

fn main() -> ExitCode {
    let opts = match Cli::parse().validate() {
        Ok(opts) => opts,
        Err(e) => {
            ui::init(false);
            ui::error(&format!("{e:#}"));
            return ExitCode::from(2);
        }
    };

    ui::init(opts.verbose);

    match run(opts) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            ui::error(&format!("{e:#}"));
            ExitCode::from(2)
        }
    }
}

fn run(opts: Options) -> Result<i32> {
    let started_at = Local::now();

    ui::step(&format!("rain {}", env!("CARGO_PKG_VERSION")));
    ui::trace(&git::version().unwrap_or_else(|_| "git: unknown".into()));

    // ── locate the repository ────────────────────────────────────────────────
    let cwd = std::env::current_dir().context("reading the working directory")?;
    let mut ctx = repo::discover(&cwd, opts.remote.as_deref(), opts.repo.as_deref())?;
    let forge = Forge::new(ctx.slug.clone());
    forge.preflight()?;
    ctx.base_branch = repo::resolve_base_branch(&ctx, opts.base.as_deref(), forge.default_branch());

    ui::ok(&format!(
        "{} — base branch `{}`, remote `{}`{}",
        ui::bold(&ctx.slug.to_string()),
        ctx.base_branch,
        ctx.remote,
        if ctx.is_bare { ", bare clone" } else { "" }
    ));

    // ── build the queue ──────────────────────────────────────────────────────
    let (issues, mut warnings) = fetch_issues(&forge, &opts.issues)?;
    let relations = if opts.ignore_relationships {
        ui::info("--ignore-relationships: working the issues in the order given");
        Vec::new()
    } else {
        gather_relations(&forge, &issues)
    };
    let sequenced = deps::sequence(&opts.issues, &relations);
    warnings.extend(sequenced.warnings.iter().cloned());

    print_queue(&sequenced, &issues, &relations);
    for warning in &warnings {
        ui::warn(warning);
    }

    if opts.dry_run {
        ui::blank();
        ui::ok("--dry-run: nothing was changed");
        return Ok(0);
    }

    // ── set up this run's directories ────────────────────────────────────────
    let rain_dir = ctx.common_dir.join("rain");
    let run_dir = opts.run_dir.clone().unwrap_or_else(|| {
        rain_dir
            .join("runs")
            .join(started_at.format("%Y%m%d-%H%M%S").to_string())
    });
    let worktree_root = opts
        .work_dir
        .clone()
        .unwrap_or_else(|| rain_dir.join("worktrees"));
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("creating the run directory {}", run_dir.display()))?;
    ui::info(&format!("run record: {}", run_dir.display()));

    worktree::fetch(&ctx)?;

    let settings = Settings {
        model: opts.model.clone(),
        effort: opts.effort,
        agent_timeout: opts.agent_timeout,
        ci_timeout: opts.ci_timeout,
        ci_grace: opts.ci_grace,
        max_ci_retries: opts.max_ci_retries,
        draft: opts.draft,
        review: opts.review,
        worktree_root,
        run_dir: run_dir.clone(),
        keep_worktrees: opts.keep_worktrees,
    };

    let mut governor = Governor::new(Policy {
        wait_on_limit: opts.wait_on_limit,
        max_total_wait: opts.max_wait,
    });

    // ── work the queue ───────────────────────────────────────────────────────
    let mut tasks = Vec::new();
    for number in &sequenced.order {
        let Some(issue) = issues.iter().find(|i| i.number == *number) else {
            let mut task = report::TaskReport::new(
                *number,
                format!("issue #{number}"),
                issue_url(&ctx.slug, *number),
            );
            task.finish(
                report::TaskStatus::Skipped,
                "the issue could not be read from GitHub".to_string(),
            );
            tasks.push(task);
            continue;
        };

        let mut engine = Pipeline {
            ctx: &ctx,
            forge: &forge,
            governor: &mut governor,
            settings: &settings,
        };
        tasks.push(engine.run_issue(issue));
    }

    // ── report ───────────────────────────────────────────────────────────────
    let report = RunReport {
        repo: ctx.slug.to_string(),
        base_branch: ctx.base_branch.clone(),
        model: opts.model,
        effort: opts.effort.to_string(),
        started_at,
        finished_at: Local::now(),
        queue: sequenced.order.clone(),
        queue_warnings: warnings,
        tasks,
        total_cost_usd: governor.spend(),
        total_sessions: governor.sessions(),
        slept_secs: governor.slept().as_secs(),
        run_dir: run_dir.clone(),
    };

    if let Err(e) = report.persist(&run_dir) {
        ui::warn(&format!("could not write the run report: {e:#}"));
    }
    report.print();

    Ok(report.exit_code())
}

/// Read every requested issue, tolerating ones that cannot be read.
fn fetch_issues(forge: &Forge, numbers: &[u64]) -> Result<(Vec<Issue>, Vec<String>)> {
    ui::step(&format!("reading {} issue(s)", numbers.len()));
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    for number in numbers {
        match forge.issue(*number) {
            Ok(issue) => {
                ui::info(&format!(
                    "#{} {} [{}]",
                    issue.number,
                    ui::clip(&issue.title, 60),
                    issue.state.to_lowercase()
                ));
                issues.push(issue);
            }
            Err(e) => {
                ui::error(&format!("could not read issue #{number}: {e:#}"));
                warnings.push(format!("issue #{number} could not be read: {e}"));
            }
        }
    }
    Ok((issues, warnings))
}

/// Collect relationships from GitHub's issue dependencies where available, and
/// from the issue bodies either way.
fn gather_relations(forge: &Forge, issues: &[Issue]) -> Vec<deps::Relation> {
    let mut relations = Vec::new();
    let mut forge_available = false;

    for issue in issues {
        if let Some(blockers) = forge.blocked_by(issue.number) {
            forge_available = true;
            relations.extend(blockers.into_iter().map(|blocker| deps::Relation {
                blocked: issue.number,
                blocker,
                source: deps::RelationSource::Forge,
            }));
        }
        if let Some(blocked) = forge.blocking(issue.number) {
            forge_available = true;
            relations.extend(blocked.into_iter().map(|blocked| deps::Relation {
                blocked,
                blocker: issue.number,
                source: deps::RelationSource::Forge,
            }));
        }
        relations.extend(deps::extract_from_body(issue.number, &issue.body));
    }

    if !forge_available {
        ui::trace("GitHub issue dependencies are unavailable; relying on issue bodies");
    }

    relations.sort();
    relations.dedup_by(|a, b| a.blocked == b.blocked && a.blocker == b.blocker);
    relations
}

fn print_queue(sequenced: &deps::Sequenced, issues: &[Issue], relations: &[deps::Relation]) {
    ui::blank();
    ui::info(&ui::bold("Queue"));
    for (position, number) in sequenced.order.iter().enumerate() {
        let title = issues
            .iter()
            .find(|i| i.number == *number)
            .map(|i| ui::clip(&i.title, 56))
            .unwrap_or_else(|| "(unreadable)".to_string());
        ui::info(&format!("  {}. #{number} {title}", position + 1));

        if let Some(blockers) = sequenced.edges.get(number)
            && !blockers.is_empty()
        {
            let detail = blockers
                .iter()
                .map(|b| {
                    let source = relations
                        .iter()
                        .find(|r| r.blocked == *number && r.blocker == *b)
                        .map(|r| r.source.label())
                        .unwrap_or("relationship");
                    format!("#{b} ({source})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            ui::info(&format!("     {}", ui::dim(&format!("after {detail}"))));
        }
    }
}

fn issue_url(slug: &repo::RepoSlug, number: u64) -> String {
    format!("https://github.com/{slug}/issues/{number}")
}
