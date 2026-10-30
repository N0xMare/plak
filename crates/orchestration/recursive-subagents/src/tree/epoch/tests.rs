use crate::epoch::RosterInput;
use crate::tree::test_support::*;
use crate::tree::*;

#[test]
fn grant_compiler_hides_submit_on_root_and_spawn_on_child() {
    let (_tmp, mut tree) = tree();
    let root = tree.compiled_tool_names("root").unwrap();
    assert!(root.contains(&"spawn_agent"));
    assert!(root.contains(&"adopt_candidate"));
    assert!(!root.contains(&"submit_result"));
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let child = tree.compiled_tool_names(&spawned.agent_id).unwrap();
    assert!(child.contains(&"submit_result"));
    assert!(child.contains(&"send_agent_message"));
    assert!(!child.contains(&"spawn_agent"));
    assert!(!child.contains(&"adopt_candidate"));
    tree.bind_session_locator(&spawned.agent_id, "session:c1")
        .unwrap();
    assert_eq!(
        tree.node(&spawned.agent_id)
            .unwrap()
            .session_locator
            .as_deref(),
        Some("session:c1")
    );
    tree.record_host_materialization(&spawned.agent_id)
        .and_then(|value| tree.fold_due(&spawned.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.attempt(&spawned.agent_id).unwrap().status,
        AttemptStatus::RecoveryRequired
    );
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
    assert_eq!(
        tree.record_turn_completed(&spawned.agent_id)
            .unwrap_err()
            .code,
        TopologyErrorCode::RecipientRecoveryRequired
    );
    tree.settle_for_test();
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

#[test]
fn unknown_template_fails_closed() {
    let (_tmp, mut tree) = tree();
    tree.snapshot.roster.clear();
    tree.snapshot.digest = tree.snapshot.compute_digest();
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::TemplateUnknown
    );
}

#[test]
fn epoch_freeze_blocks_rebind_until_descendants_are_finished() {
    let (_tmp, mut tree) = tree();
    assert!(tree.epoch_allows_rebind());
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert!(!tree.epoch_allows_rebind());
    tree.close_agent("root", &close_req(&spawned.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert!(tree.epoch_allows_rebind());
    assert_eq!(tree.state, TreeAdmissionState::Live);
    let mut input = RosterInput::unpinned("grok-4.6");
    input.root.account = "grok".to_owned();
    input.root.effort = Some("low".to_owned());
    let successor = OrchestrationSnapshot::freeze(input);
    tree.begin_successor_epoch(successor).unwrap();
    assert_eq!(tree.state, TreeAdmissionState::Live);
    assert_eq!(tree.selected_tuple, "grok-4.6");
    assert_eq!(tree.snapshot.root_tuple, "grok-4.6");
}

/// Closing the last child leaves an idle tree, not a stopped one: the
/// root delegates wave after wave, including after failed children.
#[test]
fn closing_every_child_keeps_delegation_open_for_the_next_wave() {
    let (_tmp, mut tree) = tree();
    for _wave in 0..3 {
        let done = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let failed = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_host_materialization(&failed.agent_id)
            .and_then(|value| tree.fold_due(&failed.agent_id).map(|_| value))
            .unwrap();
        tree.settle_for_test();
        for id in [&done.agent_id, &failed.agent_id] {
            tree.close_agent("root", &close_req(id), None).unwrap();
            tree.settle_for_test();
        }
        assert!(tree.epoch_allows_rebind());
        assert_eq!(tree.state, TreeAdmissionState::Live);
    }
    tree.state = TreeAdmissionState::Interrupted;
    let last = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.close_agent("root", &close_req(&last.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(tree.state, TreeAdmissionState::Live);

    tree.stop_all("root", None).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
    let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
    assert_eq!(refused.code, TopologyErrorCode::GrantDenied);
    assert_eq!(refused.message, ProductAgentTree::TREE_STOPPED_REFUSAL);
}

#[test]
fn next_root_prompt_resumes_a_settled_stop_without_resetting_nodes() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.settle_for_test();
    tree.stop_all("root", None).unwrap();
    tree.settle_for_test();
    assert_eq!(tree.state, TreeAdmissionState::TreeTerminal);
    assert!(tree.spawn_agent("root", &spawn_req(), None).is_err());
    let count = tree.nodes.len();
    tree.mark_root_live_for_prompt();
    assert_eq!(tree.state, TreeAdmissionState::Live);
    assert_eq!(tree.nodes.len(), count);
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Closed
    );
    assert!(tree.spawn_agent("root", &spawn_req(), None).is_ok());
}

/// A recovery-required child runs nothing, so it does not hold a settled
/// stop; the next root prompt lifts the stop and leaves the child's
/// recovery obligation exactly where it was.
#[test]
fn next_root_prompt_lifts_a_stop_over_a_recovery_required_child_without_clearing_it() {
    let (_tmp, mut tree) = tree();
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_host_materialization(&child.agent_id)
        .and_then(|value| tree.fold_due(&child.agent_id).map(|_| value))
        .unwrap();
    tree.settle_for_test();
    tree.state = TreeAdmissionState::TreeTerminal;
    tree.mark_root_live_for_prompt();
    assert_eq!(tree.state, TreeAdmissionState::Live);
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::RecoveryRequired
    );
}

/// A child whose session outcome is unresolved holds no child slot, and
/// closing it is accepted without authorizing a retry: the attempt keeps
/// its recovery-required failure.
#[test]
fn an_unresolved_child_holds_no_slot_and_closing_it_keeps_its_failure() {
    let (_tmp, mut tree) = tree();
    tree.grants
        .get_mut("root")
        .expect("root grant")
        .max_children = 1;
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_failure(
        &child.agent_id,
        turn_outcome::FailedTurnDisposition::RecoveryRequired,
        "child turn outcome unknown".to_owned(),
    )
    .unwrap();
    tree.settle_for_test();
    let closed = tree
        .close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert_eq!(
        closed,
        [(child.agent_id.clone(), NodeStatus::RecoveryRequired)]
    );
    assert_eq!(
        tree.attempt(&child.agent_id).unwrap().status,
        AttemptStatus::RecoveryRequired
    );
    assert!(
        tree.spawn_agent("root", &spawn_req(), None).is_ok(),
        "the unresolved child does not hold the only slot"
    );
}

/// An attempt whose turn a reopen abandoned leaves its node failed and
/// re-taskable, and is refunded: it does not count against the budget.
#[test]
fn an_abandoned_attempt_is_refunded_and_leaves_the_node_retaskable() {
    let (_tmp, mut tree) = tree();
    tree.max_attempts = 1;
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_failure(
        &child.agent_id,
        turn_outcome::FailedTurnDisposition::Failed {
            kind: TURN_ABANDONED,
        },
        "the process was lost while the child was running".to_owned(),
    )
    .unwrap();
    tree.settle_for_test();
    assert_eq!(
        tree.node(&child.agent_id).unwrap().status,
        NodeStatus::Failed
    );
    assert!(tree.attempt(&child.agent_id).unwrap().turn_was_abandoned());
    tree.close_agent("root", &close_req(&child.agent_id), None)
        .unwrap();
    tree.settle_for_test();
    assert!(
        tree.spawn_agent("root", &spawn_req(), None).is_ok(),
        "an abandoned attempt did not spend the only attempt"
    );
}

/// a completed child is idle, not terminal, yet the epoch is
/// quiescent: rebind and roster apply judge finished work, not re-taskability.
#[test]
fn a_completed_child_is_idle_for_followup_but_quiescent_for_the_epoch() {
    let (_tmp, mut tree) = tree();
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.record_turn_completed(&spawned.agent_id).unwrap();
    tree.settle_for_test();
    let status = tree.node(&spawned.agent_id).unwrap().status;
    assert_eq!(status, NodeStatus::Failed);
    assert!(!status.is_terminal());
    assert!(status.is_finished());
    assert!(tree.epoch_allows_rebind());
    assert_eq!(tree.epoch_rebind_refusal(), None);
    // Listing hides finished rows by default, as before.
    let listed = tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_read: None,
                include_self: Some(false),
                include_completed: Some(false),
            },
        )
        .unwrap();
    assert_eq!(listed.agents, [] as [serde_json::Value; 0]);
    assert_eq!(
        tree.stop_all("root", None).unwrap(),
        [] as [(String, NodeStatus); 0]
    );
}

/// A singleton (or a tree whose descendants are all finished) has nothing
/// to resume: reopening it never paints a pause, never gates rebind or the
/// roster apply, and lifts a stale pause persisted by an earlier process.
#[test]
fn reopening_a_quiescent_tree_never_pauses_it() {
    let (_tmp, mut singleton) = tree();
    singleton.restore_paused();
    assert_eq!(singleton.state, TreeAdmissionState::Live);
    assert_eq!(singleton.epoch_rebind_refusal(), None);
    assert!(singleton.spawn_agent("root", &spawn_req(), None).is_ok());

    let (_tmp, mut persisted_paused) = tree();
    persisted_paused.state = TreeAdmissionState::RestoredPaused;
    persisted_paused.restore_paused();
    assert_eq!(persisted_paused.state, TreeAdmissionState::Live);
    assert_eq!(persisted_paused.epoch_rebind_refusal(), None);

    let (_tmp, mut all_terminal) = tree();
    let spawned = all_terminal
        .spawn_agent("root", &spawn_req(), None)
        .unwrap();
    all_terminal
        .close_agent("root", &close_req(&spawned.agent_id), None)
        .unwrap();
    all_terminal.state = TreeAdmissionState::Interrupted;
    all_terminal.restore_paused();
    assert_eq!(all_terminal.state, TreeAdmissionState::Live);
    assert_eq!(all_terminal.epoch_rebind_refusal(), None);

    let (_tmp, mut terminal_epoch) = tree();
    terminal_epoch.state = TreeAdmissionState::TreeTerminal;
    terminal_epoch.restore_paused();
    assert_eq!(terminal_epoch.state, TreeAdmissionState::TreeTerminal);
}

/// Only a tree with a live descendant (or a pending spawn) reopens paused.
#[test]
fn reopening_a_tree_with_live_descendants_pauses_it() {
    let (_tmp, mut live) = tree();
    let spawned = live.spawn_agent("root", &spawn_req(), None).unwrap();
    live.restore_paused();
    assert_eq!(live.state, TreeAdmissionState::RestoredPaused);
    assert_eq!(
        live.node(&spawned.agent_id)
            .unwrap()
            .presentation_status(live.state),
        NodeStatus::Paused
    );
    assert_eq!(
        live.epoch_rebind_refusal(),
        Some(ProductAgentTree::TREE_EPOCH_REBIND_PAUSED)
    );

    let (_tmp, mut pending) = tree();
    pending.pending_spawns = 1;
    pending.restore_paused();
    assert_eq!(pending.state, TreeAdmissionState::RestoredPaused);
}

#[test]
fn epoch_rebind_refusal_splits_paused_spawns_and_descendants() {
    let (_tmp, mut tree) = tree();
    assert_eq!(tree.epoch_rebind_refusal(), None);

    tree.state = TreeAdmissionState::RestoredPaused;
    assert_eq!(
        tree.epoch_rebind_refusal(),
        Some(ProductAgentTree::TREE_EPOCH_REBIND_PAUSED)
    );
    assert!(tree.epoch_allows_rebind());

    tree.state = TreeAdmissionState::Live;
    tree.pending_spawns = 1;
    assert_eq!(
        tree.epoch_rebind_refusal(),
        Some(ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS)
    );

    tree.pending_spawns = 0;
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(
        tree.epoch_rebind_refusal(),
        Some(ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS)
    );
    let _ = spawned;
}

#[test]
fn template_authority_and_depth_govern_nesting() {
    let (_tmp, mut tree) = tree();
    tree.set_node_account_for_test("root", "grok");
    let mut snapshot = OrchestrationSnapshot::unpinned("grok-4.6");
    snapshot.bind_hints = test_bind_hints();
    snapshot.max_depth = MAX_TREE_DEPTH;
    let mut supervisor = RosterBinding::leaf("claude", "claude-sonnet-5");
    supervisor.template = "supervisor".into();
    supervisor.effort = Some("low".into());
    supervisor.max_tier = Tier::ReadOnly;
    supervisor.spawns = vec![TemplateName::parse("explore").unwrap()];
    snapshot
        .roster
        .insert(TemplateName::parse("orchestrate").unwrap(), supervisor);
    snapshot
        .working_set
        .insert(TemplateName::parse("orchestrate").unwrap());
    let mut explore = RosterBinding::leaf("grok", "grok-4.6");
    explore.max_tier = Tier::ReadOnly;
    snapshot
        .roster
        .insert(TemplateName::parse("explore").unwrap(), explore);
    snapshot.digest = snapshot.compute_digest();
    tree.install_snapshot(snapshot);
    assert_eq!(tree.grants.get("root").unwrap().max_depth, MAX_TREE_DEPTH);
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    assert_eq!(tree.node(&child.agent_id).unwrap().account, "claude");
    assert!(tree.grants.get(&child.agent_id).unwrap().may_spawn);
    let explore = SpawnAgentRequest::test_named("explore", Tier::ReadOnly, "scout", "look");
    let grandchild = tree.spawn_agent(&child.agent_id, &explore, None).unwrap();
    let grandchild = tree.node(&grandchild.agent_id).unwrap();
    assert_eq!(
        (grandchild.account.as_str(), grandchild.tuple_id.as_str()),
        ("grok", "grok-4.6"),
        "a card binds its own account and model, never its caller's"
    );
    assert_eq!(grandchild.effort, None, "nor its caller's effort");
    assert!(!tree.grants.get(&grandchild.agent_id).unwrap().may_spawn);
}

#[test]
fn only_spawn_capable_templates_compile_spawn_agent() {
    let (_tmp, mut tree) = tree();
    tree.grants.get_mut("root").unwrap().max_depth = 2;
    assert!(
        tree.compiled_tool_names("root")
            .unwrap()
            .contains(&"spawn_agent")
    );
    let review = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert!(
        !tree
            .compiled_tool_names(&review.agent_id)
            .unwrap()
            .contains(&"spawn_agent")
    );
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    assert!(
        tree.compiled_tool_names(&child.agent_id)
            .unwrap()
            .contains(&"spawn_agent")
    );
    // A spawn-capable template at the depth ceiling cannot nest.
    tree.grants.get_mut("root").unwrap().max_depth = 1;
    let capped = tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .unwrap();
    assert!(!tree.grants.get(&capped.agent_id).unwrap().may_spawn);
    assert!(!tree.grants.get(&capped.agent_id).unwrap().may_adopt);
}

#[test]
fn openrouter_template_admits_spawn_and_openai_stays_mismatch() {
    let (_tmp, mut tree) = tree();
    pin(&mut tree, "review", "openrouter", "vendor/model", None);
    let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    assert_eq!(tree.node(&spawned.agent_id).unwrap().account, "openrouter");
    assert_eq!(
        tree.node(&spawned.agent_id).unwrap().tuple_id,
        "vendor/model"
    );
    pin(&mut tree, "review", "openai", "gpt-4o", None);
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SchemaMismatch
    );
    pin(&mut tree, "review", "openrouter", "", None);
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SchemaMismatch
    );
    let mut router = RosterBinding::leaf("openrouter", "vendor/model");
    router.composition = Some(plakous_host_protocol::selection::CompositionSelection::NativePtc);
    tree.pin_template("review", router);
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SchemaMismatch
    );
}

#[test]
fn oauth_alias_templates_admit_spawn_and_anthropic_stays_mismatch() {
    for account in ["chatgpt", "supergrok", "xai-oauth"] {
        let (_tmp, mut tree) = tree();
        pin(&mut tree, "review", account, "tuple", None);
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(tree.node(&spawned.agent_id).unwrap().account, account);
    }
    let (_tmp, mut tree) = tree();
    pin(&mut tree, "review", "anthropic", "claude-sonnet-5", None);
    assert_eq!(
        tree.spawn_agent("root", &spawn_req(), None)
            .unwrap_err()
            .code,
        TopologyErrorCode::SchemaMismatch
    );
}

#[test]
fn snapshot_digest_mismatch_is_detectable() {
    let (_tmp, mut tree) = tree();
    assert!(tree.snapshot.digest_matches());
    tree.snapshot.roster.clear();
    assert!(!tree.snapshot.digest_matches());
}

/// the epoch snapshot's `max_attempts` is a lower-only clamp
/// re-read on install, and attempt stamps are milliseconds.
#[test]
fn install_snapshot_wires_max_attempts_and_stamps_are_milliseconds() {
    let (_tmp, mut tree) = tree();
    let mut snapshot = OrchestrationSnapshot::unpinned("selected-tuple");
    snapshot.bind_hints = test_bind_hints();
    snapshot.max_attempts = 3;
    tree.install_snapshot(snapshot.clone());
    assert_eq!(tree.max_attempts(), 3);
    snapshot.max_attempts = u16::MAX;
    tree.install_snapshot(snapshot);
    assert_eq!(tree.max_attempts(), MAX_TREE_ATTEMPTS);
    tree.pin_template(
        "review",
        RosterBinding::leaf("test-account", "selected-tuple"),
    );
    let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
    let started_at = tree.attempt(&child.agent_id).unwrap().started_at;
    assert!(
        started_at > 1_600_000_000_000,
        "started_at is Unix milliseconds, got {started_at}"
    );
}
