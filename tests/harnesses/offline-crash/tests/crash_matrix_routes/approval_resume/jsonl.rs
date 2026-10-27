//! Inbound single-use decisions through the real JSONL face.

use super::*;

fn resolve(id: &str, approval: &Value, choice: &str) -> Value {
    json!({"v":1,"kind":"resolve_approval","id":id,
        "approval_id":approval["approval_id"],"choice":choice})
}

fn accepted(agent: &FaceAgent, id: &str, choice: &str) -> TestResult {
    let reply = agent.wait_for("approval acceptance", |row| {
        row["id"] == id
            && (row["kind"] == "error" || (row["kind"] == "status" && row["phase"] == "note"))
    })?;
    assert_eq!(
        reply,
        json!({"v":1,"kind":"status","id":id,"phase":"note",
        "queue_len":0,"detail":format!("approval accepted: {choice}")})
    );
    Ok(())
}

fn refused(agent: &mut FaceAgent, command: &Value, message: &str) -> TestResult {
    agent.send(command)?;
    let reply = agent.wait_for("approval refusal", |row| {
        row["kind"] == "error" && row["id"] == command["id"]
    })?;
    assert_eq!(reply["message"], message);
    Ok(())
}

const UNAVAILABLE: &str = "approval refused: no pending decision with that identity";

fn finish(agent: &mut FaceAgent) -> TestResult {
    agent.wait_for("one completed terminal", |row| {
        row["kind"] == "turn_lifecycle" && row["phase"] == "completed"
    })?;
    agent.send(&quit_frame())
}

fn completes(choice: &str, restored: bool) -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let (mut agent, original) = if restored {
        let (session, approval) = cut(&fixture)?;
        (
            FaceAgent::start(&fixture, &["--session", &session])?,
            Some(approval),
        )
    } else {
        let mut agent = FaceAgent::start(&fixture, &[])?;
        agent.wait_for("hello", |row| row["kind"] == "hello")?;
        agent.send(&prompt_frame("decide this effect"))?;
        (agent, None)
    };
    let approval = agent.wait_for("offered approval", |row| row["kind"] == "approval")?;
    assert_eq!(agent.records()[0]["kind"], "hello");
    if let Some(original) = original {
        assert_same_approval(&original, &approval);
        refused(
            &mut agent,
            &resolve("old-attachment", &original, choice),
            UNAVAILABLE,
        )?;
    }
    agent.send(&resolve("decision", &approval, choice))?;
    accepted(&agent, "decision", choice)?;
    let resolved = agent.wait_for("resolution", |row| row["kind"] == "approval_resolved")?;
    assert_eq!(resolved["approval_id"], approval["approval_id"]);
    assert_eq!(
        resolved["resolution"],
        if choice == "allow" {
            "allowed"
        } else {
            "denied"
        }
    );
    // Resolution remains turn-scoped, never stolen by the decision command.
    assert_eq!(resolved.get("id"), approval.get("id"));
    finish(&mut agent)?;
    let (success, records, stderr) = agent.finish()?;
    assert!(success, "{stderr}");
    assert_eq!(
        records
            .iter()
            .filter(|row| row["kind"] == "approval")
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|row| row["kind"] == "turn_lifecycle" && row["phase"] != "admitted")
            .count(),
        1
    );
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::EffectDispatched)?,
        usize::from(choice == "allow")
    );
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        2
    );
    assert_terminals(&fixture, 1)
}

#[test]
fn live_allow_dispatches_once() -> TestResult {
    completes("allow", false)
}
#[test]
fn live_deny_never_dispatches() -> TestResult {
    completes("deny", false)
}
#[test]
fn restored_allow_dispatches_once() -> TestResult {
    completes("allow", true)
}
#[test]
fn restored_deny_never_dispatches() -> TestResult {
    completes("deny", true)
}

#[test]
fn refusals_leave_the_live_decision_pending_and_unknown_kinds_nonfatal() -> TestResult {
    let fixture = FaceFixture::new(&script())?;
    let foreign = FaceFixture::new(&script())?;
    let mut other = FaceAgent::start(&foreign, &[])?;
    other.wait_for("foreign hello", |row| row["kind"] == "hello")?;
    other.send(&prompt_frame("foreign decision"))?;
    let foreign_approval = other.wait_for("foreign approval", |row| row["kind"] == "approval")?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    agent.wait_for("hello", |row| row["kind"] == "hello")?;
    agent.send(&prompt_frame("local decision"))?;
    let approval = agent.wait_for("approval", |row| row["kind"] == "approval")?;
    let before_refusals = feed(&fixture)?.len();
    refused(
        &mut agent,
        &resolve("unknown", &json!({"approval_id":"not-offered"}), "allow"),
        UNAVAILABLE,
    )?;
    refused(
        &mut agent,
        &resolve("foreign-session", &foreign_approval, "allow"),
        UNAVAILABLE,
    )?;
    for (index, choice) in [
        Value::Null,
        json!(true),
        json!("ALLOW"),
        json!("yes"),
        json!({}),
    ]
    .iter()
    .enumerate()
    {
        let mut command = resolve(&format!("malformed-{index}"), &approval, "allow");
        command["choice"] = choice.clone();
        refused(
            &mut agent,
            &command,
            "resolve_approval choice must be allow or deny",
        )?;
    }
    for (index, handle) in [Value::Null, json!(""), json!(7)].iter().enumerate() {
        let mut command = resolve(&format!("handle-{index}"), &approval, "allow");
        command["approval_id"] = handle.clone();
        refused(
            &mut agent,
            &command,
            "resolve_approval requires a non-empty approval_id from an approval event",
        )?;
    }
    for (field, message) in [
        ("choice", "resolve_approval choice must be allow or deny"),
        (
            "approval_id",
            "resolve_approval requires a non-empty approval_id from an approval event",
        ),
    ] {
        let mut command = resolve(&format!("missing-{field}"), &approval, "allow");
        command
            .as_object_mut()
            .ok_or("command object")?
            .remove(field);
        refused(&mut agent, &command, message)?;
    }
    for kind in ["resolve_approval_typo", "future_command", ""] {
        refused(
            &mut agent,
            &json!({"v":1,"kind":kind,"id":format!("unknown-{kind}")}),
            &format!("unknown JSONL kind `{kind}`"),
        )?;
    }
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        1
    );
    assert_eq!(
        feed(&fixture)?.len(),
        before_refusals,
        "refusal reached the runtime"
    );
    assert!(
        !agent
            .records()
            .iter()
            .any(|row| row["kind"] == "approval_resolved")
    );
    agent.send(&resolve("valid-after-refusals", &approval, "deny"))?;
    accepted(&agent, "valid-after-refusals", "deny")?;
    finish(&mut agent)?;
    assert!(agent.finish()?.0);
    other.send(&json!({"v":1,"kind":"interrupt"}))?;
    other.send(&quit_frame())?;
    assert!(other.finish()?.0);
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_eq!(evidence_count(&foreign, EvidenceKind::EffectDispatched)?, 0);
    assert_terminals(&fixture, 1)
}

#[test]
fn duplicate_and_previous_turn_handles_cannot_authorize_again() -> TestResult {
    let mut plan = script();
    plan["steps"][1]["gate"] = json!({"file":"continue"});
    let second_tool = plan["steps"][0].clone();
    let final_step = json!({"output":{"text":ANSWER}});
    plan["steps"]
        .as_array_mut()
        .ok_or("steps")?
        .extend([second_tool, final_step]);
    let fixture = FaceFixture::new(&plan)?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    agent.wait_for("hello", |row| row["kind"] == "hello")?;
    agent.send(&prompt_frame("first decision"))?;
    let approval = agent.wait_for("approval", |row| row["kind"] == "approval")?;
    let command = resolve("single-use", &approval, "allow");
    agent.send(&command)?;
    accepted(&agent, "single-use", "allow")?;
    refused(&mut agent, &command, UNAVAILABLE)?;
    assert_eq!(
        agent
            .records()
            .iter()
            .filter(|row| row["id"] == "single-use" && row["phase"] == "note")
            .count(),
        1
    );
    std::fs::write(fixture.workspace.join("continue"), "release")?;
    agent.wait_for("first terminal", |row| {
        row["kind"] == "turn_lifecycle" && row["phase"] == "completed"
    })?;
    refused(
        &mut agent,
        &resolve("retired-idle", &approval, "allow"),
        UNAVAILABLE,
    )?;
    agent.send(&json!({"v":1,"kind":"prompt","id":"next-turn","text":"another turn"}))?;
    agent.wait_for("next admission", |row| {
        row["kind"] == "turn_lifecycle" && row["id"] == "next-turn" && row["phase"] == "admitted"
    })?;
    let next = agent.wait_for("next approval", |row| {
        row["kind"] == "approval" && row["id"] == "next-turn"
    })?;
    assert_ne!(next["call_id"], approval["call_id"]);
    let before_refusal = feed(&fixture)?.len();
    refused(
        &mut agent,
        &resolve("foreign-turn", &approval, "allow"),
        UNAVAILABLE,
    )?;
    assert_eq!(feed(&fixture)?.len(), before_refusal);
    agent.send(&resolve("next-decision", &next, "deny"))?;
    accepted(&agent, "next-decision", "deny")?;
    agent.wait_for("next terminal", |row| {
        row["kind"] == "turn_lifecycle" && row["id"] == "next-turn" && row["phase"] == "completed"
    })?;
    agent.send(&quit_frame())?;
    assert!(agent.finish()?.0);
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 1);
    assert_terminals(&fixture, 2)
}

#[test]
fn expired_handle_is_refused_without_dispatch() -> TestResult {
    let mut plan = script();
    plan["effect_decision_timeout_ms"] = json!(300);
    plan["steps"][1]["gate"] = json!({"file":"after-expiry"});
    let fixture = FaceFixture::new(&plan)?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    agent.wait_for("hello", |row| row["kind"] == "hello")?;
    agent.send(&prompt_frame("expire this decision"))?;
    let approval = agent.wait_for("approval", |row| row["kind"] == "approval")?;
    agent.wait_for("expiry", |row| {
        row["kind"] == "approval_resolved" && row["resolution"] == "expired"
    })?;
    refused(
        &mut agent,
        &resolve("expired", &approval, "allow"),
        UNAVAILABLE,
    )?;
    std::fs::write(fixture.workspace.join("after-expiry"), "release")?;
    finish(&mut agent)?;
    assert!(agent.finish()?.0);
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_terminals(&fixture, 1)
}

fn trace_runtime_ordinal(path: &Path) -> TestResult<u64> {
    std::fs::read_to_string(path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .filter(|row| row["store"] == "runtime" && row["indexed"] == true)
        .filter_map(|row| row["store_ordinal"].as_u64())
        .max()
        .ok_or_else(|| "missing runtime trace ordinal".into())
}

fn assert_decision_committed_before_dispatch(fixture: &FaceFixture) -> TestResult {
    use plakous_agent::internal::{DurableOperationKind, DurableOperationState, SessionRef};
    let key = fixture
        .command()
        .args(["version", "--state-key"])
        .output()?;
    assert!(key.status.success());
    let database = fixture
        .home
        .join("state")
        .join(String::from_utf8(key.stdout)?.trim())
        .join("runtime.sqlite3");
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (session, branch): (String, String) =
        connection.query_row("SELECT session_id, branch_id FROM branches", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let loaded = plakous_store::SqliteSessionStore::inspect_branch(
        &database,
        SessionRef::new(session.parse()?, branch.parse()?),
    )?;
    let operations = loaded
        .checkpoint()
        .checkpoint()
        .manifest()
        .ledger()
        .operations();
    assert!(
        operations.iter().any(|operation| {
            matches!(
                operation.kind(),
                DurableOperationKind::EffectDecision { .. }
            ) && matches!(operation.state(), DurableOperationState::Finalized(evidence)
                if String::from_utf8_lossy(evidence.bytes()).contains("Allowed"))
        }),
        "cut must follow durable decision settlement: {operations:?}"
    );
    assert_eq!(evidence_count(fixture, EvidenceKind::EffectDispatched)?, 0);
    Ok(())
}

#[test]
fn accepted_decision_sigkill_before_effect_never_reoffers_or_redispatches() -> TestResult {
    use plakous_offline_crash_harness::commit_trace::{KILL_ENV, TRACE_ENV};
    // Calibrate only the reopen prefix. No fixed commit ordinal depends on
    // unrelated startup writes; the killed run verifies that prefix and the
    // durable decision evidence at the selected cut.
    let baseline = FaceFixture::new(&script())?;
    let (session, _) = cut(&baseline)?;
    let trace = baseline.root.path().join("baseline-trace.jsonl");
    let mut command = baseline.command();
    command
        .args(["agent", "--session", &session])
        .env(TRACE_ENV, &trace);
    let mut agent = FaceAgent::start_command(command)?;
    let approval = agent.wait_for("baseline restored decision", |row| {
        row["kind"] == "approval"
    })?;
    let prefix = trace_runtime_ordinal(&trace)?;
    agent.send(&resolve("baseline-allow", &approval, "allow"))?;
    accepted(&agent, "baseline-allow", "allow")?;
    finish(&mut agent)?;
    assert!(agent.finish()?.0);

    let fixture = FaceFixture::new(&script())?;
    let (session, _) = cut(&fixture)?;
    let trace = fixture.root.path().join("cut-trace.jsonl");
    let target = prefix + 2; // command acceptance, then decision settlement
    let mut command = fixture.command();
    command
        .args(["agent", "--session", &session])
        .env(TRACE_ENV, &trace)
        .env(KILL_ENV, format!("runtime:{target}"));
    let mut agent = FaceAgent::start_command(command)?;
    let approval = agent.wait_for("restored decision before cut", |row| {
        row["kind"] == "approval"
    })?;
    assert_eq!(trace_runtime_ordinal(&trace)?, prefix);
    agent.send(&resolve("cut-allow", &approval, "allow"))?;
    let (success, _, stderr) = agent.finish()?;
    assert!(!success, "cut did not kill the process: {stderr}");
    assert_eq!(trace_runtime_ordinal(&trace)?, target);
    assert_decision_committed_before_dispatch(&fixture)?;

    let mut agent = face_reopen(&fixture, &session)?;
    agent.send(&quit_frame())?;
    let (success, records, stderr) = agent.finish()?;
    assert!(success, "{stderr}");
    assert!(!records.iter().any(|row| row["kind"] == "approval"));
    assert_eq!(evidence_count(&fixture, EvidenceKind::EffectDispatched)?, 0);
    assert_eq!(
        evidence_count(&fixture, EvidenceKind::ProviderDispatched)?,
        1
    );
    let facts = feed(&fixture)?;
    let terminals = facts
        .iter()
        .filter_map(|fact| match fact {
            FeedFact::TurnTerminal { turn_id, .. } => Some(turn_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1);
    assert_eq!(
        fixture
            .session_log_kinds()?
            .iter()
            .filter(|kind| *kind == "turn.abandoned")
            .count(),
        1
    );
    Ok(())
}
