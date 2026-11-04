#![allow(
    clippy::too_many_lines,
    reason = "JSONL protocol scenarios retain complete ordered envelopes and additive projection assertions."
)]

use super::*;
use crate::surface_protocol::HostEvent;
use plakous_host_protocol::QueueDrainState;
use plakous_host_protocol::status::{DiagnosticText, HostPhase, HostStatusDetail};

#[test]
fn remedies_and_live_pause_project_through_existing_jsonl_envelopes() {
    use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy};
    use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
    let correlation = JsonlCorrelation {
        prompt_id: Some("prompt"),
        command_id: Some("command"),
        ..Default::default()
    };
    let line = project_host_event(
        &HostEvent::Row(
            TranscriptRow::info(NoticeScope::Session, "opaque").with_remedy(NoticeRemedy::Login {
                account: AccountTarget::Grok,
            }),
        ),
        correlation,
        Some("exact-root"),
    )
    .unwrap();
    let value = object_from_line(&line);
    assert_eq!(value["kind"], "status");
    assert_eq!(value["phase"], "note");
    assert_eq!(value["queue_len"], 0);
    assert_eq!(value["id"], "command");
    assert!(
        value["detail"]
            .as_str()
            .unwrap()
            .contains("plak login grok")
    );
    assert!(!value.contains_key("remedy"));
    let line = project_host_event(
        &HostEvent::RestoredPause(plakous_host_protocol::status::HostPause { queue: true }),
        correlation,
        Some("exact-root"),
    )
    .unwrap();
    let value = object_from_line(&line);
    assert_eq!(value["id"], "command");
    assert_eq!(value["queue_len"], 0);
    let detail = value["detail"].as_str().unwrap();
    assert!(detail.contains("resume_queue") && !detail.contains("resume_tree"));
    assert!(!detail.contains("Tab") && !detail.contains("/attach"));
    let line = project_host_event(
        &HostEvent::StorageAdmissionRefused(DiagnosticText::new("opaque storage facts")),
        correlation,
        Some("exact-root"),
    )
    .unwrap();
    let value = object_from_line(&line);
    assert_eq!(value["id"], "command");
    assert_eq!(value["kind"], "status");
    assert_eq!(value["phase"], "note");
    assert!(value["detail"].as_str().unwrap().contains("plak doctor"));
}

/// The session binding changes only through templates and profiles, so
/// the retired binding kinds are unknown kinds like any other.
#[test]
fn removed_binding_kinds_are_unknown_commands() {
    for line in [
        r#"{"v":1,"kind":"set_model","id":"m","account":"codex","model":"gpt-5.6-sol","effort":"medium"}"#,
        r#"{"v":1,"kind":"set_effort","id":"e","effort":"high"}"#,
        r#"{"v":1,"kind":"set_account","id":"a","account":"codex"}"#,
    ] {
        let ParseOutcome::Diagnostic { message, .. } = parse_command_line(line) else {
            panic!("a retired binding kind must be a diagnostic: {line}");
        };
        assert!(message.starts_with("unknown JSONL kind `set_"), "{message}");
    }
}

#[test]
fn removed_resume_tree_is_an_unknown_command() {
    let ParseOutcome::Diagnostic { message, .. } =
        parse_command_line(r#"{"v":1,"kind":"resume_tree","id":"r"}"#)
    else {
        panic!("resume_tree is no longer a command");
    };
    assert!(
        message.starts_with("unknown JSONL kind `resume_tree`"),
        "{message}"
    );
}

#[test]
fn removed_harness_mode_is_an_unknown_command() {
    for intent in ["enter", "approve", "off"] {
        let line =
            serde_json::json!({"v": 1, "kind": "plan", "id": "p", "intent": intent}).to_string();
        assert!(matches!(
            parse_command_line(&line),
            ParseOutcome::Diagnostic { .. }
        ));
    }
}

#[test]
fn fork_maps_to_user_action() {
    let ParseOutcome::Command(command) =
        parse_command_line(r#"{"v":1,"kind":"fork","id":"f","account":"grok"}"#)
    else {
        panic!("fork");
    };
    assert_eq!(
        command.into_user_action(false),
        Some(UserAction::ForkSession {
            account: Some("grok".into()),
            compact: false
        })
    );
}

#[test]
fn raw_frame_id_and_decoded_prompt_bounds_are_independent() {
    for field in ["id", "ignored"] {
        let frame = serde_json::json!({"v": 1, "kind": "tree", field: "x".repeat(MAX_FRAME_BYTES)})
            .to_string();
        assert!(matches!(
            parse_command_line(&frame),
            ParseOutcome::Diagnostic { id: None, .. }
        ));
    }
    let id = serde_json::json!({"v": 1, "kind": "tree", "id": "é".repeat(129)}).to_string();
    assert!(matches!(
        parse_command_line(&id),
        ParseOutcome::Diagnostic { id: None, .. }
    ));
    let prompt = format!(
        "{{\"v\":1,\"kind\":\"prompt\",\"text\":\"{}\"}}",
        "\\u0061".repeat(64 * 1024)
    );
    assert!(matches!(
        parse_command_line(&prompt),
        ParseOutcome::Command(_)
    ));
    let prompt = serde_json::json!({"v": 1, "kind": "prompt", "text": "a".repeat(64 * 1024 + 1)})
        .to_string();
    assert!(matches!(
        parse_command_line(&prompt),
        ParseOutcome::Diagnostic { .. }
    ));
}
use plakous_host_protocol::topology::{
    AttemptStatus, CandidateId, NodeStatus, Tier, TreeAdmissionState, WorkspaceMode,
    WorkspaceStatus,
};
use plakous_host_protocol::views::presentation::ActivityKind;
use plakous_host_protocol::views::{NodeView, TreeView};

#[test]
fn prompt_maps_by_busy_and_steer() {
    let parsed =
        parse_command_line(r#"{"v":1,"kind":"prompt","id":"p1","text":"hello","ignored":true}"#);
    let ParseOutcome::Command(command) = parsed else {
        panic!("{parsed:?}");
    };
    assert_eq!(
        command.clone().into_user_action(false),
        Some(UserAction::Prompt("hello".into()))
    );
    assert_eq!(
        command.clone().into_user_action(true),
        Some(UserAction::FollowUp("hello".into()))
    );
    let ParseOutcome::Command(steer) =
        parse_command_line(r#"{"v":1,"kind":"prompt","id":"p2","text":"now","steer":true}"#)
    else {
        panic!("steer");
    };
    assert_eq!(
        steer.clone().into_user_action(true),
        Some(UserAction::Steer("now".into()))
    );
    assert_eq!(steer.into_user_action(false), None);
}

#[test]
fn prompt_images_are_read_from_paths_and_may_stand_alone() {
    let dir = tempfile::tempdir().unwrap();
    let shot = dir.path().join("shot.png");
    std::fs::write(&shot, plakous_hosted::attachments::png(4, 3, 10)).unwrap();
    let line = |fields: Value| {
        let mut object = serde_json::json!({"v": 1, "kind": "prompt", "id": "p"});
        object
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        parse_command_line(&object.to_string())
    };
    let parsed = line(serde_json::json!({"text": "what is this?", "images": [shot]}));
    let ParseOutcome::Command(command) = parsed else {
        panic!("{parsed:?}");
    };
    let Some(UserAction::Prompt(draft)) = command.into_user_action(false) else {
        panic!("expected a prompt");
    };
    assert_eq!(draft.text, "what is this?");
    assert_eq!(draft.images.len(), 1);
    assert_eq!(draft.images[0].media_type, "image/png");

    let parsed = line(serde_json::json!({"images": [shot, shot]}));
    let ParseOutcome::Command(command) = parsed else {
        panic!("{parsed:?}");
    };
    let Some(UserAction::FollowUp(draft)) = command.into_user_action(true) else {
        panic!("expected a follow-up");
    };
    assert_eq!((draft.text.as_str(), draft.images.len()), ("", 2));

    let largest = dir.path().join("largest.png");
    let header = plakous_hosted::attachments::png(4, 3, 0).len();
    std::fs::write(
        &largest,
        plakous_hosted::attachments::png(
            4,
            3,
            plakous_hosted::attachments::MAX_IMAGE_BYTES - header,
        ),
    )
    .unwrap();
    let parsed = line(serde_json::json!({"images": [&largest, &largest, &largest]}));
    assert!(matches!(parsed, ParseOutcome::Command(_)), "{parsed:?}");

    for (fields, needle) in [
        (serde_json::json!({"images": []}), "text or images"),
        (
            serde_json::json!({"images": "shot.png"}),
            "array of file paths",
        ),
        (
            serde_json::json!({"text": "x", "images": [1]}),
            "array of file paths",
        ),
        (
            serde_json::json!({"text": "x", "images": [dir.path().join("gone.png")]}),
            "gone.png",
        ),
        (
            serde_json::json!({"images": [&largest, &largest, &largest, &largest]}),
            "over the 16 MiB per-message budget",
        ),
    ] {
        let parsed = line(fields);
        assert!(
            matches!(&parsed, ParseOutcome::Diagnostic { message, .. } if message.contains(needle)),
            "{parsed:?}"
        );
    }
}

#[test]
fn unknown_kind_is_diagnostic_and_stays_connected() {
    let outcome = parse_command_line(r#"{"v":1,"kind":"login","id":"x"}"#);
    assert!(matches!(
        outcome,
        ParseOutcome::Diagnostic { id: Some(ref id), .. } if id == "x"
    ));
}

#[test]
fn empty_and_oversize_prompt_are_diagnostics() {
    assert!(matches!(
        parse_command_line(r#"{"v":1,"kind":"prompt","id":"p","text":"  "}"#),
        ParseOutcome::Diagnostic { .. }
    ));
    let huge = "a".repeat(MAX_PROMPT_BYTES + 1);
    let line = format!(r#"{{"v":1,"kind":"prompt","id":"p","text":"{huge}"}}"#);
    assert!(matches!(
        parse_command_line(&line),
        ParseOutcome::Diagnostic { message, .. } if message.contains("maximum")
    ));
}

#[test]
fn closed_kinds_round_trip_to_user_actions() {
    let cases = [
        (
            r#"{"v":1,"kind":"interrupt","id":"i"}"#,
            UserAction::Interrupt,
        ),
        (r#"{"v":1,"kind":"compact","id":"c"}"#, UserAction::Compact),
        (r#"{"v":1,"kind":"recover","id":"r"}"#, UserAction::Recover),
        (
            r#"{"v":1,"kind":"resume_queue","id":"rq"}"#,
            UserAction::ResumeQueueDrain,
        ),
        (
            r#"{"v":1,"kind":"tree","id":"t"}"#,
            UserAction::OpenTopologyNavigator,
        ),
        (
            r#"{"v":1,"kind":"stop_all","id":"s"}"#,
            UserAction::StopAllTopology,
        ),
        (
            r#"{"v":1,"kind":"clear","id":"cl"}"#,
            UserAction::NewSession,
        ),
        (r#"{"v":1,"kind":"quit","id":"q"}"#, UserAction::Quit),
        (
            r#"{"v":1,"kind":"fork","id":"f"}"#,
            UserAction::ForkSession {
                account: None,
                compact: false,
            },
        ),
    ];
    for (line, expected) in cases {
        let ParseOutcome::Command(command) = parse_command_line(line) else {
            panic!("{line}");
        };
        assert_eq!(command.into_user_action(false), Some(expected));
    }
    let ParseOutcome::Command(command) =
        parse_command_line(r#"{"v":1,"kind":"interrupt_node","id":"n","agent_id":"c1"}"#)
    else {
        panic!("interrupt_node");
    };
    assert_eq!(
        command.into_user_action(false),
        Some(UserAction::InterruptTopologyNode {
            agent_id: "c1".into()
        })
    );
    let ParseOutcome::Command(command) =
        parse_command_line(r#"{"v":1,"kind":"close_node","id":"n","agent_id":"c2"}"#)
    else {
        panic!("close_node");
    };
    assert_eq!(
        command.into_user_action(false),
        Some(UserAction::CloseTopologyNode {
            agent_id: "c2".into()
        })
    );
    let ParseOutcome::Command(command) = parse_command_line(
        r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"list_agents","arguments":{"include_completed":true},"agent_id":"root"}"#,
    ) else {
        panic!("invoke_tool");
    };
    assert_eq!(
        command.into_user_action(false),
        Some(UserAction::InvokeTool {
            correlation_id: Some("iv".into()),
            name: "list_agents".into(),
            arguments: r#"{"include_completed":true}"#.into(),
            agent_id: Some("root".into()),
        })
    );
}

#[test]
fn invoke_tool_rejects_coding_names_and_non_object_arguments() {
    let outcome = parse_command_line(
        r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"read_file","arguments":{}}"#,
    );
    assert!(matches!(
        outcome,
        ParseOutcome::Diagnostic { message, .. } if message.contains("topology tool")
    ));
    let outcome = parse_command_line(
        r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"spawn_agent","arguments":"nope"}"#,
    );
    assert!(matches!(
        outcome,
        ParseOutcome::Diagnostic { message, .. } if message.contains("JSON object")
    ));
    let outcome =
        parse_command_line(r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"spawn_agent"}"#);
    assert!(matches!(
        outcome,
        ParseOutcome::Diagnostic { message, .. } if message.contains("requires arguments")
    ));
}

/// The exported name set matches the topology tool set. `merge_workspace` is
/// renamed, not aliased, so a driver still sending it is told so rather
/// than having the call silently accepted or silently dropped.
#[test]
fn jsonl_invoke_tool_accepts_adopt_candidate_and_rejects_merge_workspace() {
    let ParseOutcome::Command(command) = parse_command_line(
        r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"adopt_candidate","arguments":{"candidate_id":"cand-1"}}"#,
    ) else {
        panic!("adopt_candidate is a topology tool");
    };
    assert!(matches!(
        command,
        AgentCommand::InvokeTool { ref name, .. } if name == "adopt_candidate"
    ));
    let outcome = parse_command_line(
        r#"{"v":1,"kind":"invoke_tool","id":"iv","name":"merge_workspace","arguments":{}}"#,
    );
    assert!(matches!(
        outcome,
        ParseOutcome::Diagnostic { message, .. } if message.contains("topology tool")
    ));
}

#[test]
fn interrupt_ignores_unknown_additive_fields() {
    let outcome = parse_command_line(
        r#"{"v":1,"kind":"interrupt","id":"i","future_flag":true,"note":"ignored"}"#,
    );
    let ParseOutcome::Command(command) = outcome else {
        panic!("interrupt with additive fields should parse");
    };
    assert_eq!(command.id(), "i");
    assert_eq!(command.into_user_action(false), Some(UserAction::Interrupt));
}

#[test]
fn correlation_turn_scoped_prefers_prompt_id() {
    let correlation = JsonlCorrelation {
        prompt_id: Some("prompt-1"),
        omit_unknown_turn_id: false,
        command_id: Some("tree-1"),
        pending_tree_snapshot_id: None,
    };
    let assistant = project_host_event(&HostEvent::AssistantDelta("hi".into()), correlation, None)
        .expect("assistant");
    assert_eq!(object_from_line(&assistant)["id"], "prompt-1");

    let tool = project_host_event(
        &HostEvent::ToolLine {
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            call_id: None,
            name: "spawn_agent".into(),
            state: "done".into(),
            subject: None,
            detail: None,
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        },
        correlation,
        None,
    )
    .expect("tool");
    assert_eq!(object_from_line(&tool)["id"], "prompt-1");
}

#[test]
fn correlation_tree_snapshot_uses_pending_tree_id_only() {
    let mut snapshot = TreeView {
        tree_id: "tree-1".into(),
        revision: 2,
        state: TreeAdmissionState::Live,
        solicited: false,
        nodes: vec![],
    };
    let unsolicited = project_host_event(
        &HostEvent::TopologyNavigatorSnapshot(snapshot.clone()),
        JsonlCorrelation {
            prompt_id: Some("prompt-1"),
            omit_unknown_turn_id: false,
            command_id: Some("tree-1"),
            pending_tree_snapshot_id: Some("tree-2"),
        },
        None,
    )
    .expect("tree");
    assert!(!object_from_line(&unsolicited).contains_key("id"));

    snapshot.solicited = true;
    let explicit = project_host_event(
        &HostEvent::TopologyNavigatorSnapshot(snapshot),
        JsonlCorrelation {
            prompt_id: Some("prompt-1"),
            omit_unknown_turn_id: false,
            command_id: Some("tree-2"),
            pending_tree_snapshot_id: Some("tree-2"),
        },
        None,
    )
    .expect("tree");
    assert_eq!(object_from_line(&explicit)["id"], "tree-2");
}

#[test]
fn correlation_errors_use_command_id() {
    let correlation = JsonlCorrelation {
        prompt_id: Some("prompt-1"),
        omit_unknown_turn_id: false,
        command_id: Some("tree-1"),
        pending_tree_snapshot_id: None,
    };
    let error = project_host_event(
        &HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::error(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "grant is missing",
            ),
        ),
        correlation,
        None,
    )
    .expect("error");
    assert_eq!(object_from_line(&error)["id"], "tree-1");
}

#[test]
fn idle_invoke_tool_events_echo_command_id() {
    let correlation = JsonlCorrelation {
        prompt_id: None,
        omit_unknown_turn_id: false,
        command_id: Some("iv"),
        pending_tree_snapshot_id: None,
    };
    let tool = project_host_event(
        &HostEvent::ToolLine {
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            call_id: Some("call-1".into()),
            name: "list_agents".into(),
            state: "done".into(),
            subject: None,
            detail: Some("{\"agents\":[]}".into()),
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        },
        correlation,
        None,
    )
    .expect("tool");
    let tool = object_from_line(&tool);
    assert_eq!(tool["id"], "iv");
    assert_eq!(tool["kind"], "tool");
    assert_eq!(tool["state"], "done");
    let error = project_host_event(
        &HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::error(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "topology tool `spawn_agent` is not granted to `root`",
            ),
        ),
        correlation,
        None,
    )
    .expect("error");
    assert_eq!(object_from_line(&error)["id"], "iv");
    let idle = project_host_event(
        &HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "invoke_tool: available when idle (finish or interrupt the turn)",
            ),
        ),
        correlation,
        None,
    )
    .expect("status");
    let idle = object_from_line(&idle);
    assert_eq!(idle["id"], "iv");
    assert_eq!(idle["kind"], "status");
}

/// Regression: the v1 wire dropped every `ToolLine` carrying a
/// `duration_ms`, and the host stamps one on every terminal line it can,
/// so `plak agent` announced a tool and never terminalised it.
#[test]
fn a_terminal_tool_line_reaches_the_wire_with_its_duration() {
    let terminal = |state: &str| HostEvent::ToolLine {
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Tool,
        call_id: Some("call-1".into()),
        name: "read_file".into(),
        state: state.into(),
        subject: Some("src/lib.rs".into()),
        detail: None,
        duration_ms: Some(42),
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    };
    for state in ["done", "failed", "cancelled", "rejected"] {
        let line = project_host_event(&terminal(state), JsonlCorrelation::default(), None)
            .unwrap_or_else(|| panic!("terminal `{state}` tool line has no v1 line"));
        let object = object_from_line(&line);
        assert_eq!(object["kind"], "tool");
        assert_eq!(object["state"], state);
        assert_eq!(object["duration_ms"], 42);
        assert!(required_stdout_kind(&line));
    }
    // An unstamped line stays unstamped rather than claiming a zero.
    let unstamped = object_from_line(
        &project_host_event(
            &HostEvent::ToolLine {
                parent_call_id: None,
                cell_id: None,
                kind: ActivityKind::Tool,
                call_id: Some("call-1".into()),
                name: "read_file".into(),
                state: "running".into(),
                subject: None,
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            JsonlCorrelation::default(),
            None,
        )
        .expect("running tool line"),
    );
    assert!(!unstamped.contains_key("duration_ms"));
}

#[test]
fn tool_error_tree_are_required_stdout() {
    let tool = project_host_event(
        &HostEvent::ToolLine {
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            call_id: Some("call-1".into()),
            name: "spawn_agent".into(),
            state: "done".into(),
            subject: Some("c1".into()),
            detail: None,
            // A terminal line the host could stamp. It is the case that
            // matters: a client that never receives it is left with a
            // `running` row that never terminalises.
            duration_ms: Some(1_234),
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        },
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .unwrap();
    let object = object_from_line(&tool);
    assert_eq!(object["kind"], "tool");
    assert_eq!(object["name"], "spawn_agent");
    assert_eq!(object["id"], "p1");
    assert_eq!(object["state"], "done");
    assert_eq!(object["duration_ms"], 1_234);
    assert!(required_stdout_kind(&tool));

    for name in ["exec_js", "exec_py", "wait_js", "wait_py"] {
        let line = project_host_event(
            &HostEvent::ToolLine {
                parent_call_id: None,
                cell_id: None,
                kind: ActivityKind::Compose,
                call_id: Some("cell-1".into()),
                name: name.into(),
                state: "running".into(),
                subject: Some("text(\"ptc-ok-js\")".into()),
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            JsonlCorrelation {
                prompt_id: Some("p1"),
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None,
            },
            None,
        )
        .unwrap();
        let object = object_from_line(&line);
        assert_eq!(object["kind"], "tool");
        assert_eq!(object["name"], name);
        assert!(required_stdout_kind(&line));
    }

    let error = project_host_event(
        &HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::error(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "grant is missing",
            ),
        ),
        JsonlCorrelation::default(),
        None,
    )
    .unwrap();
    assert!(required_stdout_kind(&error));

    let tree = project_host_event(
        &HostEvent::TopologyNavigatorSnapshot(TreeView {
            tree_id: "tree-1".into(),
            revision: 3,
            state: TreeAdmissionState::Live,
            solicited: false,
            nodes: vec![NodeView {
                agent_id: "tree-1".into(),
                parent_agent_id: None,
                depth: 0,
                status: NodeStatus::Running,
                attempt_status: None,
                attempt_seq: None,
                attempt_id: None,
                attempts: Vec::new(),
                tier: None,
                description: "root".into(),
                prompt: "root".into(),
                workspace_path: "/tmp".into(),
                workspace_mode: WorkspaceMode::Inherit,
                workspace_status: None,
                candidate_id: None,
                blocked: None,
                usage: None,
                active_turn: true,
                live_progress: None,
                unread_mail: 0,
                has_session: true,
                template: None,
                model_account: "grok".into(),
                model_tuple: "grok-4.6".into(),
                model_effort: None,
                turns: Some(1),
                started_at_unix: None,
                ended_at_unix: None,
                fault: None,
                waiting_on: None,
                recovery_abandoned: false,
                dormancy: plakous_host_protocol::views::NodeDormancy::default(),
            }],
        }),
        JsonlCorrelation::default(),
        None,
    )
    .unwrap();
    assert!(required_stdout_kind(&tree));
    assert_eq!(object_from_line(&tree)["tree_id"], "tree-1");
}

/// A JSONL client sees the same observations the canvas
/// does. An absent key means the host does not know — never a default that
/// would tell a client a node is `read_only` when the host held nothing.
#[test]
fn tree_rows_carry_the_tier_attempt_and_candidate_or_omit_them() {
    let mut child = NodeView {
        agent_id: "tree-1-c1".into(),
        parent_agent_id: Some("tree-1".into()),
        depth: 1,
        status: NodeStatus::Sealing,
        attempt_status: Some(AttemptStatus::Running),
        attempt_seq: Some(2),
        attempt_id: None,
        attempts: vec![1, 2],
        tier: Some(Tier::Mutate),
        description: "implement".into(),
        prompt: "do it".into(),
        workspace_path: "/tmp/c1".into(),
        workspace_mode: WorkspaceMode::Fork,
        workspace_status: Some(WorkspaceStatus::Adopting),
        candidate_id: Some(CandidateId::from_persisted("cand-0f3a".to_owned())),
        blocked: Some(plakous_host_protocol::views::BlockedObservation {
            quota: "weekly".into(),
            account: "codex".into(),
            resets_at_unix: Some(1_700_000_000),
        }),
        usage: None,
        active_turn: true,
        live_progress: None,
        unread_mail: 0,
        has_session: true,
        template: Some("implement".into()),
        model_account: "codex".into(),
        model_tuple: "gpt-5.6-luna".into(),
        model_effort: Some("low".into()),
        turns: Some(3),
        started_at_unix: None,
        ended_at_unix: None,
        fault: None,
        waiting_on: None,
        recovery_abandoned: false,
        dormancy: plakous_host_protocol::views::NodeDormancy::default(),
    };
    let snapshot = |node: NodeView| {
        let line = project_host_event(
            &HostEvent::TopologyNavigatorSnapshot(TreeView {
                tree_id: "tree-1".into(),
                revision: 4,
                state: TreeAdmissionState::Live,
                solicited: false,
                nodes: vec![node],
            }),
            JsonlCorrelation::default(),
            None,
        )
        .unwrap();
        object_from_line(&line)
    };
    let object = snapshot(child.clone());
    let row = &object["nodes"][0];
    assert_eq!(row["account"], "codex");
    assert_eq!(row["status"], "sealing");
    assert_eq!(row["tier"], "mutate");
    assert_eq!(row["template"], "implement");
    assert_eq!(row["attempt"], "running");
    assert_eq!(row["attempt_seq"], 2);
    assert_eq!(row["workspace_mode"], "fork");
    assert_eq!(row["workspace_status"], "adopting");
    assert_eq!(row["candidate"], "cand-0f3a");
    assert_eq!(row["blocked"]["quota"], "weekly");
    assert_eq!(row["blocked"]["account"], "codex");
    assert_eq!(row["blocked"]["resets_at"], 1_700_000_000_u64);

    child.model_account.clear();
    child.tier = None;
    child.attempt_status = None;
    child.attempt_seq = None;
    child.candidate_id = None;
    child.blocked = None;
    child.workspace_status = None;
    child.template = None;
    let object = snapshot(child);
    let row = object["nodes"][0].as_object().unwrap();
    for key in [
        "account",
        "tier",
        "attempt",
        "attempt_seq",
        "candidate",
        "blocked",
        "workspace_status",
        "template",
    ] {
        assert!(
            !row.contains_key(key),
            "`{key}` must be omitted, not defaulted"
        );
    }
    // The mode is always known: it falls back to `inherit` in the host
    // projection, so it is a value rather than an omission.
    assert_eq!(row["workspace_mode"], "fork");
}

#[test]
fn hello_ready_bye_are_stable() {
    let hello = hello_line(
        None,
        "sess",
        "tree",
        Some("grok"),
        Some("grok-4.6"),
        Some("low"),
        "native-ptc",
        "live",
        Some("bundled"),
        Some("aarch64-apple-darwin"),
        Some("bundled"),
        Some("aarch64-apple-darwin"),
        Some(&crate::build_features::compiled_features()),
        None,
    );
    let object = object_from_line(&hello);
    assert_eq!(object["v"], 1);
    assert_eq!(object["kind"], "hello");
    assert_eq!(
        object["capabilities"],
        serde_json::json!(crate::build_features::compiled_features())
    );
    assert_eq!(object["session_id"], "sess");
    assert_eq!(object["tree_id"], "tree");
    assert_eq!(object["composition"], "native-ptc");
    assert_eq!(object["tree_state"], "live");
    assert_eq!(object["javascript_host"], "bundled");
    assert_eq!(object["javascript_host_target"], "aarch64-apple-darwin");
    assert_eq!(object["python_host"], "bundled");
    assert_eq!(object["python_host_target"], "aarch64-apple-darwin");
    assert!(!object.contains_key("elapsed_ms"));
    assert!(required_stdout_kind(&hello));
    assert!(required_stdout_kind(&ready_line(Some("p1"), None)));
    assert!(required_stdout_kind(&bye_line(None)));
}

#[cfg(all(
    feature = "anthropic",
    feature = "claude",
    feature = "clipboard",
    feature = "codex",
    feature = "grok",
    feature = "images",
    feature = "js",
    feature = "mcp",
    feature = "openai",
    feature = "openrouter",
    feature = "python",
    feature = "tui",
    feature = "web-fetch",
    feature = "xai-api"
))]
mod default_build {
    use super::*;

    #[test]
    fn hello_capabilities_pin_the_exact_default_wire_vocabulary() {
        let hello = hello_line(
            None,
            "session",
            "tree",
            None,
            None,
            None,
            "direct",
            "live",
            None,
            None,
            None,
            None,
            Some(&crate::build_features::compiled_features()),
            None,
        );
        assert_eq!(
            object_from_line(&hello)["capabilities"],
            serde_json::json!([
                "anthropic",
                "claude",
                "clipboard",
                "codex",
                "grok",
                "images",
                "js",
                "mcp",
                "openai",
                "openrouter",
                "python",
                "tui",
                "web-fetch",
                "xai-api",
            ])
        );
    }
}

#[test]
fn hello_ready_elapsed_ms_serializes_when_some() {
    let hello = hello_line(
        None,
        "sess",
        "tree",
        None,
        None,
        None,
        "native-ptc",
        "live",
        None,
        None,
        None,
        None,
        None,
        Some(12),
    );
    assert_eq!(object_from_line(&hello)["elapsed_ms"], 12);
    assert_eq!(
        object_from_line(&ready_line(None, Some(12)))["elapsed_ms"],
        12
    );
}

#[test]
fn optional_events_project_and_are_not_required_stdout() {
    use plakous_host_protocol::QueueItem;

    let status = project_host_event(
        &HostEvent::Status {
            phase: HostPhase::Running,
            queue_len: 2,
            detail: Some(HostStatusDetail::PlanDiagnostic {
                diagnostic: DiagnosticText::new("tool"),
            }),
        },
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("status projects");
    let status_obj = object_from_line(&status);
    assert_eq!(status_obj["kind"], "status");
    assert_eq!(status_obj["phase"], "running");
    assert_eq!(status_obj["queue_len"], 2);
    assert_eq!(status_obj["detail"], "tool");
    assert!(!required_stdout_kind(&status));

    let note = project_host_event(
        &HostEvent::Row(
            plakous_host_protocol::views::transcript::TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "compaction not needed: the selected prefix already fits",
            ),
        ),
        JsonlCorrelation {
            prompt_id: None,
            omit_unknown_turn_id: false,
            command_id: Some("c1"),
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("system notes project as optional status");
    let note_obj = object_from_line(&note);
    assert_eq!(note_obj["kind"], "status");
    assert_eq!(note_obj["phase"], "note");
    assert_eq!(
        note_obj["detail"],
        "compaction not needed: the selected prefix already fits"
    );
    assert!(!required_stdout_kind(&note));

    let busy = project_host_event(&HostEvent::SetBusy(true), JsonlCorrelation::default(), None)
        .expect("busy projects");
    let busy_obj = object_from_line(&busy);
    assert_eq!(busy_obj["kind"], "busy");
    assert_eq!(busy_obj["busy"], true);
    assert!(!required_stdout_kind(&busy));

    let assistant = project_host_event(
        &HostEvent::AssistantDelta("hello".into()),
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("assistant projects");
    let assistant_obj = object_from_line(&assistant);
    assert_eq!(assistant_obj["kind"], "assistant");
    assert_eq!(assistant_obj["text"], "hello");
    assert!(!required_stdout_kind(&assistant));

    let queue = project_host_event(
        &HostEvent::QueueSnapshot {
            items: vec![QueueItem {
                rejected: false,
                id: "q1".into(),
                revision: 4,
                text: "follow up".into(),
                images: Vec::new(),
            }],
            drain: QueueDrainState::RestoredPaused,
        },
        JsonlCorrelation::default(),
        None,
    )
    .expect("queue projects");
    let queue_obj = object_from_line(&queue);
    assert_eq!(queue_obj["kind"], "queue");
    assert_eq!(queue_obj["drain"], "restored_paused");
    let items = queue_obj["items"].as_array().expect("queue items");
    assert_eq!(items[0]["id"], "q1");
    assert_eq!(items[0]["revision"], 4);
    assert_eq!(items[0]["text"], "follow up");
    assert_eq!(items[0]["rejected"], false);
    assert!(!required_stdout_kind(&queue));
}

/// additive v1 kinds and `tool` fields; `SCHEMA_V` stays 1.
///
/// A v1 client that ignores unknown kinds and fields is unaffected, and no
/// projected line carries a runtime decision address.
#[test]
fn projections_are_additive_v1_lines() {
    let correlation = JsonlCorrelation {
        prompt_id: Some("p1"),
        omit_unknown_turn_id: false,
        command_id: None,
        pending_tree_snapshot_id: None,
    };
    assert_eq!(SCHEMA_V, 1);

    let compose = object_from_line(
        &project_host_event(
            &HostEvent::ToolLine {
                call_id: Some("outer".into()),
                parent_call_id: None,
                cell_id: Some("cell-1".into()),
                kind: ActivityKind::Compose,
                name: "exec_js".into(),
                state: "running".into(),
                subject: Some("3 tools · 1 failed".into()),
                detail: None,
                duration_ms: None,
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            correlation,
            None,
        )
        .unwrap(),
    );
    assert_eq!(compose["kind"], "tool");
    assert_eq!(compose["activity_kind"], "compose");
    assert_eq!(compose["cell_id"], "cell-1");
    assert_eq!(compose["subject"], "3 tools · 1 failed");

    let child = object_from_line(
        &project_host_event(
            &HostEvent::ToolLine {
                call_id: Some("child".into()),
                parent_call_id: Some("outer".into()),
                cell_id: Some("cell-1".into()),
                kind: ActivityKind::Tool,
                name: "read_file".into(),
                state: "done".into(),
                subject: None,
                detail: None,
                duration_ms: Some(7),
                started_at_ms: None,
                finished_at_ms: None,
                wait: None,
            },
            correlation,
            None,
        )
        .unwrap(),
    );
    assert_eq!(child["parent_call_id"], "outer");
    assert_eq!(child["activity_kind"], "tool");
    assert_eq!(child["duration_ms"], 7);

    let reasoning = object_from_line(
        &project_host_event(
            &HostEvent::ReasoningDelta {
                location_key: "0:0".into(),
                text: "weighing".into(),
            },
            correlation,
            None,
        )
        .unwrap(),
    );
    assert_eq!(reasoning["kind"], "reasoning");
    assert_eq!(reasoning["text"], "weighing");

    let approval = object_from_line(
        &project_host_event(
            &HostEvent::ApprovalRequired {
                approval_id: "approval-1".into(),
                call_id: Some("call-1".into()),
                tool_name: "apply_patch".into(),
                subject: None,
                prompt: Some("Allow this write?".into()),
                expires_at_ms: 42,
            },
            correlation,
            None,
        )
        .unwrap(),
    );
    assert_eq!(approval["kind"], "approval");
    assert_eq!(approval["approval_id"], "approval-1");
    // The decision address is never on the wire.
    assert!(!approval.contains_key("decision"));
    assert!(!approval.contains_key("binding_digest"));

    let resolved = object_from_line(
        &project_host_event(
            &HostEvent::ApprovalResolved {
                approval_id: "approval-1".into(),
                resolution: "expired".into(),
            },
            correlation,
            None,
        )
        .unwrap(),
    );
    assert_eq!(resolved["kind"], "approval_resolved");
    assert_eq!(resolved["resolution"], "expired");

    let steer = object_from_line(
        &project_host_event(
            &HostEvent::SteerAck {
                state: "queued".into(),
                position: Some(2),
            },
            correlation,
            None,
        )
        .unwrap(),
    );
    assert_eq!(steer["kind"], "steer_ack");
    assert_eq!(steer["state"], "queued");
    assert_eq!(steer["position"], 2);
}

#[test]
fn live_usage_snapshot_is_not_a_turn_event() {
    let line = project_host_event(
        &HostEvent::TurnUsage {
            input_tokens: 2100,
            output_tokens: 40,
            reasoning_tokens: 12,
            cached_input_tokens: 1800,
            cache_write_tokens: 400,
            model_operations: None,
            reporting_model_operations: None,
            peak_prepared_input_tokens: None,
            terminal: false,
        },
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    );
    assert!(
        line.is_none(),
        "a mid-turn usage snapshot must not read as `turn`"
    );
}

/// The terminal `turn` record carries the growth facts a gate needs.
///
/// `input_tokens` sums over operations, so it cannot say how far a turn
/// grew against the window. The peak is one operation's own prompt and
/// the operation count says how many requests the sum covers. Both are
/// additive: a v1 client ignoring unknown fields is unaffected, and the
/// mid-turn snapshot is still dropped.
#[test]
fn the_terminal_turn_record_carries_the_context_growth_fields() {
    let line = project_host_event(
        &HostEvent::TurnUsage {
            input_tokens: 534_462,
            output_tokens: 40,
            reasoning_tokens: 12,
            cached_input_tokens: 1800,
            cache_write_tokens: 400,
            model_operations: Some(16),
            reporting_model_operations: Some(16),
            peak_prepared_input_tokens: Some(31_500),
            terminal: true,
        },
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("a terminal usage observation projects as `turn`");
    let record: Value = serde_json::from_str(&line).expect("the projection is one JSON object");
    assert_eq!(record["kind"], "turn");
    assert_eq!(record["outcome"], "usage");
    assert_eq!(record["input_tokens"], 534_462, "the sum is unchanged");
    assert_eq!(record["model_operations"], 16);
    assert_eq!(record["peak_prepared_input_tokens"], 31_500);
}

/// An unknown peak is absent from the record rather than reported as zero.
#[test]
fn an_unknown_prepared_input_peak_is_omitted_not_zero() {
    let line = project_host_event(
        &HostEvent::TurnUsage {
            input_tokens: 2100,
            output_tokens: 40,
            reasoning_tokens: 12,
            cached_input_tokens: 1800,
            cache_write_tokens: 400,
            model_operations: None,
            reporting_model_operations: None,
            peak_prepared_input_tokens: None,
            terminal: true,
        },
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("a terminal usage observation projects as `turn`");
    let record: Value = serde_json::from_str(&line).expect("the projection is one JSON object");
    assert!(record.get("peak_prepared_input_tokens").is_none());
    assert!(record.get("model_operations").is_none());
}

#[test]
fn turn_usage_projects_cached_input_as_observation() {
    let line = project_host_event(
        &HostEvent::TurnUsage {
            input_tokens: 2100,
            output_tokens: 40,
            reasoning_tokens: 12,
            cached_input_tokens: 1800,
            cache_write_tokens: 400,
            model_operations: Some(1),
            reporting_model_operations: Some(1),
            peak_prepared_input_tokens: None,
            terminal: true,
        },
        JsonlCorrelation {
            prompt_id: Some("p1"),
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("turn usage projects");
    let object = object_from_line(&line);
    assert_eq!(object["kind"], "turn");
    assert_eq!(object["outcome"], "usage");
    assert_eq!(object["id"], "p1");
    assert_eq!(object["input_tokens"], 2100);
    assert_eq!(object["output_tokens"], 40);
    assert_eq!(object["reasoning_tokens"], 12);
    assert_eq!(object["cached_input_tokens"], 1800);
    assert_eq!(object["cache_write_tokens"], 400);
    assert!(required_stdout_kind(&line));
}

#[test]
fn navigator_unavailable_projects_as_required_error() {
    let error = project_host_event(
        &HostEvent::TopologyNavigatorUnavailable {
            reason: "no topology hub is attached".into(),
        },
        JsonlCorrelation {
            prompt_id: None,
            omit_unknown_turn_id: false,
            command_id: Some("t1"),
            pending_tree_snapshot_id: None,
        },
        None,
    )
    .expect("unavailable projects");
    let object = object_from_line(&error);
    assert_eq!(object["kind"], "error");
    assert_eq!(object["id"], "t1");
    assert_eq!(object["message"], "no topology hub is attached");
    assert!(required_stdout_kind(&error));
}

#[test]
fn jsonl_command_span_fields_are_content_free_allowlist() {
    assert_eq!(
        AgentCommand::COMMAND_SPAN_FIELDS,
        &["kind", "session_id"][..]
    );
    for forbidden in [
        "text",
        "arguments",
        "prompt",
        "message",
        "detail",
        "task",
        "workspace_path",
    ] {
        assert!(
            !AgentCommand::COMMAND_SPAN_FIELDS.contains(&forbidden),
            "forbidden span field `{forbidden}` must not appear on jsonl.command"
        );
    }
}
