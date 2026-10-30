use crate::tree::test_support::*;
use crate::tree::vcs_fixtures::*;
use crate::tree::*;

/// Turn waiter registrations count per child, change no revision, and
/// end only when every waiter left.
#[test]
fn turn_waiters_are_counted_per_child_without_a_revision() {
    let (_root, _vcs, mut tree) = fake_tree();
    let revision = tree.revision;
    tree.enter_turn_waiter("c1");
    tree.enter_turn_waiter("c1");
    assert!(tree.has_turn_waiter("c1"));
    assert!(!tree.has_turn_waiter("c2"));
    tree.leave_turn_waiter("c1");
    assert!(tree.has_turn_waiter("c1"), "one waiter is still running");
    tree.leave_turn_waiter("c1");
    tree.leave_turn_waiter("c1");
    assert!(!tree.has_turn_waiter("c1"));
    assert_eq!(tree.revision, revision);
}

/// between `spawn_reserve` and its commit the mint is
/// running with the tree mutex released, and no node, attempt, grant, or
/// workspace row names the reserved id. A close or a wait that races the
/// mint therefore gets exactly the answer the id gets before it is ever
/// spawned, and a second spawn cannot be handed the same copy.
#[test]
fn a_tool_racing_the_mint_answers_as_for_an_unspawned_id() {
    let (_root, vcs, mut tree) = fake_tree();
    let SpawnAdmission::Reserved(reservation, Some(ticket)) = tree
        .spawn_reserve("root", &mint_req(), Some("op-mint"))
        .unwrap()
    else {
        unreachable!("a mutate spawn reserves a mint");
    };
    let reserved = reservation.agent_id.clone();
    let unknown = tree.node(&reserved).unwrap_err();
    assert_eq!(unknown.code, TopologyErrorCode::GrantDenied);
    assert_eq!(
        tree.close_agent("root", &close_req(&reserved), Some("op-close"))
            .unwrap_err()
            .code,
        unknown.code
    );
    assert_eq!(
        tree.poll_wait("root", &wait_for(&reserved))
            .unwrap_err()
            .code,
        unknown.code
    );
    let SpawnAdmission::Reserved(second, _) = tree
        .spawn_reserve("root", &mint_req(), Some("op-mint-2"))
        .unwrap()
    else {
        unreachable!("a mutate spawn reserves a mint");
    };
    assert_ne!(
        second.agent_id, reserved,
        "a reserved id is not offered twice"
    );
    let minted = ticket.mint(vcs.as_ref()).unwrap();
    let output = tree.spawn_commit(*reservation, Some(minted)).unwrap();
    assert_eq!(output.agent_id, reserved);
    assert_eq!(
        tree.node(&reserved).unwrap().status,
        NodeStatus::Running,
        "the rows land when the mint answers"
    );
}

/// a mint that refuses gives the reservation back. The
/// ledger closes on the refusal, and a wait for the reserved id answers
/// as for an id that was never spawned — there is nothing to wait for.
#[test]
fn a_wait_after_a_failed_mint_answers_as_for_an_unspawned_id() {
    // No git or jj identity under this launch checkout: the mint refuses.
    let (_tmp, mut tree) = tree();
    let SpawnAdmission::Reserved(reservation, Some(ticket)) = tree
        .spawn_reserve("root", &mint_req(), Some("op-mint"))
        .unwrap()
    else {
        unreachable!("a mutate spawn reserves a mint");
    };
    let reserved = reservation.agent_id.clone();
    let refused = ticket.mint(tree.vcs()).unwrap_err();
    let error = tree.spawn_abort(*reservation, topology_error_from_vcs(refused));
    assert_eq!(error.code, TopologyErrorCode::MintUnavailable);
    assert_eq!(
        tree.operations().get("op-mint").unwrap().terminal_code,
        Some(TopologyErrorCode::MintUnavailable)
    );
    assert_eq!(
        tree.poll_wait("root", &wait_for(&reserved))
            .unwrap_err()
            .code,
        TopologyErrorCode::GrantDenied
    );
    assert_eq!(tree.nodes().len(), 1, "no node is left behind");
    assert!(
        tree.epoch_allows_rebind(),
        "the pending counter is released"
    );
}

#[test]
fn spawn_list_wait_submit_pairs_with_completed() {
    let (_tmp, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn-1"))
        .unwrap();
    assert_eq!(spawned.status, NodeStatus::Running);
    assert_eq!(spawned.tier, Tier::ReadOnly);
    assert_eq!(spawned.attempt_id, AttemptId::new(&spawned.agent_id, 1));
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(true),
                include_completed: Some(false),
            },
        )
        .unwrap();
    assert_eq!(listed.agents.len(), 2);
    tree.submit_result(
        &spawned.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    assert_eq!(
        tree.record_turn_completed(&spawned.agent_id).unwrap(),
        AttemptStatus::Completed
    );
    tree.settle_for_test();
    let wait = tree
        .poll_wait("root", &wait_for(&spawned.agent_id))
        .unwrap()
        .unwrap();
    assert!(!wait.timed_out);
    assert_eq!(wait.agents[0].attempt, AttemptStatus::Completed);
    assert_eq!(
        wait.agents[0].attempt_id,
        Some(AttemptId::new(&spawned.agent_id, 1))
    );
    assert!(wait.agents[0].candidate.is_none());
    let replay = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn-1"))
        .unwrap();
    assert_eq!(replay.agent_id, spawned.agent_id);
}

/// The receipt is limited to the spawn admission facts: nothing about bind identity
/// or paths reaches the model.
#[test]
fn spawn_receipt_carries_attempt_tier_and_workspace_only() {
    let (_tmp, mut tree) = tree();
    tree.set_node_account_for_test("root", "codex");
    pin(&mut tree, "review", "grok", "grok-4", Some("high"));
    let bound = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let node = tree.node(&bound.agent_id).unwrap();
    assert_eq!(node.account, "grok");
    assert_eq!(node.tuple_id, "grok-4");
    assert_eq!(node.effort.as_deref(), Some("high"));
    let wire = serde_json::to_value(&bound).unwrap();
    assert_eq!(
        wire.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["agent_id", "attempt_id", "status", "tier", "workspace"]
    );
    assert_eq!(wire["tier"], "read_only");
    assert_eq!(wire["workspace"]["mode"], "inherit");
    assert_eq!(wire["workspace"]["id"], node.workspace_id);
    assert!(wire["workspace"]["base"].is_null());
}

#[test]
fn spawn_refuses_a_card_without_its_own_account_or_model() {
    let (_tmp, mut tree) = tree();
    tree.set_node_account_for_test("root", "codex");
    pin(&mut tree, "review", "", "selected-tuple", None);
    let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
    assert!(
        error.message.contains("names no account"),
        "{}",
        error.message
    );
    pin(&mut tree, "review", "codex", "", None);
    let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
    assert!(
        error.message.contains("names no model"),
        "{}",
        error.message
    );
    assert_eq!(tree.nodes.len(), 1, "a refused card spends no node");
}

#[test]
fn spawn_admits_explicit_model_outside_allowed_tuples() {
    let (_tmp, mut tree) = tree();
    tree.set_node_account_for_test("root", "codex");
    pin(&mut tree, "review", "codex", "not-in-catalog-yet", None);
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().tuple_id,
        "not-in-catalog-yet"
    );
}

/// a tier above `max_tier` clips; the receipt and the node
/// both carry the effective tier.
#[test]
fn tier_above_the_template_ceiling_clips_and_the_receipt_reports_it() {
    let (_tmp, mut tree) = tree();
    let mut ambitious = spawn_req();
    ambitious.tier = Tier::Mutate;
    let spawned = tree.spawn_agent("root", &ambitious, None).unwrap();
    assert_eq!(spawned.tier, Tier::ReadOnly);
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
    assert_eq!(tree.node(&spawned.agent_id).unwrap().tier, Tier::ReadOnly);
    // Below the ceiling is honoured as asked.
    let modest = spawn_req();
    let spawned = tree.spawn_agent("root", &modest, None).unwrap();
    assert_eq!(spawned.tier, Tier::ReadOnly);
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Inherit);
}

#[test]
fn request_key_is_recorded_on_the_attempt() {
    let (_tmp, mut tree) = tree();
    let mut request = spawn_req();
    request.request_key = Some("first-review".into());
    let spawned = tree.spawn_agent("root", &request, None).unwrap();
    let attempt = tree.attempt(&spawned.agent_id).unwrap();
    assert_eq!(attempt.request_key.as_deref(), Some("first-review"));
    assert_eq!(attempt.attempt_id, spawned.attempt_id);
    assert!(attempt.started_at > 0);
    assert_eq!(attempt.ended_at, None);
    assert_eq!(attempt.usage, UsageRow::default());
}

/// Two independent calls carrying one `request_key` are
/// one spawn: the `call_id` digest cannot see across calls, so without the
/// key a retried tool loop opens a second child on the same brief.
#[test]
fn request_key_dedups_two_independent_spawns() {
    let (_tmp, mut tree) = tree();
    let mut request = spawn_req();
    request.request_key = Some("review-parser".into());
    let first = tree.spawn_agent("root", &request, Some("call-1")).unwrap();
    let nodes_after_first = tree.nodes().len();
    let second = tree.spawn_agent("root", &request, Some("call-2")).unwrap();
    assert_eq!(first, second, "the same key returns the same receipt");
    assert_eq!(
        tree.nodes().len(),
        nodes_after_first,
        "no second node is created"
    );
    assert_eq!(
        tree.attempts()
            .values()
            .filter(|attempt| attempt.request_key.as_deref() == Some("review-parser"))
            .count(),
        1
    );
}

#[test]
fn request_key_identity_survives_changed_workspace_and_followup_brief() {
    let (_tmp, mut tree) = tree();
    let mut request = spawn_req();
    request.request_key = Some("immutable-request".into());
    let first = tree.spawn_agent("root", &request, None).unwrap();
    let workspace_id = tree.node(&first.agent_id).unwrap().workspace_id.clone();
    tree.workspaces.get_mut(&workspace_id).unwrap().base_commit =
        Some("jj-working-copy-rewrite".into());
    let node = tree.nodes.get_mut(&first.agent_id).unwrap();
    node.prompt = "later followup".into();
    node.schema_digest = "later-schema".into();
    let before = tree.nodes().len();
    let replay = tree.spawn_agent("root", &request, None).unwrap();
    assert_eq!(replay.agent_id, first.agent_id);
    assert_eq!(tree.nodes().len(), before);
    let mut changed = request.clone();
    changed.prompt = "later followup".into();
    assert_eq!(
        tree.spawn_agent("root", &changed, None).unwrap_err().code,
        TopologyErrorCode::RequestKeyConflict
    );
}

#[test]
fn keyed_spawn_without_admitted_identity_requires_recovery() {
    let (_tmp, mut tree) = tree();
    let mut request = spawn_req();
    request.request_key = Some("incomplete-identity".into());
    let first = tree.spawn_agent("root", &request, None).unwrap();
    tree.attempts
        .get_mut(&first.attempt_id)
        .unwrap()
        .spawn_identity = None;
    assert_eq!(
        tree.spawn_agent("root", &request, None).unwrap_err().code,
        TopologyErrorCode::OperationRecoveryRequired
    );
    assert_eq!(tree.nodes().len(), 2);
}

/// The same key on a different brief is refused, not aliased — the
/// caller asked for two different things and must learn that it did.
#[test]
fn request_key_conflict_is_typed() {
    let (_tmp, mut tree) = tree();
    let mut request = spawn_req();
    request.request_key = Some("review-parser".into());
    tree.spawn_agent("root", &request, Some("call-1")).unwrap();
    let mut conflicting = request.clone();
    conflicting.prompt = "review something else entirely".into();
    let error = tree
        .spawn_agent("root", &conflicting, Some("call-2"))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
    assert!(error.message.contains("review-parser"), "{}", error.message);
    assert_eq!(tree.nodes().len(), 2, "the conflict spawns nothing");
}

#[test]
fn request_key_conflict_includes_workspace() {
    let (_tmp, mut tree) = tree();
    let mut request = spawn_req();
    request.request_key = Some("review-parser".into());
    tree.spawn_agent("root", &request, Some("call-1")).unwrap();
    let mut conflicting = request.clone();
    conflicting.workspace = SpawnWorkspace::Fork;
    let error = tree
        .spawn_agent("root", &conflicting, Some("call-2"))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::RequestKeyConflict);
    assert_eq!(tree.nodes().len(), 2, "the conflict spawns nothing");
}

/// B1: `max_attempts` is a per-tree cap the tree may only
/// lower. A raise is ignored rather than accepted and silently clipped.
#[test]
fn retries_consume_max_attempts() {
    let (_tmp, mut tree) = tree();
    tree.lower_max_attempts(2);
    assert_eq!(tree.max_attempts(), 2);
    tree.lower_max_attempts(MAX_TREE_ATTEMPTS * 2);
    assert_eq!(tree.max_attempts(), 2, "the ceiling only ever lowers");
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let error = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::SpawnBudget);
    assert!(
        error.message.contains("attempt budget"),
        "{}",
        error.message
    );
}

#[test]
fn submit_does_not_adopt_and_only_copy_cannot_be_destroyed() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.submit_result(
        &spawned.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    assert_eq!(tree.node("root").unwrap().path, tree.launch_checkout,);
    let inherit_ws = tree.node("root").unwrap().workspace_id.clone();
    assert_eq!(
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: inherit_ws,
            },
            None,
        )
        .unwrap_err()
        .code,
        TopologyErrorCode::DiscardNotApplicable
    );
    tree.settle_for_test();
}

/// `followup` re-tasks an idle finished child on a **new**
/// attempt: one assignment row per node could not hold two attempts'
/// results without losing the first, which is why the attempt is the unit.
#[test]
fn followup_reruns_the_node_with_a_new_attempt_and_keeps_the_first_output() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.submit_result(
        &spawned.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    let first_attempt = spawned.attempt_id.clone();
    let first_token = tree.attempts().get(&first_attempt).unwrap().turn_token;

    let mut request = mail(&spawned.agent_id, "also cover the error paths");
    request.purpose = Some(MessagePurpose::Followup);
    request.output_schema_json = Some(
        r#"{"type":"object","properties":{"notes":{"type":"string"}},"required":["notes"]}"#.into(),
    );
    let sent = tree
        .send_agent_message("root", &request, None)
        .expect("followup is admitted on an idle finished child");
    assert_eq!(sent.disposition, MailDisposition::Started);

    let node = tree.node(&spawned.agent_id).unwrap();
    assert_eq!(node.status, NodeStatus::Running);
    let second_attempt = node.attempt_id.clone().unwrap();
    assert_eq!(
        second_attempt.as_str(),
        format!("{}-a2", spawned.agent_id),
        "the second attempt is a2 on the same node"
    );
    assert!(
        node.output_schema_json.contains("notes"),
        "the new attempt carries the new schema"
    );

    let second = tree.attempts().get(&second_attempt).unwrap();
    assert_eq!(second.status, AttemptStatus::Running);
    assert_eq!(second.retry_of.as_ref(), Some(&first_attempt));
    assert!(second.turn_token > first_token, "the turn token rotates");
    assert_eq!(second.output_json, None);

    let first = tree.attempts().get(&first_attempt).unwrap();
    assert_eq!(
        first.output_json.as_deref(),
        Some(r#"{"ok":true}"#),
        "attempt 1 keeps its own result"
    );
    assert_eq!(first.status, AttemptStatus::Completed);

    // The rotated token fences a late submission against the old attempt.
    assert_eq!(
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: first_token,
                output_json: r#"{"notes":"late"}"#.into(),
            },
            None,
        )
        .unwrap_err()
        .code,
        TopologyErrorCode::SubmitStaleToken
    );
}

/// A running child is steered, not re-tasked; a closed one can never run
/// again. Distinct codes identify a temporary busy state
/// versus permanent closure without requiring prose parsing.
#[test]
fn followup_rejected_for_running_and_closed() {
    let (_tmp, mut tree) = tree();
    let running = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let mut request = mail(&running.agent_id, "again");
    request.purpose = Some(MessagePurpose::Followup);
    let error = tree.send_agent_message("root", &request, None).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::FollowupNotIdle);
    assert!(error.message.contains("urgent"), "{}", error.message);

    let closed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.close_agent("root", &close_req(&closed.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    request.agent_id = closed.agent_id;
    assert_eq!(
        tree.send_agent_message("root", &request, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::FollowupClosed
    );

    // `result` is host-authored and never arrives from a model.
    let idle = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&idle.agent_id).unwrap();
    tree.settle_for_test();
    request.agent_id = idle.agent_id;
    request.purpose = Some(MessagePurpose::Result);
    assert_eq!(
        tree.send_agent_message("root", &request, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SchemaMismatch
    );
}

#[test]
fn completed_without_submission_is_distinct() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::Running
    );
    tree.submit_result(
        &spawned.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    assert_eq!(
        tree.record_turn_completed(&spawned.agent_id).unwrap(),
        AttemptStatus::Completed
    );
    tree.settle_for_test();
    assert!(tree.attempt(&spawned.agent_id).unwrap().ended_at.is_some());
    let missed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.record_turn_completed(&missed.agent_id).unwrap(),
        AttemptStatus::CompletedWithoutSubmission
    );
    tree.settle_for_test();
    assert_eq!(
        tree.submit_result(
            &missed.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap_err()
        .code,
        TopologyErrorCode::RecipientAttemptTerminal
    );
}

#[test]
fn fail_session_materialization_terminals_attempt_and_frees_the_fork() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let spawned = tree.spawn_agent("root", &mint_req(), None).unwrap();
    assert_eq!(spawned.workspace.mode, WorkspaceMode::Fork);
    tree.record_host_materialization(&spawned.agent_id)
        .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::RecoveryRequired
    );
    let discarded = tree
        .discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: spawned.workspace.id.clone(),
            },
            None,
        )
        .unwrap();
    assert_eq!(discarded.status, WorkspaceStatus::Discarding);
    tree.settle_for_test();
    assert_eq!(
        tree.workspace(&spawned.workspace.id).unwrap().status,
        WorkspaceStatus::Discarded
    );
}

#[test]
fn fail_session_materialization_completes_submitted_attempt() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.submit_result(
        &spawned.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_host_materialization(&spawned.agent_id)
        .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::Completed
    );
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Completed
    );
}

/// the 1+1 budget is a ceiling, so a count past it is
/// refused rather than clamped into looking admitted. Clamping is what a
/// `u8` field invited: `min(2)` turned a caller that had lost count into
/// a caller that looked exhausted, which reads the same as a child that
/// was actually asked twice.
#[test]
fn the_submit_harness_budget_refuses_a_count_past_its_ceiling() {
    assert_eq!(SubmitNudgeBudget::NONE.rounds(), 0);
    assert!(!SubmitNudgeBudget::NONE.is_exhausted());
    let one = one_round();
    assert_eq!(one.rounds(), 1);
    assert!(!one.is_exhausted());
    let two = one.next().expect("the second ask is admitted");
    assert_eq!(two, two_rounds());
    assert!(two.is_exhausted());
    assert!(two.next().is_none(), "there is no third ask");
    assert!(SubmitNudgeBudget::spent(SubmitNudgeBudget::CEILING + 1).is_none());
    assert!(SubmitNudgeBudget::spent(u8::MAX).is_none());
}

#[test]
fn submit_harness_round_is_persisted_on_the_attempt() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_submit_harness_round(&spawned.agent_id, one_round())
        .unwrap();
    assert_eq!(
        tree.attempt(&spawned.agent_id)
            .unwrap()
            .submit_harness_nudge,
        one_round()
    );
    tree.record_submit_harness_round(&spawned.agent_id, two_rounds())
        .unwrap();
    assert_eq!(
        tree.attempt(&spawned.agent_id)
            .unwrap()
            .submit_harness_nudge,
        two_rounds()
    );
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.record_submit_harness_round(&spawned.agent_id, one_round())
            .unwrap_err()
            .code,
        TopologyErrorCode::RecipientAttemptTerminal
    );
}

#[test]
fn restored_paused_blocks_spawn_and_mail() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.restore_paused();
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TreeRestoredPaused
    );
    assert_eq!(
        tree.send_agent_message("root", &mail(&spawned.agent_id, "hi"), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TreeRestoredPaused
    );
    assert_eq!(
        tree.discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: spawned.workspace.id,
            },
            None,
        )
        .unwrap_err()
        .code,
        TopologyErrorCode::TreeRestoredPaused
    );
    tree.settle_for_test();
    tree.resume_tree().unwrap();
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
}

/// Spawn authority is the caller's own menu: the root's is the working
/// set, a child's is its card's `spawns` list. A template frozen into the
/// closure only for a nested card is not the root's to spawn, and a
/// supervisor cannot reach a working-set template its card does not name.
#[test]
fn a_spawn_must_name_a_template_on_the_callers_own_menu() {
    let (_tmp, mut tree) = tree();
    tree.grants.get_mut("root").unwrap().max_depth = 3;
    let name = |name: &str| TemplateName::parse(name).unwrap();
    let mut nested = RosterBinding::leaf(crate::epoch::RosterInput::TEST_ACCOUNT, "selected-tuple");
    nested.template = "nested".to_owned();
    tree.snapshot.roster.insert(name("nested"), nested);
    tree.snapshot
        .roster
        .get_mut(&name("orchestrate"))
        .unwrap()
        .spawns = vec![name("nested")];
    tree.snapshot.digest = tree.snapshot.compute_digest();
    let nested_request = SpawnAgentRequest::test_named("nested", Tier::Mutate, "nested", "do it");
    assert_eq!(
        tree.spawn_agent("root", &nested_request, None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TemplateNotSpawnable,
        "the root spawns the working set only"
    );
    let supervisor = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    assert_eq!(
        tree.spawn_agent(&supervisor.agent_id, &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TemplateNotSpawnable,
        "review is in the working set but not on the supervisor's card"
    );
    let child = tree
        .spawn_agent(&supervisor.agent_id, &nested_request, None)
        .unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().template,
        Some(name("nested"))
    );
    let grant = tree.grants.get(&child.agent_id).unwrap();
    assert!(!grant.may_spawn && !grant.may_adopt, "nested is a leaf");
    assert!(
        !tree
            .compiled_tool_names(&child.agent_id)
            .unwrap()
            .contains(&"spawn_agent")
    );
}

/// An off-menu card is refused as not spawnable before its account is
/// checked, so an unsupported account is not `schema_mismatch`.
#[test]
fn an_off_menu_template_with_an_unsupported_account_is_not_spawnable() {
    let (_tmp, mut tree) = tree();
    let name = |raw: &str| TemplateName::parse(raw).unwrap();
    let mut nested = RosterBinding::leaf("anthropic", "claude-sonnet-5");
    nested.template = "nested".to_owned();
    tree.snapshot.roster.insert(name("nested"), nested);
    tree.snapshot
        .roster
        .get_mut(&name("orchestrate"))
        .unwrap()
        .spawns = vec![name("nested")];
    tree.snapshot.digest = tree.snapshot.compute_digest();
    let error = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("nested", Tier::Mutate, "nested", "do it"),
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::TemplateNotSpawnable);
}

/// a node's menu excludes its own template.
#[test]
fn a_node_cannot_spawn_its_own_template() {
    let (_tmp, mut tree) = tree();
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let supervisor = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let error = tree
        .spawn_agent(
            &supervisor.agent_id,
            &SpawnAgentRequest::test_orchestrate(),
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::TemplateNotSpawnable);
    // The supervisor's advertised menu agrees with the refusal.
    assert!(
        tree.compiled_tool_names(&supervisor.agent_id)
            .unwrap()
            .contains(&"spawn_agent")
    );
    tree.snapshot
        .roster
        .retain(|name, _| name.as_str() == "orchestrate");
    tree.snapshot.digest = tree.snapshot.compute_digest();
    assert!(
        !tree
            .compiled_tool_names(&supervisor.agent_id)
            .unwrap()
            .contains(&"spawn_agent"),
        "a menu that is only the node's own template is empty"
    );
}

#[test]
fn set_active_turn_refuses_terminal_node_and_attempt() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.bind_session_locator(&spawned.agent_id, "session:c1")
        .unwrap();
    tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
    tree.set_active_turn(&spawned.agent_id, true).unwrap();
    tree.record_host_materialization(&spawned.agent_id)
        .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.set_active_turn(&spawned.agent_id, true)
            .unwrap_err()
            .code,
        TopologyErrorCode::RecipientClosed
    );
    assert_eq!(
        tree.inflight_join_targets(),
        [] as [(String, String, String, String); 0]
    );
    tree.set_active_turn(&spawned.agent_id, false).unwrap();
    // A completed node is idle, not terminal: the refusal
    // there names the finished attempt, which a followup would replace.
    let done = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&done.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.set_active_turn(&done.agent_id, true).unwrap_err().code,
        TopologyErrorCode::RecipientAttemptTerminal
    );
}

#[test]
fn luna_tuple_is_admitted_at_tree_layer_and_delegate_rejects() {
    let (_tmp, mut tree) = tree();
    pin(&mut tree, "review", "codex", "luna", None);
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(tree.node(&spawned.agent_id).unwrap().tuple_id, "luna");
    assert!(
        serde_json::from_str::<SendAgentMessageRequest>(
            r#"{"agent_id":"c1","message":"hi","purpose":"delegate"}"#
        )
        .is_err()
    );
}

#[test]
fn close_retains_rows_and_child_cannot_close_parent() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.close_agent(&spawned.agent_id, &close_req("root"), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::GrantDenied
    );
    tree.settle_for_test();
    let closed = tree
        .close_agent("root", &close_req(&spawned.agent_id), None)
        .unwrap();
    assert_eq!(closed[0].1, NodeStatus::Closed);
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::Closed
    );
    assert!(tree.attempt(&spawned.agent_id).unwrap().ended_at.is_some());
}

/// `expected_attempt_id` fences interrupt and close to the
/// attempt the caller saw.
#[test]
fn stale_expected_attempt_id_refuses_interrupt_and_close() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.set_active_turn(&spawned.agent_id, true).unwrap();
    let stale = AttemptScopedRequest {
        agent_id: spawned.agent_id.clone(),
        expected_attempt_id: Some(AttemptId::new(&spawned.agent_id, 7)),
    };
    assert_eq!(
        tree.interrupt_agent("root", &stale, None).unwrap_err().code,
        TopologyErrorCode::AttemptMismatch
    );
    assert_eq!(
        tree.close_agent("root", &stale, None).unwrap_err().code,
        TopologyErrorCode::AttemptMismatch
    );
    tree.settle_for_test();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Running
    );
    let exact = AttemptScopedRequest {
        agent_id: spawned.agent_id.clone(),
        expected_attempt_id: Some(spawned.attempt_id.clone()),
    };
    assert_eq!(
        tree.interrupt_agent("root", &exact, None).unwrap(),
        NodeStatus::Interrupted
    );
    tree.close_agent("root", &exact, None).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Closed
    );
}

/// The submission outcomes stay distinct. A schema-invalid payload, a
/// stale turn token, an identical resubmit, and a conflicting one are four
/// different answers, not one "submit failed".
#[test]
fn invalid_stale_duplicate_and_conflicting_submissions_are_distinct() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let id = spawned.agent_id.as_str();
    let submit = |output: &str, token: u64| SubmitResultRequest {
        turn_token: token,
        output_json: output.to_owned(),
    };
    assert_eq!(
        tree.submit_result(id, &submit("{ not json", 1), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SchemaMismatch
    );
    assert_eq!(
        tree.submit_result(id, &submit(r#"{"ok":true}"#, 9), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SubmitStaleToken
    );
    let first = tree
        .submit_result(id, &submit(r#"{"ok":true}"#, 1), None)
        .unwrap();
    assert_eq!(first.attempt, AttemptStatus::Submitted);
    assert_eq!(first.candidate, None, "a submission is not a seal");
    let repeat = tree
        .submit_result(id, &submit(r#"{"ok":true}"#, 1), None)
        .unwrap();
    assert_eq!(repeat, first, "an identical resubmit is idempotent");
    assert_eq!(
        tree.submit_result(id, &submit(r#"{"ok":false}"#, 1), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SubmitDuplicateConflict
    );
}

#[test]
fn submit_refuses_a_running_child_attempt() {
    let (_tmp, mut tree) = tree();
    tree.grants_mut_for_test("root").max_depth = 3;
    let hub = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let child = tree.spawn_agent(&hub.agent_id, &spawn_req(), None).unwrap();
    let result = tree.submit_result(
        &hub.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    );
    let error = result.expect_err("running child must refuse submission");
    assert!(error.message.contains(&child.agent_id));
    assert_eq!(serde_json::to_value(error.code).unwrap(), "live_work");
}

#[test]
fn completion_closes_the_live_subtree_without_result_mail() {
    let (_tmp, mut tree) = tree();
    tree.grants_mut_for_test("root").max_depth = 3;
    let hub = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    let child = tree.spawn_agent(&hub.agent_id, &spawn_req(), None).unwrap();
    tree.record_turn_completed(&hub.agent_id).unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(tree.pending_mail(&hub.agent_id), [] as [MailRecord; 0]);
}

/// the template's `max_turns` ends the attempt `blocked` with
/// whatever it produced, rather than letting the submit harness hand it
/// another round.
#[test]
fn every_attempt_end_closes_descendants_including_work_started_after_submit() {
    for ending in [
        "completed",
        "missing_submission",
        "failed",
        "blocked",
        "cancelled",
    ] {
        let (_tmp, mut tree) = tree();
        tree.grants_mut_for_test("root").max_depth = 3;
        let hub = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap();
        if ending == "completed" {
            tree.submit_result(
                &hub.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
        }
        let child = tree.spawn_agent(&hub.agent_id, &spawn_req(), None).unwrap();
        match ending {
            "completed" | "missing_submission" => {
                tree.record_turn_completed(&hub.agent_id).unwrap();
            }
            "failed" => {
                tree.record_turn_failure(
                    &hub.agent_id,
                    turn_outcome::FailedTurnDisposition::Failed { kind: "test" },
                    "failed fixture".into(),
                )
                .unwrap();
            }
            "blocked" => {
                tree.record_attempt_blocked(
                    &hub.agent_id,
                    BlockedReason {
                        quota: "test".into(),
                        account: "test".into(),
                        resets_at: None,
                    },
                )
                .unwrap();
            }
            "cancelled" => {
                tree.record_host_cancellation(&hub.agent_id)
                    .and_then(|value| tree.fold_due(&hub.agent_id).map(|_| value))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        tree.fold_reserved_closes().unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Closed,
            "{ending}"
        );
        assert!(tree.pending_mail(&hub.agent_id).is_empty(), "{ending}");
    }
}

#[test]
fn max_turns_ends_the_attempt_blocked_with_partial_output() {
    let (_tmp, mut tree) = tree();
    let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::ReadOnly;
    binding.max_turns = Some(1);
    tree.pin_template("review", binding);
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert!(!tree.attempt_turns_spent(&spawned.agent_id));
    tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
    // Rebinding the same turn after a reopen is not a second turn.
    tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
    assert_eq!(tree.attempt(&spawned.agent_id).unwrap().usage.turns, 1);
    assert!(tree.attempt_turns_spent(&spawned.agent_id));
    assert_eq!(
        tree.record_submit_harness_round(&spawned.agent_id, one_round())
            .unwrap_err()
            .code,
        TopologyErrorCode::QuotaBlocked
    );
    assert_eq!(
        tree.record_turn_completed(&spawned.agent_id).unwrap(),
        AttemptStatus::Blocked
    );
    tree.settle_for_test();
    let attempt = tree.attempt(&spawned.agent_id).unwrap();
    assert_eq!(attempt.blocked.as_ref().unwrap().quota, "max_turns");
    assert_eq!(attempt.output_json, None, "there was no partial output");
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Failed
    );
}

/// model-visible rows carry per-attempt usage, and only that.
#[test]
fn usage_row_fields_are_populated_per_attempt() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.bind_attempt_turn(&spawned.agent_id, "turn-1").unwrap();
    // What the tree projector folds from the turn's `TurnTerminal`.
    let row = &mut tree.current_attempt_mut(&spawned.agent_id).unwrap().usage;
    row.elapsed_ms = 1_500;
    row.tool_calls = 4;
    row.input_tokens = 900;
    row.output_tokens = 120;
    tree.submit_result(
        &spawned.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();

    let usage = tree.attempt(&spawned.agent_id).unwrap().usage;
    assert_eq!(usage.turns, 1);
    assert_eq!(usage.tool_calls, 4);
    assert_eq!(usage.input_tokens, 900);
    assert_eq!(usage.output_tokens, 120);
    assert!(usage.elapsed_ms >= 1_500);

    let settled = tree
        .poll_wait("root", &wait_for(&spawned.agent_id))
        .unwrap()
        .unwrap();
    assert_eq!(settled.agents[0].usage, Some(usage));
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: None,
                include_completed: Some(true),
            },
        )
        .unwrap();
    assert_eq!(listed.agents[0]["usage"]["tool_calls"], 4);
    assert!(
        listed.agents[0].get("cost").is_none(),
        "cost never reaches a model-visible row"
    );

    // A second attempt starts its own row; usage is never rolled up.
    let mut followup = mail(&spawned.agent_id, "one more pass");
    followup.purpose = Some(MessagePurpose::Followup);
    tree.send_agent_message("root", &followup, None).unwrap();
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().usage,
        UsageRow::default()
    );
}

#[test]
fn leftover_workspace_or_worktree_from_another_tree_does_not_steal_child_id() {
    let (_tmp, mut tree) = tree();
    std::fs::create_dir_all(tree.launch_checkout.join(".plak/worktrees/c1")).unwrap();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(spawned.agent_id, "c1");
    tree.workspaces.insert(
        "ws-c2".into(),
        WorkspaceRecord {
            conflict_base: None,
            resolves: None,
            conflict_target: None,
            conflict_paths: Vec::new(),
            input_candidate_id: None,
            workspace_id: "ws-c2".into(),
            owner_agent_id: "root".into(),
            path: tree.launch_checkout.join(".plak/worktrees/old-tree-c2"),
            mode: WorkspaceMode::Fork,
            status: WorkspaceStatus::Active,
            base_commit: None,
            base_content: None,
            base_change_id: None,
            generation: 0,
            adopter_agent_id: None,
        },
    );
    let next = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(next.agent_id, "c3");
    tree.rollback_minted_workspace("ws-c2");
    tree.settle_for_test();
    assert!(tree.workspace("ws-c2").is_none());
}

#[test]
fn grant_max_nodes_and_pending_spawn_budgets() {
    let (_tmp, mut tree) = tree();
    tree.grants.get_mut("root").unwrap().max_nodes = 1;
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SpawnBudget
    );
    tree.grants.get_mut("root").unwrap().max_nodes = 32;
    tree.pending_spawns = u32::try_from(MAX_PENDING_SPAWNS).unwrap();
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SpawnBudget
    );
}

/// Closed children give their node slot back, so a session can keep
/// delegating past the old lifetime budget of 31 children.
#[test]
fn closed_children_free_node_slots() {
    let (_tmp, mut tree) = tree();
    for _ in 0..40 {
        tree.mark_root_live_for_prompt();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
    }
    tree.grants.get_mut("root").unwrap().max_nodes = 2;
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
    assert!(
        refused.message.contains("2 of 2 live"),
        "{}",
        refused.message
    );
}

/// The attempt budget counts one root prompt; the next prompt starts a
/// fresh count.
#[test]
fn the_attempt_budget_resets_with_each_root_prompt() {
    let (_tmp, mut tree) = tree();
    tree.lower_max_attempts(3);
    tree.mark_root_live_for_prompt();
    for _ in 0..3 {
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.close_agent("root", &close_req(&child.agent_id), None)
            .unwrap();
        tree.settle_for_test();
    }
    let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
    assert!(
        refused.message.contains("this prompt"),
        "{}",
        refused.message
    );
    tree.mark_root_live_for_prompt();
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
}

#[test]
fn depth_exhaustion_is_its_own_code() {
    let (_tmp, mut tree) = tree();
    tree.grants.get_mut("root").unwrap().max_depth = 0;
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::DepthExhausted
    );
}

#[test]
fn spawn_and_submit_admit_portable_schema_v2() {
    let (_tmp, mut tree) = tree();
    let mut req = spawn_req();
    req.output_schema_json = "{}".into();
    assert_eq!(
        tree.spawn_agent("root", &req, None).unwrap_err().code,
        TopologyErrorCode::SchemaMismatch
    );
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":1}"#.into(),
            },
            None,
        )
        .unwrap_err()
        .code,
        TopologyErrorCode::SchemaMismatch
    );
}

/// `output_schema_json` arguments of the shape `codex · gpt-5.6-luna`
/// (native Code Mode) emits: four consecutive `spawn_agent` calls that the
/// old admission refused as `schema_mismatch`.
const LUNA_LIVE_ARGUMENTS: [&str; 4] = [
    r#"{"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}},"raw_output":{"type":"string"}},"required":["files","raw_output"]}"#,
    r#"{"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}}},"required":["files"]}"#,
    r#"{"version":2,"type":"object","properties":{"files":{"type":"array","items":{"type":"string"}}},"required":["files"]}"#,
    "{}",
];

#[test]
fn admits_the_live_luna_output_schema_shapes() {
    // The two benign shapes (a correct Draft 2020-12 object schema with
    // `$schema` and `additionalProperties` omitted) now admit and freeze
    // closed; the two guesses the model made after the content-free
    // refusal still fail closed.
    for raw in [LUNA_LIVE_ARGUMENTS[0], LUNA_LIVE_ARGUMENTS[1]] {
        let (frozen, digest) = admit_output_schema(raw).expect(raw);
        assert!(frozen.contains(OUTPUT_SCHEMA_DIALECT), "{frozen}");
        assert!(
            frozen.contains(r#""additionalProperties":false"#),
            "{frozen}"
        );
        assert_ne!(digest, "");
        // The frozen schema is the contract the child submits against.
        validate_output_json(&frozen, r#"{"files":["a"],"raw_output":"x"}"#)
            .or_else(|_| validate_output_json(&frozen, r#"{"files":["a"]}"#))
            .expect(raw);
    }
    for raw in [LUNA_LIVE_ARGUMENTS[2], LUNA_LIVE_ARGUMENTS[3]] {
        let error = admit_output_schema(raw).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::SchemaMismatch);
        // The refusal carries the closed-dialect repair, not just a verdict.
        assert!(
            error.message.contains("PortableToolSchemaV2")
                && error.message.contains("additionalProperties=false"),
            "{}",
            error.message
        );
    }
}

#[test]
fn output_schema_normalisation_is_bounded_and_truthful() {
    // An explicitly open object is the one shape that cannot be filled in
    // truthfully: a submitted result is validated closed.
    let open = r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":true}"#;
    assert!(
        admit_output_schema(open)
            .unwrap_err()
            .message
            .contains("additionalProperties")
    );
    // A foreign dialect is refused by name rather than silently rewritten.
    let dialect = r#"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"a":{"type":"string"}},"required":["a"]}"#;
    assert!(
        admit_output_schema(dialect)
            .unwrap_err()
            .message
            .contains("Draft 2020-12")
    );
    // Nested objects, including inside array items, normalise too.
    let nested = r#"{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"}}}}}}"#;
    let (frozen, _) = admit_output_schema(nested).unwrap();
    assert_eq!(frozen.matches(r#""additionalProperties":false"#).count(), 2);
    assert_eq!(frozen.matches(r#""required":[]"#).count(), 2);
    // One level of double encoding is unwrapped.
    let encoded = serde_json::to_string(LUNA_LIVE_ARGUMENTS[1]).unwrap();
    assert_eq!(
        admit_output_schema(&encoded).unwrap(),
        admit_output_schema(LUNA_LIVE_ARGUMENTS[1]).unwrap()
    );
}

#[test]
fn interrupt_cancels_turn_without_terminalizing_attempt() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.set_active_turn(&spawned.agent_id, true).unwrap();
    tree.interrupt_agent("root", &close_req(&spawned.agent_id), None)
        .unwrap();
    // The node reads `interrupted`, not `running` — but it is still
    // non-terminal, and its attempt is untouched.
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Interrupted
    );
    assert!(!NodeStatus::Interrupted.is_terminal());
    assert!(tree.node(&spawned.agent_id).unwrap().active_turn);
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::Running
    );
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Failed
    );
    let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.close_agent("root", &close_req(&other.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    tree.record_turn_completed(&other.agent_id).unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&other.agent_id).unwrap().status,
        NodeStatus::Closed
    );
}

#[test]
fn nested_children_bind_their_own_cards_not_their_callers() {
    let (_tmp, mut tree) = tree();
    tree.set_node_account_for_test("root", "grok");
    pin(
        &mut tree,
        "orchestrate",
        "claude",
        "claude-sonnet-5",
        Some("high"),
    );
    pin(&mut tree, "review", "codex", "gpt-5.6-luna", None);
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().tuple_id,
        "claude-sonnet-5"
    );
    assert_eq!(tree.node(&child.agent_id).unwrap().account, "claude");
    assert!(tree.grants.get(&child.agent_id).unwrap().may_spawn);
    assert!(tree.grants.get(&child.agent_id).unwrap().may_adopt);
    let grandchild = tree
        .spawn_agent(&child.agent_id, &spawn_req(), None)
        .unwrap();
    let grandchild = tree.node(&grandchild.agent_id).unwrap();
    assert_eq!(
        (grandchild.account.as_str(), grandchild.tuple_id.as_str()),
        ("codex", "gpt-5.6-luna")
    );
    assert_eq!(grandchild.effort, None, "the caller's effort is not copied");
    assert!(!tree.grants.get(&grandchild.agent_id).unwrap().may_spawn);
}

#[test]
fn stop_all_closes_non_root_and_is_root_only() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.stop_all(&spawned.agent_id, None).unwrap_err().code,
        TopologyErrorCode::GrantDenied
    );
    tree.settle_for_test();
    let closed = tree.stop_all("root", None).unwrap();
    tree.settle_for_test();
    assert!(closed.iter().any(|(id, _)| id == &spawned.agent_id));
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
}

#[test]
fn stop_all_with_shared_host_operation_closes_every_sibling() {
    let (_tmp, mut tree) = tree();
    let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let closed = tree.stop_all("root", Some("op-stop-all")).unwrap();
    tree.settle_for_test();
    assert!(closed.iter().any(|(id, _)| id == &first.agent_id));
    assert!(closed.iter().any(|(id, _)| id == &second.agent_id));
    assert_eq!(
        tree.node(&first.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(
        tree.node(&second.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
}

/// After `interrupt_agent` the
/// child read `recovery_required` / `session_materialization` and its
/// parent's inbox got a `result` row, so it could be neither steered nor
/// re-tasked. An interrupt cancels *this
/// turn*; the node stays reusable, seals nothing and pushes nothing. Once
/// the cancelled turn has unwound (`release_attempt_turn`), the next mail
/// starts a fresh turn on the same attempt and the node runs again.
#[test]
fn interrupted_child_keeps_its_attempt_pushes_no_result_and_resumes_on_the_next_mail() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    std::fs::write(
        tree.node(&child.agent_id).unwrap().path.join("H.txt"),
        "partial\n",
    )
    .unwrap();
    tree.set_active_turn(&child.agent_id, true).unwrap();
    tree.bind_attempt_turn(&child.agent_id, "turn-1").unwrap();
    tree.interrupt_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    // The host waiter observes the cancelled turn and releases it.
    tree.release_attempt_turn(&child.agent_id).unwrap();

    let node = tree.node(&child.agent_id).unwrap();
    assert_eq!(node.status, NodeStatus::Interrupted);
    assert!(!node.active_turn);
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert_eq!(attempt.attempt_id, child.attempt_id);
    assert_eq!(attempt.status, AttemptStatus::Running);
    assert!(attempt.turn_id.is_none(), "the cancelled turn is released");
    assert!(attempt.failure.is_none(), "no failure kind on an interrupt");
    assert!(tree.candidates().is_empty(), "an interrupt seals nothing");
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(true),
            },
        )
        .unwrap();
    assert_eq!(
        listed
            .inbox
            .iter()
            .filter(|mail| mail.purpose == MessagePurpose::Result)
            .count(),
        0,
        "an interrupt pushes no result: {:?}",
        listed.inbox
    );

    // An urgent steer is the resume: with no bound turn it starts one.
    let mut steer = mail(&child.agent_id, "stop sleeping, finish and submit");
    steer.priority = Some(MessagePriority::Urgent);
    let sent = tree.send_agent_message("root", &steer, None).unwrap();
    assert_eq!(sent.disposition, MailDisposition::Started);
    // ...which the tool layer records exactly like a spawn start.
    tree.set_active_turn(&child.agent_id, true).unwrap();
    tree.bind_attempt_turn(&child.agent_id, "turn-2").unwrap();
    let node = tree.node(&child.agent_id).unwrap();
    assert_eq!(node.status, NodeStatus::Running, "the node runs again");
    assert!(node.active_turn);
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert_eq!(attempt.attempt_id, child.attempt_id, "same attempt");
    assert_eq!(attempt.turn_id.as_deref(), Some("turn-2"));
    assert_eq!(attempt.usage.turns, 2, "the resume is a second turn");
    // The wait parks again on the running child.
    assert!(
        tree.poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .is_none()
    );
}

/// `followup` is refused only for `Closed`/`RecoveryRequired`
/// nodes and discarded copies. An interrupted child whose turn has ended
/// can be re-tasked: the new attempt supersedes the interrupted one, which
/// ends `cancelled` without a result. While the cancelled turn is still
/// unwinding (bound turn present) the followup is refused as not idle.
#[test]
fn followup_after_interrupt_opens_a_new_attempt_and_cancels_the_interrupted_one() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.set_active_turn(&child.agent_id, true).unwrap();
    tree.bind_attempt_turn(&child.agent_id, "turn-1").unwrap();
    tree.interrupt_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    let mut followup = mail(&child.agent_id, "create LATER.md and submit");
    followup.purpose = Some(MessagePurpose::Followup);
    let error = tree
        .send_agent_message("root", &followup, None)
        .unwrap_err();
    assert_eq!(
        error.code,
        TopologyErrorCode::FollowupNotIdle,
        "the turn has not unwound yet: {error:?}"
    );

    tree.release_attempt_turn(&child.agent_id).unwrap();
    let sent = tree.send_agent_message("root", &followup, None).unwrap();
    assert_eq!(sent.disposition, MailDisposition::Started);
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert_ne!(attempt.attempt_id, child.attempt_id, "a new attempt");
    assert_eq!(attempt.status, AttemptStatus::Running);
    assert_eq!(attempt.retry_of.as_ref(), Some(&child.attempt_id));
    let interrupted = tree.attempts.get(&child.attempt_id).unwrap();
    assert_eq!(interrupted.status, AttemptStatus::Cancelled);
    assert!(interrupted.failure.is_none());
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Running
    );
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(true),
            },
        )
        .unwrap();
    assert!(
        listed
            .inbox
            .iter()
            .all(|mail| mail.purpose != MessagePurpose::Result),
        "{:?}",
        listed.inbox
    );
}

#[test]
fn bind_after_materialization_failure_restores_running() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_host_materialization(&spawned.agent_id)
        .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    tree.bind_session_locator(&spawned.agent_id, "session:c1")
        .unwrap();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    tree.record_session_bound_after_failure(&spawned.agent_id)
        .unwrap();
    tree.fold_due(&spawned.agent_id).unwrap();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::Running
    );
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::Running
    );
    assert_eq!(
        tree.node(&spawned.agent_id)
            .unwrap()
            .session_locator
            .as_deref(),
        Some("session:c1")
    );
}

#[test]
fn prepared_spawn_replays_and_restore_refuses_unfinished_operation() {
    let (_tmp, mut tree) = tree();
    let req = spawn_req();
    assert!(
        tree.prepare_operation("op-spawn-live", "root", "spawn_agent", &req)
            .unwrap()
            .is_none()
    );
    let first = tree
        .spawn_agent("root", &req, Some("op-spawn-live"))
        .unwrap();
    let replay = tree
        .spawn_agent("root", &req, Some("op-spawn-live"))
        .unwrap();
    assert_eq!(replay.agent_id, first.agent_id);
    assert_eq!(replay.attempt_id, first.attempt_id);
    tree.prepare_operation("op-spawn-crash", "root", "spawn_agent", &req)
        .unwrap();
    tree.restore_paused();
    assert_eq!(
        tree.spawn_agent("root", &req, Some("op-spawn-crash"))
            .unwrap_err()
            .code,
        TopologyErrorCode::OperationRecoveryRequired
    );
}

/// B9: a child that needs recovery is not idle, and a followup would open
/// a second attempt over a copy whose state nobody has accounted for.
#[test]
fn followup_to_a_recovery_required_child_is_refused() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    tree.record_fault(&child.agent_id, "the session went away");
    tree.record_host_materialization(&child.agent_id)
        .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    let mut followup = mail(&child.agent_id, "carry on");
    followup.purpose = Some(MessagePurpose::Followup);
    let error = tree
        .send_agent_message("root", &followup, None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::FollowupRecoveryRequired);
    assert!(error.message.contains("recovery"), "{}", error.message);
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().attempt_id,
        child.attempt_id,
        "no second attempt was opened"
    );
}

/// Gate 3: `AgentErrorKind::Cancelled` also arrives from a provider, on a
/// node the host never interrupted. Ending the attempt is what lets the
/// parent's `wait_agent` settle; leaving it `running` stranded the node.
#[test]
fn a_cancel_the_host_did_not_ask_for_ends_the_attempt() {
    let repo = git_repo();
    let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
    let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
    let child_path = tree.node(&child.agent_id).unwrap().path.clone();
    std::fs::write(child_path.join("H.txt"), "partial\n").unwrap();
    tree.set_active_turn(&child.agent_id, true).unwrap();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Running
    );
    tree.record_host_cancellation(&child.agent_id)
        .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    let node = tree.node(&child.agent_id).unwrap();
    assert_eq!(node.status, NodeStatus::Failed);
    assert!(node.status.settles_a_wait(), "the parent's wait settles");
    let attempt = tree.attempt(&child.agent_id).unwrap();
    assert_eq!(attempt.status, AttemptStatus::Failed);
    assert_eq!(
        attempt
            .failure
            .as_ref()
            .map(|failure| failure.kind.as_str()),
        Some(CANCELLED_KIND)
    );
    assert!(attempt.candidate_id.is_some(), "partial work still seals");
}
