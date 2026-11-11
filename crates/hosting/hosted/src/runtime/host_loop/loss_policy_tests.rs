//! The attached root forwards `[host] resume_after_loss` to its tree's hub.

#![allow(clippy::unwrap_used, reason = "test-owned host and tree")]
use crate::runtime::test_kit::BlockOn as _;

use crate::runtime::Sender;
use crate::runtime::recover_tests::{script, scripted_host_with};
use crate::runtime::test_kit::TestExtension;

#[test]
fn placement_hold_root_loss_resume_defers_then_runs_one_prepared_delivery() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {"output": {"text": "continued once"}}
        ])),
    );
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        super::RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    lose_root(&host);
    let roots = host.role.config.state_roots.clone();
    let registry = plakous::runner::RunnerRegistry::default();
    registry.configure(plakous::runner::ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    let pending = roots.clone().with_runner_readiness(registry, "local");
    host.role.config.state_roots = pending.clone();
    assert!(
        host.block(async |host| host.resume_after_loss_all().await)
            .is_none()
    );
    assert!(
        host.block(async |host| host.root_loss_resume_delivery().await)
            .unwrap()
            .is_none()
    );
    let intents = host
        .role
        .config
        .store
        .load_intents(&host.role.config.session.id)
        .unwrap();
    assert_eq!(intents.root_continuations.len(), 0);
    assert_eq!(intents.submissions.len(), 0);
    assert!(host.role.root_loss_resume_due);
    host.role.config.state_roots = roots;
    let (text, delivery, echoed) = host
        .block(async |host| host.root_loss_resume_delivery().await)
        .unwrap()
        .expect("ready continuation");
    // A loss after preparation must not discard or repeatedly re-drive its receipt.
    host.role.config.state_roots = pending;
    host.block(async |host| host.run_prompt(text, delivery, echoed, None).await)
        .unwrap();
    assert_eq!(host.role.config.session.root_turn_count, 1);
    let intents = host
        .role
        .config
        .store
        .load_intents(&host.role.config.session.id)
        .unwrap();
    assert_eq!(intents.root_continuations.len(), 1);
    assert_eq!(intents.submissions.len(), 1);
    host.block(async |host| host.release().await).unwrap();
}

/// Records a lost root turn as the open does after its feed scan.
fn lose_root<P: plakous_host_protocol::ProtocolExtension>(host: &super::RootActor<'_, P>) {
    assert!(
        crate::runtime::root_loss::record_root_owner_lost(
            host.role.config.store.as_ref(),
            &host.role.config.session,
            &crate::runtime::root_loss::LostRootTurn {
                turn_id: "lost-turn".to_owned(),
                applied_seq: 0,
            },
        )
        .unwrap()
    );
}

/// A tree whose one child a lost host left held after a loss.
pub(in crate::runtime) fn held_child_tree(
    id: &str,
    cwd: std::path::PathBuf,
) -> (crate::agent_tree::ProductAgentTree, String) {
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        id,
        "root",
        "owner",
        cwd,
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent(
            "root",
            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    // What a reopen's parked mark records.
    tree.mark_owner_lost(&child);
    assert_eq!(
        tree.loss_continuation(&child).map(|held| held.state),
        Some(crate::agent_tree::LossContinuationState::Held)
    );
    (tree, child)
}

/// A tree whose one child a planned handoff interrupted while it ran: the
/// next host continues it by itself.
fn handed_off_child_tree(
    id: &str,
    cwd: std::path::PathBuf,
) -> (crate::agent_tree::ProductAgentTree, String) {
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        id,
        "root",
        "owner",
        cwd,
        "selected-tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent(
            "root",
            &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.record_handoff().unwrap();
    assert_eq!(
        tree.loss_continuation(&child).map(|held| held.cause),
        Some(crate::agent_tree::ContinuationCause::Handoff)
    );
    (tree, child)
}

/// The open forwards the host config's `resume_after_loss` to the tree's
/// hub, which is what every recovery run's continuation pass reads
/// (`a_continue_policy_resumes_a_held_child_once` pins the pass itself).
#[test]
fn an_attached_root_forwards_its_loss_policy_to_its_tree() {
    for continues in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
        config.resume_after_loss = if continues {
            crate::runtime::ResumeAfterLoss::Continue
        } else {
            crate::runtime::ResumeAfterLoss::Hold
        };
        let id = config.session.id.clone();
        let (tree, _child) = held_child_tree(&id, config.cwd.clone());
        let runtime = crate::topology_runtime::insert_root(id.clone(), tree, None)
            .block()
            .unwrap();
        // The hub starts at the other policy, so only the open can set it.
        runtime.set_continue_after_loss(!continues);
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let host = super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        assert_eq!(runtime.continues_after_loss(), continues);
        drop(host);
        crate::topology_runtime::force_release(&id).block();
    }
}

/// The attention notices (`ResumeAfterLoss` rows) the open sent, read once
/// the open's scan run over `tree_id` has finished.
fn attention_notices(
    tree_id: &str,
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
) -> Vec<String> {
    let runtime = crate::topology_runtime::lookup(tree_id).expect("the tree's hub");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while runtime.recovery_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "the scan run never finished"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            crate::runtime::HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice),
            ) if notice.remedy
                == Some(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeAfterLoss {}) =>
            {
                Some(notice.text)
            }
            _ => None,
        })
        .collect()
}

/// A host config over a tree whose one child a lost host left held (or a
/// planned handoff interrupted, with `handoff`), with the root's own turn
/// lost too when `root_lost`, under `policy`; with the tree id and the child.
fn after_loss(
    root: &std::path::Path,
    policy: crate::runtime::ResumeAfterLoss,
    root_lost: bool,
    handoff: bool,
) -> (crate::runtime::HostConfig<TestExtension>, String, String) {
    let (mut config, _) = scripted_host_with(root, script(serde_json::json!([])));
    config.resume_after_loss = policy;
    let id = config.session.id.clone();
    let (tree, child) = if handoff {
        handed_off_child_tree(&id, config.cwd.clone())
    } else {
        held_child_tree(&id, config.cwd.clone())
    };
    let _runtime = crate::topology_runtime::insert_root(id.clone(), tree, None)
        .block()
        .unwrap();
    if root_lost {
        config.store.save(&mut config.session).unwrap();
        assert!(
            crate::runtime::root_loss::record_root_owner_lost(
                config.store.as_ref(),
                &config.session,
                &crate::runtime::root_loss::LostRootTurn {
                    turn_id: "lost-turn".to_owned(),
                    applied_seq: 0,
                },
            )
            .unwrap()
        );
    }
    (config, id, child)
}

/// Opens a host on `config` and returns the attention notices its open
/// sent, read once its scan run finished.
fn open_and_read_attention(
    config: crate::runtime::HostConfig<TestExtension>,
    id: &str,
) -> Vec<String> {
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let host = super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    let attention = attention_notices(id, &events);
    drop(host);
    crate::topology_runtime::force_release(id).block();
    attention
}

/// Under hold the open's scan run sends one attention notice for everything
/// a lost host left held, once it has marked every child: the root's turn
/// and each held child, by id and what it was doing, with the one Resume.
/// Nothing sends a second one.
#[test]
fn the_open_sends_one_attention_notice_for_the_root_and_its_children() {
    let root = tempfile::tempdir().unwrap();
    let (config, id, child) = after_loss(
        root.path(),
        crate::runtime::ResumeAfterLoss::Hold,
        true,
        false,
    );
    let attention = open_and_read_attention(config, &id);
    assert_eq!(attention.len(), 1, "{attention:#?}");
    assert!(
        attention[0].starts_with("the turn the lost host was running is held, and so are 1 agent"),
        "{attention:#?}"
    );
    assert!(attention[0].contains(&child), "{attention:#?}");
    assert!(attention[0].contains("reviewer"), "{attention:#?}");
}

/// A handed-off child continues by itself: the notice does not name it, so
/// with nothing else held there is none.
#[test]
fn a_handed_off_child_is_not_in_the_attention_notice() {
    let root = tempfile::tempdir().unwrap();
    let (config, id, _) = after_loss(
        root.path(),
        crate::runtime::ResumeAfterLoss::Hold,
        false,
        true,
    );
    let attention = open_and_read_attention(config, &id);
    assert!(attention.is_empty(), "{attention:#?}");
}

/// Under `continue` nothing waits for the operator, so the open sends no
/// attention notice.
#[test]
fn the_continue_policy_sends_no_attention_notice() {
    let root = tempfile::tempdir().unwrap();
    let (config, id, _) = after_loss(
        root.path(),
        crate::runtime::ResumeAfterLoss::Continue,
        true,
        false,
    );
    let attention = open_and_read_attention(config, &id);
    assert!(attention.is_empty(), "{attention:#?}");
}

/// The one Resume of a root a lost host left held records consent
/// before it posts the root's continuation, a host-wake submission under
/// the decision's command id; a second Resume records nothing new.
#[test]
fn the_one_resume_records_consent_then_posts_the_root_continuation() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    lose_root(&host);
    assert!(host.root_loss_held());
    let (text, item, _) = host
        .block(async |host| host.resume_after_loss_all().await)
        .expect("the root's continuation is posted");
    assert_eq!(text, crate::runtime::root_loss::ROOT_LOSS_RESUME_TEXT);
    let intents = host
        .role
        .config
        .store
        .load_intents(&host.role.config.session.id)
        .unwrap();
    assert_eq!(intents.root_continuations.len(), 1, "consent is recorded");
    let crate::sessions::projector::RootContinuationDecision::NewTurn { command_id, .. } =
        &intents.root_continuations[0].value.decision
    else {
        panic!("a new turn");
    };
    let submission = intents.submission(command_id).expect("posted under it");
    assert_eq!(
        submission.kind,
        crate::sessions::projector::SubmissionKind::HostWake
    );
    assert_eq!(
        Some(&submission.queue_item_id),
        item.as_ref().map(|item| &item.id)
    );
    let _ = host.block(async |host| host.resume_after_loss_all().await);
    let intents = host
        .role
        .config
        .store
        .load_intents(&host.role.config.session.id)
        .unwrap();
    assert_eq!(intents.root_continuations.len(), 1);
    assert_eq!(
        intents
            .submissions
            .iter()
            .filter(
                |submission| crate::runtime::root_loss::is_root_loss_command(
                    &submission.command_id
                )
            )
            .count(),
        1
    );
}

/// At open a held root follows `resume_after_loss`: under hold it stays
/// held with its restored queue paused and one live notice offering the
/// Resume; under continue its continuation is due and the queue drains.
#[test]
fn the_open_holds_or_continues_a_held_root_by_policy() {
    for policy in [
        crate::runtime::ResumeAfterLoss::Hold,
        crate::runtime::ResumeAfterLoss::Continue,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.role.config.session.queue_drain_state =
            crate::sessions::QueueDrainState::RestoredPaused;
        host.role
            .config
            .store
            .save(&mut host.role.config.session)
            .unwrap();
        lose_root(&host);
        while events.try_recv().is_ok() {}
        host.role.config.resume_after_loss = policy;
        host.arm_root_loss_resume();
        let continues = policy == crate::runtime::ResumeAfterLoss::Continue;
        assert_eq!(host.role.root_loss_resume_due, continues, "{policy:?}");
        assert_eq!(
            host.role.config.session.queue_drain_state,
            if continues {
                crate::sessions::QueueDrainState::LiveAutoDrain
            } else {
                crate::sessions::QueueDrainState::RestoredPaused
            },
            "{policy:?}"
        );
        let notes = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| format!("{event:?}").contains(super::loss_resume::ROOT_LOSS_HELD_NOTE))
            .count();
        assert_eq!(notes > 0, !continues, "{policy:?}");
    }
}

/// The root's continuation after a loss is its next turn, so its recorded
/// command names the child the lost host interrupted.
#[test]
fn the_roots_continuation_names_the_child_a_lost_host_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host_with(root.path(), script(serde_json::json!([])));
    let id = config.session.id.clone();
    let (tree, child) = held_child_tree(&id, config.cwd.clone());
    let _runtime = crate::topology_runtime::insert_root(id.clone(), tree, None)
        .block()
        .unwrap();
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = super::RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    lose_root(&host);
    let (text, _, _) = host
        .block(async |host| host.root_loss_resume_delivery().await)
        .unwrap()
        .expect("the root's continuation");
    assert_eq!(text, crate::runtime::root_loss::ROOT_LOSS_RESUME_TEXT);
    let intents = host
        .role
        .config
        .store
        .load_intents(&host.role.config.session.id)
        .unwrap();
    let submission = intents
        .submissions
        .iter()
        .find(|submission| crate::runtime::root_loss::is_root_loss_command(&submission.command_id))
        .expect("the continuation's submission");
    assert!(
        submission.canonical_command.starts_with(&format!(
            "{}\n\n[harness]\n<agent_notice agent_id=\"{child}\"",
            crate::runtime::root_loss::ROOT_LOSS_RESUME_TEXT
        )),
        "{}",
        submission.canonical_command
    );
    drop(host);
    crate::topology_runtime::force_release(&id).block();
}
