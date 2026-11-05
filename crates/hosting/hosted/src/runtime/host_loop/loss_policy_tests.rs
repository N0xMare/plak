//! The attached root forwards `[host] resume_after_loss` to its tree's hub.

#![allow(clippy::unwrap_used, reason = "test-owned host and tree")]
use crate::runtime::test_kit::BlockOn as _;

use crate::runtime::Sender;
use crate::runtime::recover_tests::{script, scripted_host_with};
use crate::runtime::test_kit::TestExtension;

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
