//! Drives a real Claude Code session through the agent runner.
//!
//! Ignored by default: it needs a working Claude Code login and it spends
//! quota. Run it deliberately after changing anything about how rain spawns or
//! parses a session:
//!
//! ```text
//! cargo test --test agent_smoke -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use rain::agent::{self, AgentSpec, Effort, Outcome, Role};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rain-agent-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The model these smoke tests run on. Deliberately not rain's default: the
/// point is to exercise the plumbing, not to spend Opus quota on it.
const SMOKE_MODEL: &str = "haiku";

#[test]
#[ignore = "spends quota and needs a Claude Code login"]
fn runs_a_session_and_parses_the_result() {
    let dir = scratch("result");
    let spec = AgentSpec {
        label: "smoke".into(),
        cwd: dir.clone(),
        prompt: "Reply with exactly the word PONG. Do not use any tools.".into(),
        model: SMOKE_MODEL.into(),
        effort: Effort::Low,
        timeout: Duration::from_secs(180),
        role: Role::Author,
        transcript: dir.join("smoke.jsonl"),
    };

    let run = agent::run(&spec).expect("the session should start");

    assert!(
        matches!(run.outcome, Outcome::Success),
        "expected success, got {:?}",
        run.outcome
    );
    assert!(
        run.result_text.contains("PONG"),
        "got {:?}",
        run.result_text
    );
    assert!(run.num_turns >= 1, "turns should be counted");
    assert!(run.cost_usd > 0.0, "notional cost should be recorded");
    assert!(run.session_id.is_some(), "session id should be captured");

    let transcript = std::fs::read_to_string(&run.transcript).expect("a transcript was written");
    assert!(
        transcript
            .lines()
            .any(|l| l.contains("\"type\":\"result\"")),
        "the transcript should contain the raw result block"
    );

    // The quota telemetry rain's limit handling depends on.
    let quota = run.quota.expect("a rate_limit_event should have arrived");
    assert!(!quota.status.is_empty(), "quota status should be populated");
    assert!(
        quota.five_hour_used.is_some() || quota.seven_day_used.is_some(),
        "at least one window's utilization should be reported: {quota:?}"
    );
    println!("quota: {:?}", quota.summary_line());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "spends quota and needs a Claude Code login"]
fn a_session_can_use_tools_and_commit() {
    let dir = scratch("tools");
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .arg(&dir)
        .output()
        .unwrap();
    for (k, v) in [
        ("user.email", "rain@example.invalid"),
        ("user.name", "rain"),
    ] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", k, v])
            .output()
            .unwrap();
    }

    let spec = AgentSpec {
        label: "smoke-tools".into(),
        cwd: dir.clone(),
        prompt: "Create a file named hello.txt containing the single line `hi`, \
                 then stage and commit it with the message `feat: add hello`. \
                 Do not push. Reply DONE when finished."
            .into(),
        model: SMOKE_MODEL.into(),
        effort: Effort::Low,
        timeout: Duration::from_secs(300),
        role: Role::Author,
        transcript: dir.join("tools.jsonl"),
    };

    let run = agent::run(&spec).expect("the session should start");
    assert!(
        matches!(run.outcome, Outcome::Success),
        "expected success, got {:?}",
        run.outcome
    );

    assert_eq!(
        std::fs::read_to_string(dir.join("hello.txt"))
            .expect("the agent should have created the file")
            .trim(),
        "hi"
    );

    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("add hello"), "expected a commit, got: {log}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "spends quota and needs a Claude Code login"]
fn a_timeout_is_bounded_and_reported() {
    let dir = scratch("timeout");
    let spec = AgentSpec {
        label: "smoke-timeout".into(),
        cwd: dir.clone(),
        prompt: "Count from 1 to 20, one number per line.".into(),
        model: SMOKE_MODEL.into(),
        effort: Effort::Low,
        // Shorter than a session takes to start, so the runner always has to cut
        // it off. Asking the agent to do something slow instead would leave the
        // test at the mercy of how it chooses to do the work — an earlier
        // version asked for `sleep 120` and the agent backgrounded it and
        // finished immediately.
        timeout: Duration::from_secs(2),
        role: Role::Author,
        transcript: dir.join("timeout.jsonl"),
    };

    let started = std::time::Instant::now();
    let run = agent::run(&spec).expect("the session should start");
    let elapsed = started.elapsed();

    assert!(
        matches!(run.outcome, Outcome::Timeout),
        "expected a timeout, got {:?}",
        run.outcome
    );
    // The point of the test: killing the session must not leave the runner
    // waiting on a pipe a surviving child still holds open.
    assert!(
        elapsed < Duration::from_secs(30),
        "the runner should have returned promptly after the kill, took {elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
