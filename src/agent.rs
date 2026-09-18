//! Running Claude Code headlessly and understanding what came back.
//!
//! Each invocation is one `claude -p --output-format stream-json` process in a
//! worktree. The raw stream is written verbatim to a transcript file so a human
//! can audit any decision later, while a condensed view goes to the terminal so
//! an unattended run is watchable in real time.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

use crate::ui;
use crate::util;

/// Claude Code's reasoning effort setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much freedom an invocation gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Implements changes: may edit, commit and push its own branch.
    Author,
    /// Reads and reports: may run read-only commands but must not change files.
    Reviewer,
}

/// One planned Claude Code invocation.
pub struct AgentSpec {
    /// Short identifier used for the transcript filename and log lines.
    pub label: String,
    pub cwd: PathBuf,
    pub prompt: String,
    pub model: String,
    pub effort: Effort,
    pub timeout: Duration,
    pub role: Role,
    pub transcript: PathBuf,
}

/// Why a usage limit stopped us, and when we can try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLimit {
    pub kind: LimitKind,
    pub resets_at: Option<DateTime<Utc>>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// The rolling five-hour session window.
    Session,
    /// The fixed weekly cap.
    Weekly,
    Unknown,
}

impl LimitKind {
    pub fn label(self) -> &'static str {
        match self {
            LimitKind::Session => "5-hour session limit",
            LimitKind::Weekly => "weekly limit",
            LimitKind::Unknown => "usage limit",
        }
    }
}

/// Quota telemetry, read from the `rate_limit_event` messages Claude Code emits
/// on the stream.
///
/// This is the answer to "can usage be read programmatically?": it can, and it
/// is authoritative — both the window that is binding and when it resets come
/// from the same place the limit itself does, so rain never has to guess a
/// backoff from an error string.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Quota {
    /// `allowed` while there is headroom; anything else means we are cut off.
    pub status: String,
    /// `five_hour` or `seven_day` — which window the status refers to.
    pub limit_type: Option<String>,
    /// Fraction of the five-hour window consumed, 0.0–1.0.
    pub five_hour_used: Option<f64>,
    pub five_hour_resets_at: Option<DateTime<Utc>>,
    /// Fraction of the weekly window consumed, 0.0–1.0.
    pub seven_day_used: Option<f64>,
    pub seven_day_resets_at: Option<DateTime<Utc>>,
    /// The reset time attached to the event itself.
    pub resets_at: Option<DateTime<Utc>>,
}

impl Quota {
    /// Whether the account is currently cut off.
    pub fn is_exhausted(&self) -> bool {
        !self.status.is_empty() && !self.status.eq_ignore_ascii_case("allowed")
    }

    pub fn kind(&self) -> LimitKind {
        match self.limit_type.as_deref() {
            Some("seven_day") | Some("weekly") => LimitKind::Weekly,
            Some("five_hour") => LimitKind::Session,
            _ => LimitKind::Unknown,
        }
    }

    /// When the binding window resets, preferring the window the event names.
    pub fn reset_for_kind(&self) -> Option<DateTime<Utc>> {
        match self.kind() {
            LimitKind::Weekly => self.seven_day_resets_at.or(self.resets_at),
            LimitKind::Session => self.five_hour_resets_at.or(self.resets_at),
            LimitKind::Unknown => self.resets_at,
        }
    }

    /// One line for the run summary: what rain left behind for the human.
    pub fn summary_line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(used) = self.five_hour_used {
            parts.push(format!(
                "5-hour window {:.0}% used{}",
                used * 100.0,
                resets_suffix(self.five_hour_resets_at)
            ));
        }
        if let Some(used) = self.seven_day_used {
            parts.push(format!(
                "weekly {:.0}% used{}",
                used * 100.0,
                resets_suffix(self.seven_day_resets_at)
            ));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

fn resets_suffix(at: Option<DateTime<Utc>>) -> String {
    match at {
        Some(t) => format!(
            " (resets {})",
            t.with_timezone(&chrono::Local).format("%H:%M %a %e %b")
        ),
        None => String::new(),
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Success,
    /// The agent ran but reported an error, or hit its turn ceiling.
    Failed(String),
    /// The agent exceeded its wall-clock budget and was killed.
    Timeout,
    /// A subscription limit was hit; the caller should wait and retry.
    UsageLimit(UsageLimit),
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Success)
    }
}

/// What one invocation produced.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub label: String,
    pub outcome: Outcome,
    /// The agent's closing message — what it says it did.
    pub result_text: String,
    pub num_turns: u64,
    pub cost_usd: f64,
    pub duration: Duration,
    pub transcript: PathBuf,
    pub session_id: Option<String>,
    /// The most recent quota reading from this session, if one was reported.
    pub quota: Option<Quota>,
}

impl AgentRun {
    pub fn summary_line(&self) -> String {
        format!(
            "{} — {} turns, {}, notional cost ${:.4}",
            self.label,
            self.num_turns,
            util::format_duration(self.duration),
            self.cost_usd
        )
    }
}

/// Bash patterns the agent is never allowed to run, regardless of permission
/// mode. This is one of two independent guards; the other is rain checking
/// afterwards that the base branch did not move (see `pipeline`).
const FORBIDDEN: &[&str] = &[
    "Bash(git push --force:*)",
    "Bash(git push -f:*)",
    "Bash(git push --force-with-lease:*)",
    "Bash(git push --delete:*)",
    "Bash(git push origin --delete:*)",
    "Bash(git tag:*)",
    "Bash(git remote:*)",
    "Bash(git filter-branch:*)",
    "Bash(git config --global:*)",
    "Bash(gh pr merge:*)",
    "Bash(gh release:*)",
    "Bash(gh repo delete:*)",
    "Bash(gh workflow disable:*)",
    "Bash(gh api --method DELETE:*)",
];

/// Additionally denied to reviewers, which must not change anything.
const REVIEWER_FORBIDDEN: &[&str] = &[
    "Edit",
    "Write",
    "NotebookEdit",
    "Bash(git commit:*)",
    "Bash(git push:*)",
    "Bash(git add:*)",
    "Bash(gh pr create:*)",
];

/// Run one Claude Code session to completion, a timeout, or a usage limit.
pub fn run(spec: &AgentSpec) -> Result<AgentRun> {
    if let Some(parent) = spec.transcript.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating transcript directory {}", parent.display()))?;
    }

    let mut child = spawn(spec)?;
    let started = Instant::now();

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let (tx, rx): (Sender<Option<String>>, Receiver<Option<String>>) = channel();
    let transcript_path = spec.transcript.clone();
    let stdout_thread = std::thread::spawn(move || -> Result<()> {
        let mut file = File::create(&transcript_path)
            .with_context(|| format!("creating {}", transcript_path.display()))?;
        for line in BufReader::new(stdout).lines() {
            let line = line?;
            writeln!(file, "{line}")?;
            if tx.send(Some(line)).is_err() {
                break;
            }
        }
        let _ = file.flush();
        let _ = tx.send(None);
        Ok(())
    });

    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_buf);
    let stderr_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(mut buf) = stderr_sink.lock() {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
    });

    let mut state = StreamState::default();
    let deadline = started + spec.timeout;
    let mut timed_out = false;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(Some(line)) => state.absorb(&line),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                timed_out = true;
                break;
            }
        }
    }

    if timed_out {
        ui::warn(&format!(
            "{} exceeded its {} budget — stopping the session",
            spec.label,
            util::format_duration(spec.timeout)
        ));
        let _ = child.kill();
    }

    let status = child.wait().context("waiting for the claude process")?;
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let stderr_text = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
    let duration = started.elapsed();

    let outcome = classify(timed_out, status.code().unwrap_or(-1), &state, &stderr_text);

    Ok(AgentRun {
        label: spec.label.clone(),
        outcome,
        result_text: state.result_text.clone(),
        num_turns: state.num_turns,
        cost_usd: state.cost_usd,
        duration,
        transcript: spec.transcript.clone(),
        session_id: state.session_id.clone(),
        quota: state.quota.clone(),
    })
}

fn spawn(spec: &AgentSpec) -> Result<Child> {
    let mut cmd = Command::new("claude");
    cmd.current_dir(&spec.cwd)
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--disallowed-tools");

    for pattern in FORBIDDEN {
        cmd.arg(pattern);
    }
    if spec.role == Role::Reviewer {
        for pattern in REVIEWER_FORBIDDEN {
            cmd.arg(pattern);
        }
    }

    // The variadic list above ends at the next flag, so every remaining option
    // must be flag-shaped and the prompt must come last.
    cmd.arg("--permission-mode")
        .arg("bypassPermissions")
        .arg("--model")
        .arg(&spec.model)
        .arg("--effort")
        .arg(spec.effort.as_str())
        .arg("--append-system-prompt")
        .arg(crate::prompts::GUARDRAILS)
        .arg(&spec.prompt);

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    ui::info(&format!(
        "claude ({}, effort {}) in {}",
        spec.model,
        spec.effort,
        spec.cwd.display()
    ));

    cmd.spawn()
        .map_err(|e| anyhow!("failed to start claude (is Claude Code installed and on PATH?): {e}"))
}

/// Everything we learn from the stream as it arrives.
#[derive(Default)]
struct StreamState {
    result_text: String,
    result_subtype: String,
    is_error: bool,
    saw_result: bool,
    num_turns: u64,
    cost_usd: f64,
    session_id: Option<String>,
    quota: Option<Quota>,
}

impl StreamState {
    fn absorb(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // Non-JSON output on stdout is unexpected but occasionally happens
            // (early startup warnings); surface it rather than dropping it.
            if !line.trim().is_empty() {
                ui::trace(&format!("non-JSON stream line: {}", ui::clip(line, 160)));
            }
            return;
        };

        match value.get("type").and_then(Value::as_str) {
            Some("system") => {
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_string());
                }
                if value.get("subtype").and_then(Value::as_str) == Some("init")
                    && let Some(model) = value.get("model").and_then(Value::as_str)
                {
                    ui::trace(&format!("session started on {model}"));
                }
            }
            Some("assistant") => self.report_assistant(&value),
            Some("rate_limit_event") => {
                if let Some(quota) = parse_quota(&value) {
                    if quota.is_exhausted() {
                        ui::warn(&format!("quota status: {}", quota.status));
                    } else if let Some(line) = quota.summary_line() {
                        ui::trace(&format!("quota: {line}"));
                    }
                    self.quota = Some(quota);
                }
            }
            Some("result") => {
                self.saw_result = true;
                self.result_subtype = value
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.is_error = value
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.num_turns = value.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
                self.cost_usd = value
                    .get("total_cost_usd")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                if let Some(text) = value.get("result").and_then(Value::as_str) {
                    self.result_text = text.to_string();
                }
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_string());
                }
            }
            _ => {}
        }
    }

    fn report_assistant(&self, value: &Value) {
        let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if !text.trim().is_empty() {
                        ui::agent(&ui::clip(text, 160));
                    }
                }
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let brief = tool_brief(name, block.get("input"));
                    ui::agent(&ui::dim(&format!("{name}: {brief}")));
                }
                Some("thinking") => ui::trace("thinking"),
                _ => {}
            }
        }
    }
}

/// A one-line gist of a tool call, chosen per tool so the live log is readable.
fn tool_brief(name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let field = match name {
        "Bash" => "command",
        "Read" | "Edit" | "Write" | "NotebookEdit" => "file_path",
        "Grep" => "pattern",
        "Glob" => "pattern",
        "Task" | "Agent" => "description",
        "WebFetch" => "url",
        _ => "",
    };
    if !field.is_empty()
        && let Some(v) = input.get(field).and_then(Value::as_str)
    {
        return ui::clip(v, 120);
    }
    ui::clip(&input.to_string(), 100)
}

fn classify(timed_out: bool, exit_code: i32, state: &StreamState, stderr: &str) -> Outcome {
    if timed_out {
        return Outcome::Timeout;
    }

    let failed = state.is_error || exit_code != 0 || !state.saw_result;

    // The stream's own quota telemetry is authoritative when it says we are cut
    // off: it names the binding window and its reset time, so no backoff has to
    // be guessed. Only trust it to end a run that actually failed — a session
    // can report a limit on its closing event having already done its work.
    if failed && let Some(quota) = state.quota.as_ref().filter(|q| q.is_exhausted()) {
        return Outcome::UsageLimit(UsageLimit {
            kind: quota.kind(),
            resets_at: quota.reset_for_kind(),
            message: format!("Claude Code reported quota status `{}`", quota.status),
        });
    }

    // Otherwise fall back to the wording. Only trust limit wording from rain's
    // own channels — the process's stderr, or the agent's closing message when
    // the run failed. A successful run whose text happens to discuss usage
    // limits is not a usage limit.
    let mut haystack = stderr.to_string();
    if failed {
        haystack.push('\n');
        haystack.push_str(&state.result_text);
    }
    if let Some(mut limit) = detect_usage_limit(&haystack) {
        // Prefer a reset time the stream gave us over one scraped from text.
        if limit.resets_at.is_none()
            && let Some(quota) = &state.quota
        {
            limit.resets_at = quota.reset_for_kind();
            if limit.kind == LimitKind::Unknown {
                limit.kind = quota.kind();
            }
        }
        return Outcome::UsageLimit(limit);
    }

    if !failed {
        return Outcome::Success;
    }

    let reason = if state.result_subtype == "error_max_turns" {
        "the agent hit its maximum turn count without finishing".to_string()
    } else if !state.result_text.trim().is_empty() {
        ui::clip(&state.result_text, 400)
    } else if !stderr.trim().is_empty() {
        ui::clip(stderr, 400)
    } else if !state.saw_result {
        format!("the session ended without a result block (exit {exit_code})")
    } else {
        format!("the session failed (exit {exit_code})")
    };
    Outcome::Failed(reason)
}

/// Read a `rate_limit_event` message into a [`Quota`].
fn parse_quota(value: &Value) -> Option<Quota> {
    let info = value.get("rate_limit_info")?;
    let windows = info.get("unifiedWindows");
    let window = |name: &str, field: &str| -> Option<&Value> {
        windows.and_then(|w| w.get(name)).and_then(|w| w.get(field))
    };

    Some(Quota {
        status: info
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        limit_type: info
            .get("rateLimitType")
            .and_then(Value::as_str)
            .map(str::to_string),
        five_hour_used: window("five_hour", "utilization").and_then(Value::as_f64),
        five_hour_resets_at: window("five_hour", "resetsAt").and_then(epoch_to_utc),
        seven_day_used: window("seven_day", "utilization").and_then(Value::as_f64),
        seven_day_resets_at: window("seven_day", "resetsAt").and_then(epoch_to_utc),
        resets_at: info.get("resetsAt").and_then(epoch_to_utc),
    })
}

/// Accept epoch seconds or milliseconds, as a number or a numeric string.
fn epoch_to_utc(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))?;
    if raw <= 0 {
        return None;
    }
    let secs = if raw > 100_000_000_000 {
        raw / 1000
    } else {
        raw
    };
    Utc.timestamp_opt(secs, 0).single()
}

fn limit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(claude\s+ai\s+usage\s+limit\s+reached|usage\s+limit\s+reached|you'?ve\s+reached\s+your\s+(?:usage\s+)?limit|(?:5|five)[-\s]hour\s+limit\s+reached|weekly\s+limit\s+reached|approaching\s+your\s+weekly\s+limit)",
        )
        .expect("static regex")
    })
}

fn epoch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"limit reached\|(\d{9,13})").expect("static regex"))
}

/// Recognise a subscription limit in Claude Code's error output, and pull out
/// the reset time when it tells us one.
pub fn detect_usage_limit(text: &str) -> Option<UsageLimit> {
    let m = limit_re().find(text)?;
    let lower = text.to_lowercase();

    let kind = if lower.contains("weekly") {
        LimitKind::Weekly
    } else if lower.contains("5-hour")
        || lower.contains("5 hour")
        || lower.contains("five-hour")
        || lower.contains("session limit")
    {
        LimitKind::Session
    } else {
        LimitKind::Unknown
    };

    // Claude Code emits `Claude AI usage limit reached|<epoch>`.
    let resets_at = epoch_re().captures(text).and_then(|c| {
        let raw: i64 = c.get(1)?.as_str().parse().ok()?;
        // Values of 13 digits are milliseconds.
        let secs = if raw > 100_000_000_000 {
            raw / 1000
        } else {
            raw
        };
        Utc.timestamp_opt(secs, 0).single()
    });

    Some(UsageLimit {
        kind,
        resets_at,
        message: ui::clip(&text[m.start()..], 200),
    })
}

/// Where a task's transcripts live.
pub fn transcript_path(task_dir: &Path, label: &str) -> PathBuf {
    task_dir.join(format!("{}.jsonl", util::slugify(label)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_limit_with_epoch() {
        let limit = detect_usage_limit("Claude AI usage limit reached|1780000000").unwrap();
        assert_eq!(limit.resets_at.unwrap().timestamp(), 1_780_000_000);
    }

    #[test]
    fn detects_millisecond_epoch() {
        let limit = detect_usage_limit("Claude AI usage limit reached|1780000000000").unwrap();
        assert_eq!(limit.resets_at.unwrap().timestamp(), 1_780_000_000);
    }

    #[test]
    fn distinguishes_weekly_from_session() {
        assert_eq!(
            detect_usage_limit("Weekly limit reached. Resets Monday.")
                .unwrap()
                .kind,
            LimitKind::Weekly
        );
        assert_eq!(
            detect_usage_limit("5-hour limit reached").unwrap().kind,
            LimitKind::Session
        );
        assert_eq!(
            detect_usage_limit("usage limit reached").unwrap().kind,
            LimitKind::Unknown
        );
    }

    #[test]
    fn ignores_ordinary_text() {
        assert!(detect_usage_limit("all tests passed").is_none());
        assert!(detect_usage_limit("we should document the usage limits").is_none());
    }

    #[test]
    fn successful_run_discussing_limits_is_not_a_limit() {
        let mut state = StreamState::default();
        state.absorb(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":4,"total_cost_usd":0.5,"result":"Documented what happens when the usage limit reached message appears."}"#,
        );
        assert!(matches!(classify(false, 0, &state, ""), Outcome::Success));
    }

    #[test]
    fn failed_run_mentioning_a_limit_is_a_limit() {
        let mut state = StreamState::default();
        state.absorb(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1,"result":"Claude AI usage limit reached|1780000000"}"#,
        );
        assert!(matches!(
            classify(false, 1, &state, ""),
            Outcome::UsageLimit(_)
        ));
    }

    #[test]
    fn parses_a_result_block() {
        let mut state = StreamState::default();
        state.absorb(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1000,"num_turns":7,"total_cost_usd":1.25,"result":"done","session_id":"abc"}"#,
        );
        assert_eq!(state.num_turns, 7);
        assert!((state.cost_usd - 1.25).abs() < f64::EPSILON);
        assert_eq!(state.result_text, "done");
        assert_eq!(state.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn missing_result_block_is_a_failure() {
        let state = StreamState::default();
        assert!(matches!(classify(false, 0, &state, ""), Outcome::Failed(_)));
    }

    #[test]
    fn timeout_wins_over_everything() {
        let state = StreamState::default();
        assert!(matches!(classify(true, 0, &state, ""), Outcome::Timeout));
    }

    /// The shape Claude Code actually emits, captured from a live session.
    const RATE_LIMIT_EVENT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1789736400,"rateLimitType":"five_hour","overageStatus":"allowed","overageResetsAt":1790812800,"isUsingOverage":false,"unifiedWindows":{"five_hour":{"utilization":0.14,"resetsAt":1789736400},"seven_day":{"utilization":0.16,"resetsAt":1790193600}}},"uuid":"6d0af5e7","session_id":"50b6903f"}"#;

    fn quota_from(json: &str) -> Quota {
        parse_quota(&serde_json::from_str::<Value>(json).unwrap()).unwrap()
    }

    #[test]
    fn reads_quota_telemetry_from_the_stream() {
        let quota = quota_from(RATE_LIMIT_EVENT);
        assert_eq!(quota.status, "allowed");
        assert!(!quota.is_exhausted());
        assert_eq!(quota.kind(), LimitKind::Session);
        assert_eq!(quota.five_hour_used, Some(0.14));
        assert_eq!(quota.seven_day_used, Some(0.16));
        assert_eq!(
            quota.five_hour_resets_at.unwrap().timestamp(),
            1_789_736_400
        );
        assert_eq!(
            quota.seven_day_resets_at.unwrap().timestamp(),
            1_790_193_600
        );
        assert_eq!(quota.reset_for_kind(), quota.five_hour_resets_at);
    }

    #[test]
    fn summarises_both_quota_windows() {
        let line = quota_from(RATE_LIMIT_EVENT).summary_line().unwrap();
        assert!(line.contains("5-hour window 14% used"), "got: {line}");
        assert!(line.contains("weekly 16% used"), "got: {line}");
    }

    #[test]
    fn a_weekly_exhaustion_reads_the_weekly_reset() {
        let quota = quota_from(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"seven_day","unifiedWindows":{"five_hour":{"utilization":0.2,"resetsAt":1789736400},"seven_day":{"utilization":1.0,"resetsAt":1790193600}}}}"#,
        );
        assert!(quota.is_exhausted());
        assert_eq!(quota.kind(), LimitKind::Weekly);
        assert_eq!(quota.reset_for_kind().unwrap().timestamp(), 1_790_193_600);
    }

    #[test]
    fn quota_telemetry_drives_the_limit_verdict() {
        let mut state = StreamState::default();
        state.absorb(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","resetsAt":1789736400,"unifiedWindows":{"five_hour":{"utilization":1.0,"resetsAt":1789736400}}}}"#,
        );
        state.absorb(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1,"result":"stopped"}"#,
        );

        match classify(false, 1, &state, "") {
            Outcome::UsageLimit(limit) => {
                assert_eq!(limit.kind, LimitKind::Session);
                assert_eq!(limit.resets_at.unwrap().timestamp(), 1_789_736_400);
            }
            other => panic!("expected a usage limit, got {other:?}"),
        }
    }

    #[test]
    fn an_allowed_quota_reading_never_stops_a_run() {
        let mut state = StreamState::default();
        state.absorb(RATE_LIMIT_EVENT);
        state.absorb(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"done"}"#,
        );
        assert!(matches!(classify(false, 0, &state, ""), Outcome::Success));
        assert_eq!(state.quota.as_ref().unwrap().five_hour_used, Some(0.14));
    }

    #[test]
    fn a_failure_with_an_allowed_quota_is_an_ordinary_failure() {
        let mut state = StreamState::default();
        state.absorb(RATE_LIMIT_EVENT);
        state.absorb(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1,"result":"the build broke"}"#,
        );
        assert!(matches!(classify(false, 1, &state, ""), Outcome::Failed(_)));
    }

    #[test]
    fn quota_fills_in_a_reset_time_the_text_lacks() {
        let mut state = StreamState::default();
        state.absorb(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.99,"resetsAt":1789736400}}}}"#,
        );
        state.absorb(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"usage limit reached"}"#,
        );
        match classify(false, 1, &state, "") {
            Outcome::UsageLimit(limit) => {
                assert_eq!(limit.resets_at.unwrap().timestamp(), 1_789_736_400)
            }
            other => panic!("expected a usage limit, got {other:?}"),
        }
    }

    #[test]
    fn ignores_a_rate_limit_event_without_payload() {
        assert!(parse_quota(&serde_json::json!({"type": "rate_limit_event"})).is_none());
    }

    #[test]
    fn summarises_a_bash_tool_call() {
        let input = serde_json::json!({"command": "cargo test --all"});
        assert_eq!(tool_brief("Bash", Some(&input)), "cargo test --all");
    }
}
