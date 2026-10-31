use super::*;

/// Grok's output and kill calls read as the await and the interrupt only
/// when they name a child; process-only calls and other tools keep their
/// names.
#[test]
fn presented_slot_name_reads_grok_calls_naming_a_child_as_the_slot() {
    use serde_json::json;
    for (name, args, expected) in [
        (
            "get_command_or_subagent_output",
            json!({"task_ids": ["p1", "c2"], "timeout_ms": 0}),
            "wait_agent",
        ),
        (
            "get_command_or_subagent_output",
            json!({"task_ids": ["c2"]}),
            "wait_agent",
        ),
        (
            "get_command_or_subagent_output",
            json!({"task_ids": ["p1", "2"]}),
            "get_command_or_subagent_output",
        ),
        (
            "kill_command_or_subagent",
            json!({"task_id": "c2"}),
            "interrupt_agent",
        ),
        (
            "kill_command_or_subagent",
            json!({"task_id": "p3"}),
            "kill_command_or_subagent",
        ),
        ("wait_agent", json!({"agent_ids": ["c2"]}), "wait_agent"),
        ("wait", json!({"ids": ["p1", "c2"]}), "wait_agent"),
        ("wait", json!({"ids": ["p1", "2"]}), "wait"),
        ("wait", json!({}), "wait"),
        ("wait", json!({"cell_id": "c2"}), "wait"),
        ("stop", json!({"id": "c2"}), "interrupt_agent"),
        ("stop", json!({"id": "p2"}), "stop"),
        ("TaskStop", json!({"task_id": "c2"}), "TaskStop"),
    ] {
        assert_eq!(
            presented_slot_name(name, Some(&args), true),
            expected,
            "{args}"
        );
    }
    assert_eq!(
        presented_slot_name("get_command_or_subagent_output", None, true),
        "get_command_or_subagent_output"
    );
    let child = json!({"task_ids": ["c2"]});
    assert_eq!(
        presented_slot_name("get_command_or_subagent_output", Some(&child), false),
        "get_command_or_subagent_output"
    );
    assert!(is_topology_registration("plak.topology.wait_agent.v1"));
    assert!(!is_topology_registration("plakous.process-session-poll.v3"));
    // A bare pi `wait` reads as the await once it settled as one.
    let bare = json!({"timeout_ms": 5000});
    let envelope = json!({"result": "{\"woke_by\":\"mail\",\"agents\":[]}"});
    let raw = json!({"mail": [], "woke_by": "settled", "agents": [{"agent_id": "c1"}]});
    let tasks = json!({"tasks": [{"task_id": "p1", "status": "exited"}]});
    for (name, args, result, topology, expected) in [
        ("wait", &bare, &envelope, true, "wait_agent"),
        ("wait", &bare, &raw, true, "wait_agent"),
        ("wait", &bare, &tasks, true, "wait"),
        ("wait", &bare, &envelope, false, "wait"),
        ("wait", &json!({"ids": ["p1"]}), &envelope, true, "wait"),
        ("wait", &json!({"cell_id": "c2"}), &raw, true, "wait"),
        ("wait_agent", &bare, &tasks, true, "wait_agent"),
        ("exec", &bare, &envelope, true, "exec"),
    ] {
        assert_eq!(
            presented_settled_name(name, Some(args), Some(result), topology),
            expected,
            "{name} {args} {result} {topology}"
        );
    }
    assert_eq!(
        presented_settled_name("wait", None, Some(&envelope), true),
        "wait_agent"
    );
    assert_eq!(presented_settled_name("wait", None, None, true), "wait");
    for (id, process) in [
        ("p1", true),
        ("7", true),
        ("p0", false),
        ("0", false),
        ("c1", false),
    ] {
        assert_eq!(is_process_task_id(id), process, "{id}");
    }
}
#[test]
fn spawn_request_parses_template_and_tier_and_refuses_removed_fields() {
    let parsed = serde_json::from_str::<SpawnAgentRequest>(
        r#"{"template":"smol","description":"r","prompt":"t","output_schema_json":"{}","tier":"execute"}"#,
    )
    .unwrap();
    assert_eq!(parsed.template.as_str(), "smol");
    assert_eq!(parsed.tier, Tier::Execute);
    assert_eq!(parsed.validate(), Ok(()));
    for removed in [
        "class",
        "workspace_mode",
        "workspace_from",
        "workspace_id",
        "write_effect",
    ] {
        let request = format!(
            r#"{{"template":"smol","prompt":"t","output_schema_json":"{{}}","tier":"mutate","{removed}":"x"}}"#
        );
        let error = serde_json::from_str::<SpawnAgentRequest>(&request).unwrap_err();
        assert!(error.to_string().contains(removed), "{removed}");
    }
    for rejected in ["Smol", "1smol", "-smol", "sm_ol", "roster", "agent", ""] {
        let request = format!(
            r#"{{"template":"{rejected}","prompt":"t","output_schema_json":"{{}}","tier":"mutate"}}"#
        );
        let error = serde_json::from_str::<SpawnAgentRequest>(&request).unwrap_err();
        assert!(
            error.to_string().contains("template name"),
            "{rejected}: {error}"
        );
    }
}

#[test]
fn tier_is_required_and_clips_to_the_ceiling() {
    let error = serde_json::from_str::<SpawnAgentRequest>(
        r#"{"template":"smol","prompt":"t","output_schema_json":"{}"}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("tier"));
    assert_eq!(Tier::Mutate.clipped_to(Tier::ReadOnly), Tier::ReadOnly);
    assert_eq!(Tier::ReadOnly.clipped_to(Tier::Mutate), Tier::ReadOnly);
    assert_eq!(Tier::Execute.clipped_to(Tier::Mutate), Tier::Execute);
    assert!(!Tier::ReadOnly.allows_file_writes());
    assert!(!Tier::Execute.allows_file_writes());
    assert!(Tier::Mutate.allows_file_writes());
    assert_eq!(SpawnWorkspace::default(), SpawnWorkspace::Inherit);
}

#[test]
fn inherit_with_candidate_input_is_schema_mismatch() {
    let mut request = SpawnAgentRequest::test_review();
    request.inputs = Some(vec!["cand-c1-1".into()]);
    assert_eq!(request.validate(), Err(TopologyErrorCode::SchemaMismatch));
    request.tier = Tier::Execute;
    assert_eq!(request.validate(), Err(TopologyErrorCode::SchemaMismatch));
    request.workspace = SpawnWorkspace::Fork;
    assert_eq!(request.validate(), Ok(()));
    request.inputs = Some(vec![String::new()]);
    assert_eq!(request.validate(), Err(TopologyErrorCode::SchemaMismatch));
    request.inputs = Some((0..=MAX_SPAWN_INPUTS).map(|n| format!("c{n}")).collect());
    assert_eq!(request.validate(), Err(TopologyErrorCode::SchemaMismatch));
}

#[test]
fn omitted_description_deserializes_empty() {
    let req: SpawnAgentRequest = serde_json::from_str(
        r#"{"template":"review","prompt":"t","output_schema_json":"{}","tier":"read_only"}"#,
    )
    .unwrap();
    assert_eq!(req.description, "");
    assert_eq!(req.template.as_str(), "review");
}

#[test]
fn error_codes_serialize_snake_case() {
    for (code, wire) in [
        (TopologyErrorCode::TemplateUnknown, "template_unknown"),
        (
            TopologyErrorCode::TemplateNotSpawnable,
            "template_not_spawnable",
        ),
        (TopologyErrorCode::InputNotSealed, "input_not_sealed"),
        (TopologyErrorCode::NotSealed, "not_sealed"),
        (TopologyErrorCode::TargetChanged, "target_changed"),
        (TopologyErrorCode::MergeConflict, "merge_conflict"),
        (TopologyErrorCode::FollowupNotIdle, "followup_not_idle"),
        (TopologyErrorCode::FollowupClosed, "followup_closed"),
        (
            TopologyErrorCode::FollowupRecoveryRequired,
            "followup_recovery_required",
        ),
        (
            TopologyErrorCode::FollowupWorkspaceGone,
            "followup_workspace_gone",
        ),
        (TopologyErrorCode::MintUnavailable, "mint_unavailable"),
        (
            TopologyErrorCode::WorkspaceRecoveryRequired,
            "workspace_recovery_required",
        ),
        (TopologyErrorCode::Capacity, "capacity"),
        (TopologyErrorCode::StorageLow, "storage_low"),
        (TopologyErrorCode::DepthExhausted, "depth_exhausted"),
        (TopologyErrorCode::AttemptMismatch, "attempt_mismatch"),
        (TopologyErrorCode::NoPendingChildren, "no_pending_children"),
        (TopologyErrorCode::RecoveryLeaseHeld, "recovery_lease_held"),
        (
            TopologyErrorCode::RecoveryVcsAmbiguous,
            "recovery_vcs_ambiguous",
        ),
        (
            TopologyErrorCode::WorkspaceReferenced,
            "workspace_referenced",
        ),
    ] {
        assert_eq!(serde_json::to_value(code).unwrap(), serde_json::json!(wire));
    }
    assert!(serde_json::from_str::<TopologyErrorCode>("\"lease_saturated\"").is_err());
    assert!(serde_json::from_str::<TopologyErrorCode>("\"class_not_pinned\"").is_err());
    assert!(serde_json::from_str::<TopologyErrorCode>("\"merge_not_owner\"").is_err());
}

#[test]
fn account_model_effort_on_spawn_wire_reject() {
    let err = serde_json::from_str::<SpawnAgentRequest>(
        r#"{"template":"review","description":"r","prompt":"t","output_schema_json":"{}","tier":"read_only","account":"grok"}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("account"));
}

#[test]
fn delegate_purpose_is_schema_mismatch() {
    let err = serde_json::from_str::<SendAgentMessageRequest>(
        r#"{"agent_id":"c1","message":"hi","purpose":"delegate"}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("delegate"));
}

#[test]
fn followup_parses_and_result_purpose_is_refused_from_a_model() {
    let followup: SendAgentMessageRequest = serde_json::from_str(
        r#"{"agent_id":"c1","message":"again","purpose":"followup","output_schema_json":"{}"}"#,
    )
    .unwrap();
    assert_eq!(followup.purpose, Some(MessagePurpose::Followup));
    assert_eq!(followup.validate("root"), Ok(()));
    let result = SendAgentMessageRequest {
        refresh: None,
        agent_id: "root".into(),
        message: "done".into(),
        purpose: Some(MessagePurpose::Result),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    };
    assert_eq!(
        result.validate("c1"),
        Err(TopologyErrorCode::SchemaMismatch)
    );
    let schema_on_coordinate = SendAgentMessageRequest {
        refresh: None,
        agent_id: "c1".into(),
        message: "hi".into(),
        purpose: None,
        priority: None,
        in_reply_to: None,
        output_schema_json: Some("{}".into()),
    };
    assert_eq!(
        schema_on_coordinate.validate("root"),
        Err(TopologyErrorCode::SchemaMismatch)
    );
}

#[test]
fn reply_requires_in_reply_to() {
    let req = SendAgentMessageRequest {
        refresh: None,
        agent_id: "p".into(),
        message: "q".into(),
        purpose: Some(MessagePurpose::Reply),
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    };
    assert_eq!(req.validate("c1"), Err(TopologyErrorCode::ReplyInvalid));
    let empty_ref = SendAgentMessageRequest {
        refresh: None,
        agent_id: "c1".into(),
        message: "hi".into(),
        purpose: None,
        priority: None,
        in_reply_to: Some(String::new()),
        output_schema_json: None,
    };
    assert_eq!(empty_ref.validate("root"), Ok(()));
}

#[test]
fn spawn_wires_parent_peer_to_child_and_adopt_follows_may_adopt() {
    let mut parent = TopologyGrant {
        may_spawn: true,
        may_adopt: true,
        max_depth: 1,
        ..TopologyGrant::default()
    };
    let mut child = TopologyGrant::default();
    TopologyGrant::apply_spawn(&mut parent, &mut child, "root", "c1");
    assert!(parent.message_peers.contains(&"c1".to_owned()));
    assert_eq!(child.message_peers, vec!["root"]);
    assert!(!child.may_spawn);
    assert!(!child.may_adopt);
    let root_tools = parent.compiled_tool_names(true, true);
    assert_eq!(
        root_tools,
        [
            "spawn_agent",
            "list_agents",
            "send_agent_message",
            "wait_agent",
            "interrupt_agent",
            "close_agent",
            "recover_agent",
            "adopt_candidate",
            "discard_workspace",
        ]
    );
    parent.may_adopt = false;
    assert!(
        !parent
            .compiled_tool_names(true, true)
            .contains(&"adopt_candidate")
    );
    let child_tools = child.compiled_tool_names(false, false);
    assert_eq!(
        child_tools,
        ["list_agents", "send_agent_message", "submit_result"]
    );
}

#[test]
fn writer_mode_parse_is_closed_to_single_lease() {
    assert_eq!(
        WriterMode::parse("single_lease"),
        Ok(WriterMode::SingleLease)
    );
    assert!(WriterMode::parse("shared_cwd_mutate").is_err());
    assert!(WriterMode::parse("yolo").is_err());
    assert_eq!(WriterMode::SingleLease.as_str(), "single_lease");
}

#[test]
fn wait_defaults_to_five_minutes_and_clamps_to_one_hour() {
    let ok = WaitAgentRequest {
        agent_ids: Some(vec!["a".into()]),
        timeout_ms: Some(7_200_000),
    };
    assert_eq!(ok.validate(), Ok(MAX_WAIT_TIMEOUT_MS));
    let defaulted = WaitAgentRequest {
        agent_ids: None,
        timeout_ms: None,
    };
    assert_eq!(defaulted.validate(), Ok(DEFAULT_WAIT_TIMEOUT_MS));
    let from_float: WaitAgentRequest = serde_json::from_value(serde_json::json!({
        "agent_ids": ["c1"],
        "timeout_ms": 60000.0
    }))
    .expect("whole JSON numbers decode as wait timeout");
    assert_eq!(from_float.timeout_ms, Some(60_000));
    let token: SubmitResultRequest = serde_json::from_value(serde_json::json!({
        "turn_token": 1.0,
        "output_json": "{\"ok\":true}"
    }))
    .expect("whole JSON numbers decode as turn token");
    assert_eq!(token.turn_token, 1);
    assert!(
        serde_json::from_value::<WaitAgentRequest>(serde_json::json!({
            "agent_ids": ["c1"],
            "timeout_ms": 60000.5
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<WaitAgentRequest>(serde_json::json!({
            "agent_ids": ["c1"],
            "timeout_ms": -1
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SubmitResultRequest>(serde_json::json!({
            "turn_token": 1.5,
            "output_json": "{\"ok\":true}"
        }))
        .is_err()
    );
    let dup = WaitAgentRequest {
        agent_ids: Some(vec!["a".into(), "a".into()]),
        timeout_ms: None,
    };
    assert_eq!(dup.validate(), Err(TopologyErrorCode::SchemaMismatch));
    let empty = WaitAgentRequest {
        agent_ids: Some(Vec::new()),
        timeout_ms: None,
    };
    assert_eq!(empty.validate(), Err(TopologyErrorCode::SchemaMismatch));
}

#[test]
fn root_cannot_submit() {
    let req = SubmitResultRequest {
        turn_token: 1,
        output_json: "{}".into(),
    };
    assert_eq!(
        req.validate(true),
        Err(TopologyErrorCode::SubmitRootForbidden)
    );
}

#[test]
fn attempt_id_is_a_portable_agent_id() {
    let id = AttemptId::new("c1", 2);
    assert_eq!(id.as_str(), "c1-a2");
    assert!(
        id.as_str()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    );
    assert_eq!(
        serde_json::to_value(&id).unwrap(),
        serde_json::json!("c1-a2")
    );
    let scoped: AttemptScopedRequest =
        serde_json::from_str(r#"{"agent_id":"c1","expected_attempt_id":"c1-a1"}"#).unwrap();
    assert_eq!(scoped.expected_attempt_id, Some(AttemptId::new("c1", 1)));
}

/// Every variant is pinned: terminal is "can never run again",
/// finished is "no turn running and none without a re-task".
#[test]
fn node_status_classification_is_explicit_per_variant() {
    let table = [
        // (status, is_terminal, is_finished, settles_a_wait)
        (NodeStatus::Pending, false, false, false),
        (NodeStatus::Running, false, false, false),
        (NodeStatus::Paused, false, false, false),
        (NodeStatus::Sealing, false, false, false),
        (NodeStatus::Completed, false, true, true),
        (NodeStatus::Interrupted, false, false, true),
        (NodeStatus::Failed, false, true, true),
        (NodeStatus::Closed, true, true, true),
        (NodeStatus::RecoveryRequired, true, true, true),
    ];
    for (status, terminal, finished, settles) in table {
        assert_eq!(status.is_terminal(), terminal, "{status:?} terminal");
        assert_eq!(status.is_finished(), finished, "{status:?} finished");
        assert_eq!(status.settles_a_wait(), settles, "{status:?} settles");
    }
}

#[test]
fn attempt_status_classification_is_explicit_per_variant() {
    let table = [
        (AttemptStatus::Running, false),
        (AttemptStatus::Submitted, false),
        (AttemptStatus::Completed, true),
        (AttemptStatus::CompletedWithoutSubmission, true),
        (AttemptStatus::Failed, true),
        (AttemptStatus::Cancelled, true),
        (AttemptStatus::RecoveryRequired, true),
        (AttemptStatus::Closed, true),
        (AttemptStatus::Blocked, true),
        (AttemptStatus::Adopted, true),
    ];
    for (status, stopped) in table {
        assert_eq!(status.is_stopped(), stopped, "{status:?}");
    }
    assert!(WorkspaceStatus::Adopting.is_in_flight());
    assert!(!WorkspaceStatus::Adopted.is_in_flight());
}

/// the three node predicates are three readings of one
/// disposition, and this is the table that says which. A status added
/// without a disposition does not compile; a disposition read differently
/// by one predicate fails here.
#[test]
fn every_node_status_has_one_disposition_and_the_predicates_read_it() {
    let table = [
        (NodeStatus::Pending, NodeDisposition::Live),
        (NodeStatus::Running, NodeDisposition::Live),
        (NodeStatus::Paused, NodeDisposition::Live),
        (NodeStatus::Sealing, NodeDisposition::Sealing),
        (NodeStatus::Completed, NodeDisposition::Idle),
        (NodeStatus::Failed, NodeDisposition::Idle),
        (NodeStatus::Interrupted, NodeDisposition::Interrupted),
        (NodeStatus::Closed, NodeDisposition::Dead),
        (NodeStatus::RecoveryRequired, NodeDisposition::Dead),
    ];
    for (status, disposition) in table {
        assert_eq!(status.disposition(), disposition, "{status:?}");
        assert_eq!(
            status.is_terminal(),
            disposition == NodeDisposition::Dead,
            "{status:?} terminal"
        );
        assert_eq!(
            status.is_finished(),
            matches!(disposition, NodeDisposition::Idle | NodeDisposition::Dead),
            "{status:?} finished"
        );
        assert_eq!(
            status.settles_a_wait(),
            matches!(
                disposition,
                NodeDisposition::Idle | NodeDisposition::Dead | NodeDisposition::Interrupted
            ),
            "{status:?} settles a wait"
        );
    }
}

/// The disposition's spellings are its serde names, like every other
/// status vocabulary on this wire.
#[test]
fn node_disposition_spellings_round_trip() {
    for disposition in [
        NodeDisposition::Live,
        NodeDisposition::Sealing,
        NodeDisposition::Idle,
        NodeDisposition::Interrupted,
        NodeDisposition::Dead,
    ] {
        let encoded = serde_json::to_value(disposition).unwrap();
        assert_eq!(encoded, serde_json::Value::from(disposition.as_str()));
        assert_eq!(
            serde_json::from_value::<NodeDisposition>(encoded).unwrap(),
            disposition
        );
    }
}

#[test]
fn working_set_excludes_the_caller_and_bounds_its_size() {
    let set = WorkingSet::parse(["a", "b", "c"]).unwrap();
    assert_eq!(
        set.iter_excluding(Some("b"))
            .map(TemplateName::as_str)
            .collect::<Vec<_>>(),
        ["a", "c"]
    );
    assert_eq!(set.iter_excluding(None).count(), 3);
    assert!(WorkingSet::parse(["a", "a"]).is_err());
    assert!(WorkingSet::parse(["a", "b", "c", "d", "e", "f"]).is_err());
    assert!(WorkingSet::parse(["roster"]).is_err());
    assert!(WorkingSet::parse(Vec::<&str>::new()).unwrap().is_empty());
}

/// The operator `u` gate: only a non-root failed or recovery-required child
/// that still has an attempt, on a live, interrupted, or terminal-cleanup tree.
/// A paused or closed tree is not eligible. Removing the tree-state guard
/// accepts `restored_paused` and `closed`.
#[test]
fn operator_child_recover_eligible_matches_tree_state_and_node_status() {
    let trees = [
        TreeAdmissionState::Live,
        TreeAdmissionState::Interrupted,
        TreeAdmissionState::TreeTerminal,
        TreeAdmissionState::RestoredPaused,
        TreeAdmissionState::Closed,
    ];
    let statuses = [
        NodeStatus::Pending,
        NodeStatus::Running,
        NodeStatus::Paused,
        NodeStatus::Sealing,
        NodeStatus::Completed,
        NodeStatus::Interrupted,
        NodeStatus::Failed,
        NodeStatus::Closed,
        NodeStatus::RecoveryRequired,
    ];
    for tree in trees {
        for status in statuses {
            let eligible =
                operator_child_recover_eligible(tree.as_str(), status.as_str(), false, true);
            let expect = matches!(
                tree,
                TreeAdmissionState::Live
                    | TreeAdmissionState::Interrupted
                    | TreeAdmissionState::TreeTerminal
            ) && matches!(status, NodeStatus::Failed | NodeStatus::RecoveryRequired);
            assert_eq!(eligible, expect, "{} / {}", tree.as_str(), status.as_str());
        }
    }
    assert!(!operator_child_recover_eligible(
        "live", "failed", true, true
    ));
    assert!(!operator_child_recover_eligible(
        "live",
        "recovery_required",
        false,
        false
    ));
}
