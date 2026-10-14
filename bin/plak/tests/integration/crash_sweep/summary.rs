use super::super::offline_support::{AgentProcess, Fixture};
use super::shapes::{ANSWER, FOLLOWUP};
use plak::home::paths::PlakPaths;
use plak::store::session_records::{SessionRecordStore, SqliteSessionStore};
use plakous::sessions::{AgentSessions, SessionLocator};
use plakous_offline_crash_harness::ledger_signature::{
    inspect_family_artifact, inspect_jobs, inspect_ledger, inspect_process_notices,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Summary {
    pub head: String,
    pub revision: i64,
    pub phase: Option<String>,
    pub runtime_verdict: String,
    pub head_turn: Option<String>,
    pub root_lifecycles: Vec<Value>,
    pub obligations: Vec<(u64, String)>,
    pub ledger: Value,
    pub provider_owners: Value,
    pub durable_answer: bool,
    pub family_artifact: Option<Value>,
    pub jobs: Vec<Value>,
    pub job_notices: Vec<u32>,
    pub locator: Option<String>,
    pub drain: String,
    pub submissions: Vec<Value>,
    pub workflow_requests: Vec<Value>,
    pub queue: Vec<String>,
    pub original_queued: bool,
    pub queued_prompt_pending: bool,
    pub transcript_kinds: BTreeMap<String, usize>,
    pub original_answers: usize,
    pub capacity_terminals: usize,
    pub queued_answers: usize,
    pub published_completed_commands: Vec<String>,
    pub unapplied_steering_notices: usize,
    pub followup_answers: usize,
    pub abandonment_claims: Vec<String>,
    pub root_turn_count: i64,
    pub tool_call_count: i64,
    pub tree_nodes: Vec<Value>,
}

pub(super) fn outcome_allows_verdict(outcome: &str, verdict: &str) -> bool {
    match outcome.split('{').next().unwrap_or(outcome) {
        "ABANDONED" => verdict == "RecoversAbandonedTurn",
        "COMPLETED_ONCE" => matches!(
            verdict,
            "RecoversSavedTerminal"
                | "ResolvesCompactionWorkflow"
                | "RecoversCommit"
                | "OpensQuiescent"
        ),
        "NOT_ADMITTED" | "REQUEUED" => verdict == "OpensQuiescent",
        "REFUSED" => verdict == "Refused",
        _ => false,
    }
}

pub(super) fn assert_outcome_verdict(outcome: &str, before: &Summary, baseline: &Summary) {
    let roots = before
        .submissions
        .iter()
        .filter(|submission| {
            submission["kind"] == "root_start"
                && !baseline
                    .submissions
                    .iter()
                    .any(|old| old["command"] == submission["command"])
        })
        .collect::<Vec<_>>();
    // The verdict is about the head, not a completed original followed by a later root.
    if outcome == "COMPLETED_ONCE"
        && let Some(head_turn) = &before.head_turn
    {
        let original = roots.first().expect("original root submission");
        let head = before
            .root_lifecycles
            .iter()
            .find(|root| root["turn"] == *head_turn)
            .expect("head command lifecycle");
        if head["command"] != original["command"] {
            let later = roots
                .iter()
                .skip(1)
                .find(|submission| submission["command"] == head["command"])
                .expect("distinct later root submission");
            let original = before
                .root_lifecycles
                .iter()
                .find(|root| root["command"] == original["command"])
                .expect("original admitted command");
            assert!(
                head["sequence"].as_u64().unwrap() > original["sequence"].as_u64().unwrap(),
                "head is not a later admission"
            );
            assert_eq!(
                original["state"], "finalized",
                "original must already be terminal"
            );
            assert_ne!(later["state"], "rejected", "later root was refused");
            let later_outcome = match head["state"].as_str().unwrap() {
                "running" => "ABANDONED",
                "saved_terminal" | "completed" | "finalized" => "COMPLETED_ONCE",
                _ => panic!("unsupported later root lifecycle"),
            };
            assert!(
                outcome_allows_verdict(later_outcome, &before.runtime_verdict),
                "later root outcome/verdict mismatch: {later_outcome} / {}",
                before.runtime_verdict
            );
            return;
        }
    }
    assert!(
        outcome_allows_verdict(outcome, &before.runtime_verdict),
        "outcome/verdict mismatch: {outcome} / {}",
        before.runtime_verdict
    );
}

#[test]
fn outcome_verdict_table_rejects_every_mismatch() {
    let verdicts = [
        "OpensQuiescent",
        "RecoversSavedTerminal",
        "RecoversAbandonedTurn",
        "ResolvesCompactionWorkflow",
        "RecoversCommit",
        "RestoresEffectDecision",
        "RestoresHostedContinuation",
        "RestoresWorkingFrame",
        "RefusesRecoveryScope",
        "Refused",
        "unknown",
    ];
    let cases: &[(&str, &[&str])] = &[
        ("ABANDONED{model_request}", &["RecoversAbandonedTurn"]),
        (
            "COMPLETED_ONCE",
            &[
                "RecoversSavedTerminal",
                "ResolvesCompactionWorkflow",
                "RecoversCommit",
                "OpensQuiescent",
            ],
        ),
        ("NOT_ADMITTED", &["OpensQuiescent"]),
        ("REQUEUED", &["OpensQuiescent"]),
        ("REFUSED{scope}", &["Refused"]),
        ("unknown", &[]),
    ];
    for (outcome, allowed) in cases {
        for verdict in verdicts {
            assert_eq!(
                outcome_allows_verdict(outcome, verdict),
                allowed.contains(&verdict),
                "{outcome} / {verdict}"
            );
        }
    }
}

#[test]
fn outcome_verdict_binds_later_root_to_its_own_lifecycle() {
    let baseline = Summary::default();
    let mut before = Summary {
        runtime_verdict: "RecoversSavedTerminal".into(),
        head_turn: Some("original-turn".into()),
        submissions: vec![
            serde_json::json!({"command": "original", "kind": "root_start", "state": "accepted"}),
        ],
        root_lifecycles: vec![
            serde_json::json!({"command": "original", "turn": "original-turn", "sequence": 1, "state": "completed"}),
        ],
        ..Summary::default()
    };
    assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline);
    before.root_lifecycles[0]["state"] = serde_json::json!("finalized");
    before.head_turn = Some("later-turn".into());
    before
        .submissions
        .push(serde_json::json!({"command": "later", "kind": "root_start", "state": "pending"}));
    before.root_lifecycles.push(serde_json::json!({"command": "later", "turn": "later-turn", "sequence": 2, "state": "running"}));
    before.runtime_verdict = "RecoversAbandonedTurn".into();
    assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline);
    before.runtime_verdict = "RecoversSavedTerminal".into();
    assert!(
        std::panic::catch_unwind(|| assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline))
            .is_err()
    );
    before.root_lifecycles[1]["state"] = serde_json::json!("saved_terminal");
    assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline);
    before.root_lifecycles[1]["state"] = serde_json::json!("completed");
    assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline);
    before.runtime_verdict = "RecoversAbandonedTurn".into();
    assert!(
        std::panic::catch_unwind(|| assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline))
            .is_err()
    );
    before.submissions.pop();
    assert!(
        std::panic::catch_unwind(|| assert_outcome_verdict("COMPLETED_ONCE", &before, &baseline))
            .is_err()
    );
}

fn open(path: &Path) -> Connection {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("read-only database")
}

pub(super) fn paths(fixture: &Fixture) -> PlakPaths {
    PlakPaths::explicit(fixture.home())
}

struct RuntimeFacts {
    ledger: Value,
    durable_answer: bool,
    family_artifact: Option<Value>,
    jobs: Vec<Value>,
    job_notices: Vec<u32>,
}

fn runtime_facts(database: &Path, locator: SessionLocator) -> RuntimeFacts {
    let facts = inspect_ledger(database, locator, ANSWER).expect("decoded ledger");
    let durable_answer = facts.iter().any(|fact| fact.answer_marker);
    let ledger = serde_json::to_value(facts).unwrap();
    let job_notices =
        inspect_process_notices(database, locator).expect("read-only canonical process notices");
    let jobs = inspect_jobs(database, locator)
        .expect("read-only jobs")
        .into_iter()
        .map(|job| serde_json::to_value(job).unwrap())
        .collect();
    let family_artifact = inspect_family_artifact(database, locator)
        .expect("read-only family artifact")
        .map(|artifact| {
            let digest = plakous::model::Digest::sha256(
                plakous::model::DigestDomain::ProviderOperation,
                1,
                &[artifact.bytes()],
            )
            .expect("artifact digest");
            json!({"family": artifact.family(), "schema": artifact.schema_revision(),
                "codec": artifact.codec_revision(), "digest": digest.to_string(),
                "fixture_native": artifact.bytes().starts_with(b"fixture-encrypted-native-")})
        });
    RuntimeFacts {
        ledger,
        durable_answer,
        family_artifact,
        jobs,
        job_notices,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one read-only pass over the three stores builds the whole summary"
)]
pub(super) fn read(fixture: &Fixture) -> Summary {
    let paths = paths(fixture);
    let runtime = open(&paths.runtime_database());
    let head: String = runtime
        .query_row(
            "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches",
            [],
            |r| r.get(0),
        )
        .expect("runtime head");
    let locator: SessionLocator = head.parse().expect("head locator");
    let report = AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), locator)
        .expect("read-only recovery facts");
    let runtime_verdict = format!(
        "{:?}",
        plakous::sessions::rehearsal::inspect_sqlite(paths.runtime_database(), locator)
            .expect("pure runtime verdict")
            .outcome
    );
    let phase = report.as_ref().map(|r| format!("{:?}", r.phase()));
    let (head_turn, root_lifecycles) =
        plakous_offline_crash_harness::ledger_signature::inspect_root_lifecycles(
            &paths.runtime_database(),
            locator,
        )
        .expect("decoded head root lifecycles");
    let obligations = report
        .as_ref()
        .map(|r| {
            r.obligations()
                .iter()
                .map(|o| (o.operation_id(), format!("{:?}", o.action())))
                .collect()
        })
        .unwrap_or_default();
    let facts = runtime_facts(&paths.runtime_database(), locator);
    let provider_owners = serde_json::to_value(
        plakous_offline_crash_harness::ledger_signature::inspect_provider_owners(
            &paths.runtime_database(),
            locator,
            ANSWER,
        )
        .unwrap(),
    )
    .unwrap();
    let product = open(&paths.product_database());
    let (product_locator, revision) = product
        .query_row(
            "SELECT json_extract(v.state,'$.head.session_id') || '/' || json_extract(v.state,'$.head.branch_id') || '/' || json_extract(v.state,'$.head.checkpoint_id'),revision FROM sessions LEFT JOIN session_views v ON v.session_id=sessions.id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    // The counters are the views (spec 02 §2.6), read where the product reads them.
    let (root_turn_count, tool_call_count) = product
        .query_row(
            "SELECT COALESCE(MAX(v.root_turn_count),0),COALESCE(MAX(v.tool_call_count),0) FROM sessions LEFT JOIN session_views v ON v.session_id=sessions.id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let submissions = rows(
        &product,
        r#"SELECT json_object('command',s.command_id,'queue',s.queue_item_id,'kind',s.kind,'state',CASE WHEN json_extract(v.state,'$.deliveries."'||s.command_id||'"')='accepted' THEN 'accepted' WHEN json_extract(v.state,'$.deliveries."'||s.command_id||'".rejected') IS NOT NULL THEN 'rejected' ELSE 'pending' END) FROM submissions AS s LEFT JOIN session_views AS v ON v.session_id=s.session_id ORDER BY s.rowid"#,
    );
    let workflow_requests = rows(
        &product,
        r#"SELECT json_object('operation',w.operation_id,'kind',w.kind,'state',CASE WHEN json_extract(v.state,'$.workflows."'||w.operation_id||'".settled') IS NOT NULL THEN 'settled' WHEN json_extract(v.state,'$.workflows."'||w.operation_id||'".admitted_at') IS NOT NULL THEN 'admitted' ELSE 'pending' END,'source',w.source_locator) FROM workflow_requests AS w LEFT JOIN session_views AS v ON v.session_id=w.session_id ORDER BY w.rowid"#,
    );
    let product_id: String = product
        .query_row("SELECT id FROM sessions", [], |row| row.get(0))
        .unwrap();
    let record = plak::store::ProductStore::inspect_session_read_only(&paths, &product_id).unwrap();
    let drain = record.queue_drain_state.as_str().to_owned();
    let queue = record
        .pending_queue
        .iter()
        .map(|item| item.id.clone())
        .collect();
    let queued = |prompt: &str| record.pending_queue.iter().any(|item| item.text == prompt);
    let original_queued = queued("crash-sweep-original-prompt");
    let queued_prompt_pending = queued(super::shapes::c1::QUEUED_PROMPT);
    let transcript = rows(
        &product,
        "SELECT row FROM (SELECT anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT anchor,1 AS lane,ordinal,row FROM operator_notices) ORDER BY anchor,lane,ordinal",
    );
    let mut transcript_kinds = BTreeMap::new();
    for row in &transcript {
        *transcript_kinds
            .entry(row["kind"].as_str().unwrap().to_owned())
            .or_insert(0) += 1;
    }
    let count = |marker: &str| {
        transcript
            .iter()
            .filter(|row| row["text"] == marker)
            .count()
    };
    let original_answers = count(ANSWER);
    let capacity_terminals = transcript
        .iter()
        .filter(|row| {
            row["kind"] == "notice"
                && row["tone"] == "error"
                && row["scope"] == "turn"
                && row["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(super::shapes::c1::CAPACITY))
        })
        .count();
    let queued_answers = count(super::shapes::c1::QUEUED);
    let published_completed_commands = super::publication::completed_publications(
        &plakous_offline_crash_harness::ledger_signature::inspect_completed_root_answers(
            &paths.runtime_database(),
            locator,
        )
        .expect("decoded completed root terminals"),
        &submissions,
        &transcript,
    );
    let followup_answers = count(FOLLOWUP);
    let abandonment_claims = log_rows(fixture)
        .into_iter()
        .filter(|row| row["kind"] == "turn.abandoned")
        .map(|row| {
            row["claim"]
                .as_str()
                .expect("typed abandonment claim")
                .to_owned()
        })
        .collect();
    let tree_nodes = read_tree_nodes(&paths);
    Summary {
        head,
        revision,
        phase,
        runtime_verdict,
        head_turn,
        root_lifecycles,
        obligations,
        ledger: facts.ledger,
        provider_owners,
        durable_answer: facts.durable_answer,
        family_artifact: facts.family_artifact,
        jobs: facts.jobs,
        job_notices: facts.job_notices,
        locator: product_locator,
        drain,
        submissions,
        workflow_requests,
        queue,
        original_queued,
        queued_prompt_pending,
        transcript_kinds,
        original_answers,
        capacity_terminals,
        queued_answers,
        published_completed_commands,
        unapplied_steering_notices: transcript
            .iter()
            .filter(|row| {
                row["text"].as_str().is_some_and(|text| {
                    text == plakous_hosted::runtime::RECOVERED_COMPLETED_STEERING_NOTICE
                        || text.ends_with("; queued steering was not applied and is not retried")
                })
            })
            .count(),
        followup_answers,
        abandonment_claims,
        root_turn_count,
        tool_call_count,
        tree_nodes,
    }
}

fn read_tree_nodes(paths: &PlakPaths) -> Vec<Value> {
    if !paths.subagents_database().is_file() {
        return Vec::new();
    }
    rows(
        &open(&paths.subagents_database()),
        "SELECT json_object('tree',tree_id,'agent',agent_id,'status',status,'locator',session_locator,'active',active_turn) FROM agent_nodes ORDER BY tree_id,agent_id",
    )
}

fn rows(connection: &Connection, sql: &str) -> Vec<Value> {
    let mut stmt = connection.prepare(sql).unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
        .collect()
}

pub(super) fn log_rows(fixture: &Fixture) -> Vec<Value> {
    let mut rows = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths(fixture).sessions_dir()) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                for line in std::fs::read_to_string(entry.path()).unwrap().lines() {
                    rows.push(serde_json::from_str(line).expect("session log row"));
                }
            }
        }
    }
    rows
}

pub(super) fn database_bytes(fixture: &Fixture) -> BTreeMap<PathBuf, Vec<u8>> {
    let paths = paths(fixture);
    let mut bytes = BTreeMap::new();
    // SHM contains transient SQLite reader coordination, not durable authority.
    for database in [
        paths.runtime_database(),
        paths.product_database(),
        paths.subagents_database(),
    ] {
        for path in [
            database.clone(),
            PathBuf::from(format!("{}-wal", database.display())),
        ] {
            if path.is_file() {
                bytes.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    bytes
}

pub(super) fn rehearse(fixture: &Fixture, session: &str) -> Value {
    let before = database_bytes(fixture);
    let mut command = fixture.command();
    command.args(["session", "rehearse", session]);
    let outcome = AgentProcess::start_command(command, true).finish();
    assert!(
        before == database_bytes(fixture),
        "rehearsal wrote durable databases"
    );
    let root = outcome
        .lines
        .iter()
        .find(|line| line.starts_with("root: offline=") || line.starts_with("root: refuses:"))
        .expect("typed root rehearsal verdict");
    let verdict = if root.contains("refuses:") {
        "Refused"
    } else {
        root.split("offline=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
    };
    json!({"verdict": verdict, "success": outcome.status.success(), "database_byte_identical": true})
}

pub(super) fn refusal_signature(shape: &str, state: &Summary) -> String {
    json!({
        "shape": shape,
        "phase": state.phase,
        "obligations": state.obligations,
        "ledger": state.ledger,
        "submissions": state.submissions.iter().map(|row| json!({"kind": row["kind"], "state": row["state"]})).collect::<Vec<_>>(),
        "locator": if state.locator.as_ref() == Some(&state.head) { "exact" } else { "stale" },
        "queue_length": state.queue.len(),
        "durable_answer": state.durable_answer,
    }).to_string()
}

pub(super) fn read_converged(fixture: &Fixture) -> Summary {
    read_projected(fixture, false)
}

/// Reopen must pay its own projection debt. The harness pass only checks
/// idempotence, never repairs a missed step 0 or step 2.
pub(super) fn read_reopened(fixture: &Fixture) -> Summary {
    read_projected(fixture, true)
}

fn read_projected(fixture: &Fixture, require_converged: bool) -> Summary {
    let paths = paths(fixture);
    let session: String = open(&paths.product_database())
        .query_row("SELECT id FROM sessions", [], |row| row.get(0))
        .expect("product session");
    let records: Arc<dyn SessionRecordStore> = Arc::new(SqliteSessionStore::new(paths.clone()));
    let record = records.load(&session).expect("stored session");
    let sessions = plakous::sessions::SessionBackend::sqlite(paths.runtime_database())
        .and_then(AgentSessions::new)
        .expect("runtime sessions");
    let executor = plakous::application::ApplicationExecutor::new().expect("application executor");
    let before = require_converged.then(|| {
        use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
        let views = records
            .load_views(&session)
            .expect("stored views")
            .expect("reopen alone must converge product views to the runtime tail");
        let runtime_session = record
            .plakous_locator
            .expect("runtime binding")
            .session_id();
        let request = FeedPageRequest::after(FeedCursor::new(runtime_session, views.cursor));
        let page = executor
            .block_on(sessions.feed_page(request))
            .expect("feed execution")
            .expect("feed tail");
        assert!(
            matches!(page, FeedPage::Records { records, tail, .. }
                if records.is_empty() && tail == views.cursor),
            "reopen alone must converge product views to the runtime tail"
        );
        views
    });
    // Only the host command receives TRACE_ENV/KILL_ENV. This lease-free,
    // cursor-CAS'd pass is not an indexed commit in that process's cut trace.
    executor
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &records, &record,
        ))
        .expect("projector execution")
        .expect("project views to feed tail")
        .expect("session runtime");
    if let Some(before) = before {
        assert_eq!(
            records.load_views(&session).expect("stored views"),
            Some(before),
            "the harness projection after reopen must be idempotent"
        );
    }
    read(fixture)
}
