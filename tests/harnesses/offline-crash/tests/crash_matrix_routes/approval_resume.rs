//! Real-face approval recovery, including decisions after a killed owner.

use super::*;
#[path = "approval_resume/jsonl.rs"]
mod jsonl;
use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest};

const ANSWER: &str = "approval completed";

fn script() -> Value {
    json!({"version": 1, "tool_policy": "decision_required", "repeat_last_step": true,
    "steps": [
        {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
        {"output": {"text": ANSWER}}
    ]})
}

fn cut(fixture: &FaceFixture) -> TestResult<(String, Value)> {
    let mut agent = FaceAgent::start(fixture, &[])?;
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello")?;
    let session = hello["session_id"].as_str().ok_or("session id")?.to_owned();
    agent.send(&prompt_frame("perform the loopback effect"))?;
    let approval = agent.wait_for("approval", |record| record["kind"] == "approval")?;
    agent.sigkill()?;
    // The fixture's response cursor is process-local. The retained frame
    // owns the pending call; the next provider request should answer it,
    // not restart this test's scripted tool round.
    let mut resumed = script();
    resumed["steps"] = json!([{"output": {"text": ANSWER}}]);
    fixture.write_script(&resumed)?;
    Ok((session, approval))
}

fn evidence_count(fixture: &FaceFixture, kind: EvidenceKind) -> TestResult<usize> {
    Ok(read_evidence(&fixture.evidence)?
        .iter()
        .filter(|row| row.kind == kind)
        .count())
}

fn feed(fixture: &FaceFixture) -> TestResult<Vec<FeedFact>> {
    let key = fixture
        .command()
        .args(["version", "--state-key"])
        .output()?;
    if !key.status.success() {
        return Err("state key failed".into());
    }
    let directory = fixture
        .home
        .join("state")
        .join(String::from_utf8(key.stdout)?.trim());
    let database = directory.join("runtime.sqlite3");
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let sessions = connection
        .prepare("SELECT session_id FROM sessions")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut facts = Vec::new();
    for session in sessions {
        let mut cursor = FeedCursor::start(session.parse()?);
        loop {
            let FeedPage::Records { records, tail, .. } =
                AgentSessions::inspect_feed_sqlite(&database, FeedPageRequest::after(cursor))?
            else {
                return Err("fixture feed was pruned".into());
            };
            for record in records {
                cursor = FeedCursor::new(cursor.session_id(), record.header().seq);
                facts.extend_from_slice(record.facts());
            }
            if cursor.applied() == tail {
                break;
            }
        }
    }
    Ok(facts)
}

fn assert_same_approval(original: &Value, restored: &Value) {
    for field in ["call_id", "name", "expires_at_ms", "prompt"] {
        assert_eq!(
            restored[field], original[field],
            "restored approval changed {field}"
        );
    }
}

pub(super) fn assert_terminals(fixture: &FaceFixture, count: usize) -> TestResult {
    let facts = feed(fixture)?;
    let terminals = facts
        .iter()
        .filter_map(|fact| match fact {
            FeedFact::TurnTerminal { turn_id, .. } => Some(*turn_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), count, "{facts:?}");
    assert_eq!(
        terminals
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        count
    );
    let admitted = facts
        .iter()
        .filter_map(|fact| match fact {
            FeedFact::CommandAccepted {
                kind: plakous::sessions::feed::FeedCommandKind::Root { .. },
                turn_id: Some(turn),
                ..
            } => Some(*turn),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        admitted.len(),
        count,
        "reopen admitted another original turn"
    );
    assert_eq!(
        admitted.iter().collect::<std::collections::BTreeSet<_>>(),
        terminals.iter().collect::<std::collections::BTreeSet<_>>()
    );
    assert!(
        !fixture
            .session_log_kinds()?
            .iter()
            .any(|kind| kind == "turn.abandoned")
    );
    Ok(())
}

#[test]
fn jsonl_repeated_approval_sigkill_keeps_the_original_decision() -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let (session, original) = cut(&fixture)?;
    let mut again = FaceAgent::start(&fixture, &["--session", &session])?;
    let restored = again.wait_for("first restored approval", |row| row["kind"] == "approval")?;
    assert_same_approval(&original, &restored);
    again.sigkill()?;
    let mut again = FaceAgent::start(&fixture, &["--session", &session])?;
    let restored = again.wait_for("second restored approval", |row| row["kind"] == "approval")?;
    assert_same_approval(&original, &restored);
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        1
    );
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    again.send(&json!({"v":1,"kind":"interrupt"}))?;
    again.wait_for("cancelled terminal", |row| {
        row["kind"] == "turn_lifecycle" && row["phase"] == "cancelled"
    })?;
    again.send(&quit_frame())?;
    let (success, _, stderr) = again.finish()?;
    assert!(success, "{stderr}");
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        1
    );
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_terminals(&fixture, 1)
}

#[test]
fn run_denial_commits_before_a_continuation_cut_is_abandoned() -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let (session, _) = cut(&fixture)?;
    let mut continuation = script();
    continuation["steps"] =
        json!([{"gate":{"file":"release-denied-continuation"},"output":{"text":ANSWER}}]);
    fixture.write_script(&continuation)?;
    let mut command = fixture.command();
    command.args(["run", "--session", &session, "--", "continue after the cut"]);
    let mut runner = FaceAgent::start_command(command)?;
    let deadline = Instant::now() + FACE_WAIT;
    while evidence_count(&fixture, EvidenceKind::ProviderDispatched)? != 2 {
        if Instant::now() >= deadline {
            return Err("run never reached its continuation provider step".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    runner.sigkill()?;
    let mut again = face_reopen(&fixture, &session)?;
    again.send(&quit_frame())?;
    let (success, records, stderr) = again.finish()?;
    assert!(success, "{stderr}");
    assert!(!records.iter().any(|row| row["kind"] == "approval"));
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        2
    );
    let kinds = fixture.session_log_kinds()?;
    let resolved = kinds
        .iter()
        .position(|kind| kind == "effect.decision.resolved")
        .ok_or("decision resolution not logged")?;
    let abandoned = kinds
        .iter()
        .position(|kind| kind == "turn.abandoned")
        .ok_or("continuation abandonment not logged")?;
    assert!(resolved < abandoned, "{kinds:?}");
    assert_eq!(
        feed(&fixture)?
            .iter()
            .filter(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn jsonl_restored_approval_accepts_interrupt_after_hello() -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let (session, approval) = cut(&fixture)?;
    let mut agent = FaceAgent::start(&fixture, &["--session", &session])?;
    let restored = agent.wait_for("restored approval", |record| record["kind"] == "approval")?;
    assert_eq!(
        agent.records().first().map(|record| &record["kind"]),
        Some(&json!("hello"))
    );
    assert_same_approval(&approval, &restored);
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        1
    );
    agent.send(&json!({"v": 1, "kind": "interrupt"}))?;
    agent.wait_for("cancelled terminal", |record| {
        record["kind"] == "turn_lifecycle" && record["phase"] == "cancelled"
    })?;
    agent.send(&quit_frame())?;
    let (success, records, stderr) = agent.finish()?;
    assert!(success, "{stderr}");
    assert_eq!(
        records
            .iter()
            .filter(|record| record["kind"] == "turn_lifecycle" && record["phase"] != "admitted")
            .count(),
        1
    );
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        1
    );
    assert_terminals(&fixture, 1)
}

fn run(fixture: &FaceFixture, arguments: &[&str]) -> TestResult<std::process::Output> {
    let mut child = fixture
        .command()
        .arg("run")
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Only the first-run compatibility notice reads this answer. Effect
    // approvals must be denied without asking for any further stdin input.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"y\n");
    }
    let deadline = Instant::now() + FACE_WAIT;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("non-interactive approval hung".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(child.wait_with_output()?)
}

fn assert_denied(fixture: &FaceFixture, output: &std::process::Output, turns: usize) -> TestResult {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("approval accepted"), "{stderr}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("approval accepted"));
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("denied approval for loopback_effect: run is non-interactive"),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(ANSWER));
    assert_eq!(evidence_count(fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_terminals(fixture, turns)
}

#[test]
fn run_denies_new_approval_without_waiting_for_input() -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let output = run(
        &fixture,
        &[
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--composition",
            "direct",
            "--",
            "perform the loopback effect",
        ],
    )?;
    assert_denied(&fixture, &output, 1)?;
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        2
    );
    Ok(())
}

#[test]
fn run_session_denies_restored_approval_and_drives_the_requested_prompt() -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let (session, _) = cut(&fixture)?;
    let output = run(
        &fixture,
        &["--session", &session, "--", "continue after the cut"],
    )?;
    assert_denied(&fixture, &output, 2)?;
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        3
    );
    Ok(())
}
