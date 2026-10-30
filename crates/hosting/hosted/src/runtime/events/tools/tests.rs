#![allow(
    clippy::too_many_lines,
    reason = "Tool-event scenarios retain the full nested call and settlement sequence needed to compare parent and child activity."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::session_store::SessionRecordStore;

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;

use crate::runtime::events::apply_event;
use crate::runtime::events::tools::observe_tool_activity;
use crate::runtime::test_support::host_event_sink;
use crate::runtime::{ToolSubjectStash, TreeSnapshotPush, TurnProjection, test_kit};
use crate::session_home::SessionHome;
use crate::sessions;
use crate::sessions::SessionRecord;
use plakous::agent::AgentEvent;
use std::time::{Duration, Instant};

#[test]
fn sdk_uses_a_generic_tool_row_with_safely_fenced_code_and_output() {
    let observation = serde_json::from_value(serde_json::json!({
        "subject": null,
        "arguments_preview": serde_json::json!({"code":"print('```')"}).to_string(),
    }))
    .unwrap();
    let commands = std::collections::HashMap::new();
    let started = super::started_tool_text(
        "run_py",
        None,
        Some(&observation),
        None,
        super::ProcessContext {
            label: None,
            args: None,
            locator: None,
            processes: &[],
            commands: &commands,
        },
    );
    assert_eq!(
        crate::activity::tool_activity_kind("run_py"),
        plakous_host_protocol::views::presentation::ActivityKind::Tool
    );
    let detail = super::completed_tool_detail(started.detail.as_deref(), Some("finished")).unwrap();
    assert!(
        detail.contains("````python\nprint('```')\n````"),
        "{detail}"
    );
    assert!(detail.ends_with("finished"), "{detail}");
}

#[test]
fn process_wait_uses_its_result_after_timeout_exit_race() {
    use plakous::model::{Digest, DigestDomain};
    use plakous::tools::{JobHost, JobOwner, JobStatus, JobView};
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let empty_roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("empty-state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
    session.plakous_locator = Some(locator);
    let (events, rx) = host_event_sink();
    let mut projection = TurnProjection::default();
    let mut stash = ToolSubjectStash::new();
    let mut assistant = String::new();
    let mut delta = String::new();
    let mut last_flush = Instant::now();
    let mut tree = TreeSnapshotPush::default();
    let mut compacted = false;
    let mut peak = 0;
    let uuid = uuid::Uuid::from_u128(1);
    let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
    {
        let mut dispatch = |roots: &crate::agent::ProductStateRoots,
                            name: &str,
                            call: u128,
                            kind: &str,
                            observation: serde_json::Value| {
            let identity = serde_json::json!({
                "canonical_name": name,
                "exposed_name": null,
                "exposure_map_digest": null,
                "registered_tool_id": "test.process",
                "executor_id": "test.process",
                "definition_digest": digest,
                "schema_digest": digest,
                "effect_metadata_digest": digest,
                "argument_limits_digest": digest,
            });
            let event: AgentEvent = serde_json::from_value(serde_json::json!({
                "branch_sequence": 1,
                "event_id": uuid,
                "session_id": locator.session_id(),
                "branch_id": locator.branch_id(),
                "agent_id": uuid,
                "run_id": uuid,
                "turn_id": null,
                "attempt_id": null,
                "call_id": uuid::Uuid::from_u128(call),
                "timestamp": {"seconds": 1, "nanoseconds": 0},
                "causal_parent": null,
                "origin": "Tool",
                "classification": "Public",
                "event_schema": 3,
                "projection_revision": 6,
                "visibility": "Content",
                "scope": "Agent",
                "event_kind_version": 11,
                "kind": {kind: {
                    "identity": identity,
                    "lineage": plakous::agent::ToolCallerLineageView::direct(),
                    "observation": observation,
                }},
                "retained_bytes": 512,
                "metadata_retained_bytes": 512,
                "content_retained_bytes": 0,
            }))
            .unwrap();
            apply_event(
                &event,
                roots,
                &rows,
                &store,
                &home,
                &mut session,
                &events,
                &mut assistant,
                &mut delta,
                &mut last_flush,
                &mut stash,
                &mut projection,
                None,
                "test",
                &mut tree,
                &mut compacted,
                &mut peak,
            );
        };
        let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
        let completed = |result: serde_json::Value| serde_json::json!({"succeeded":true,"preview":result.to_string()});
        dispatch(
            &roots,
            "process_session_start",
            10,
            "ToolStarted",
            started(serde_json::json!({"command":"cargo test"})),
        );
        dispatch(
            &roots,
            "process_session_start",
            10,
            "ToolCompleted",
            completed(serde_json::json!({"session_id":7,"status":"running","exit_code":null})),
        );
        dispatch(
            &roots,
            "process_session_poll",
            11,
            "ToolStarted",
            started(serde_json::json!({"session_id":7,"yield_time_ms":1000})),
        );
        // The wait timed out, then the process exited before the host consumed it.
        roots.job_registry().observe_terminal(
            JobOwner {
                session_id: locator.session_id(),
                branch_id: locator.branch_id(),
                agent_id: plakous::model::AgentId::try_from(uuid).unwrap(),
            },
            &[JobView {
                number: 7,
                command: "cargo test".into(),
                started_at: plakous::model::Timestamp::new(1, 0).unwrap(),
                owner: "root".into(),
                elapsed: Duration::ZERO,
                status: JobStatus::Exited(Some(0)),
                output: String::new(),
                session_id: [7; 16],
                owner_generation: 1,
            }],
        );
        dispatch(
            &roots,
            "process_session_poll",
            11,
            "ToolCompleted",
            completed(serde_json::json!({"session_id":7,"status":"running","exit_code":null})),
        );
        // A later wait still knows the start row's command after registry eviction.
        dispatch(
            &empty_roots,
            "process_session_poll",
            12,
            "ToolStarted",
            started(serde_json::json!({"session_id":7,"yield_time_ms":1000})),
        );
        dispatch(
            &empty_roots,
            "process_session_poll",
            12,
            "ToolCompleted",
            completed(
                serde_json::json!({"tasks":[{"task_id":"p7","status":"exited","exit_code":0}]}),
            ),
        );
    }
    let emitted = rx.try_iter().collect::<Vec<_>>();
    for (call, expected) in [
        (11, "Waited on process 7 · cargo test · still running"),
        (12, "Waited on process 7 · cargo test · exited 0"),
    ] {
        let id = uuid::Uuid::from_u128(call).to_string();
        assert_eq!(
            session
                .transcript
                .tool_activity(&id)
                .unwrap()
                .subject
                .as_deref(),
            Some(expected)
        );
        assert!(emitted.iter().any(|event| matches!(event, HostEvent::ToolLine { call_id:Some(call_id), subject:Some(subject), .. } if call_id == &id && subject == expected)));
    }
}

/// Spec 02 §5.4: a published notice in a persisted scope joins the
/// session's transcript in publication order, so a restore shows it as it
/// was shown live; settings, approval and clipped feedback stay live only,
/// a diagnostic is never persisted, a child's live rows never enter the
/// root transcript, a row its producer already stored is not kept twice,
/// a re-announced state note is never kept, and a background publisher's
/// notice for a session the host no longer owns is dropped.
#[test]
fn live_tool_observations_never_commit_product_records_or_views() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::session_store::MemoryStore::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    store.save(&mut session).unwrap();
    let revision = session.store_revision;
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "live-call".into(),
            name: "read_file".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("read_file"),
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        },
    );
    assert!(session.transcript.tool_activity("live-call").is_some());
    assert_eq!(session.store_revision, revision);
    assert_eq!(store.load(&session.id).unwrap().store_revision, revision);
    assert!(store.load_views(&session.id).unwrap().is_none());
    assert_eq!(
        store.load_intents(&session.id).unwrap().notices,
        [] as [sessions::projector::OperatorNotice; 0]
    );
}
