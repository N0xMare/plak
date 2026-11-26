use super::*;
use plakous::sessions::feed::FeedSeq;
use plakous_hosted::agent_tree_protocol::{
    MessagePriority, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest, WriterMode,
};
use plakous_hosted::sessions::projector::{
    Anchored, AnchoredIntent, PauseCause, RootContinuation, RootContinuationCause,
    RootContinuationDecision, RootInterrupt, RootInterruptCause, RootStopAnchor, SessionViews,
    Submission, SubmissionKind, ViewWrites,
};

#[test]
fn parked_child_mail_needs_attention_on_owner_loss_only_with_hold_policy() {
    parked_child_mail_after_loss(true);
    parked_child_mail_after_loss(false);
}

fn parked_child_mail_after_loss(hold: bool) {
    let (_temp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    if hold {
        std::fs::write(
            paths.config_file(),
            "[host]\nresume_after_loss = \"hold\"\n",
        )
        .unwrap();
    }
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
    ProductStore::open(&paths)
        .unwrap()
        .commit(&mut record)
        .unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &record.id,
        "root",
        "owner",
        paths.home().to_owned(),
        "selected",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator").unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-4000-8000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    tree.send_agent_message(
        "root",
        &SendAgentMessageRequest {
            agent_id: child.clone(),
            message: "for the next boundary".into(),
            purpose: None,
            priority: Some(MessagePriority::Deferred),
            in_reply_to: None,
            acknowledge_recovery: None,
            refresh: None,
            output_schema_json: None,
        },
        None,
    )
    .unwrap();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let services = Services::for_home(paths.clone());
    let sessions = ProductSessions::new(&services, None, None).unwrap();
    sessions.directory.settle();
    assert!(!status(&paths).contains(&record.id));
    assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);

    tree.mark_owner_lost(&child);
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    sessions.directory.settle();
    let reported = status(&paths);
    assert!(!reported.contains("unavailable"), "{reported}");
    assert_eq!(reported.contains("child_held_after_loss"), hold);
    assert_eq!(
        sessions.rows(None, None, false).unwrap()[0].needs_operator,
        hold
    );
}

#[test]
fn status_and_directory_show_blocked_mail_and_clear_with_operator_action() {
    for (held_loss, policy) in [(false, "hold"), (true, "hold"), (true, "continue")] {
        let (temp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        std::fs::write(
            paths.config_file(),
            format!("[host]\nresume_after_loss = \"{policy}\"\n"),
        )
        .unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut record = crate::store::sessions::SessionRecord::new(workspace.clone());
        let mut product = ProductStore::open(&paths).unwrap();
        product.commit(&mut record).unwrap();
        let services = Services::for_home(paths.clone());
        let sessions = ProductSessions::new(&services, None, None).unwrap();
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &record.id,
            "root",
            "owner",
            workspace,
            "selected",
            WriterMode::SingleLease,
        )
        .unwrap();
        let (before, stop) = recovered_pause(&mut product, &record.id, held_loss);
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        sessions.directory.settle();
        assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);
        assert!(!status(&paths).contains(&record.id));

        finish_child(&mut tree);
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        sessions.directory.settle();
        if policy == "continue" {
            assert!(!status(&paths).contains(&record.id));
            assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);
            consent_to_resume(&mut product, &record.id, before.cursor.get(), stop);
            sessions.directory.settle();
            assert!(!status(&paths).contains(&record.id));
            assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);
            continue;
        }

        let status_text = status(&paths);
        assert!(
            status_text.contains("Sessions needing operator attention"),
            "{status_text}"
        );
        assert!(status_text.contains(&record.id), "{status_text}");
        let (reason, action) = if held_loss {
            ("root_held_after_loss", "choose Resume after loss")
        } else {
            ("recovery_required", "send a new prompt")
        };
        assert!(
            status_text.contains(&format!("{reason}: 1 pending mail/wake item")),
            "{status_text}"
        );
        assert!(status_text.contains(action), "{status_text}");
        let SessionRow {
            needs_operator,
            activity,
            ..
        } = sessions.rows(None, None, false).unwrap().remove(0);
        assert!(needs_operator);
        if held_loss {
            assert_eq!(activity, SessionActivity::HeldAfterLoss);
            consent_to_resume(&mut product, &record.id, before.cursor.get(), stop);
        } else {
            project_pause(&mut product, &record.id, &before, None);
        }
        // Clearing the blocker is sufficient; mail need not have been read yet.
        sessions.directory.settle();
        assert!(!status(&paths).contains(&record.id));
        assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);
    }
}

#[test]
fn status_counts_prepared_wakes_but_not_loss_continuation_commands() {
    let (_temp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_owned());
    let mut product = ProductStore::open(&paths).unwrap();
    product.commit(&mut record).unwrap();
    project_pause(
        &mut product,
        &record.id,
        &SessionViews::empty(),
        Some(PauseCause::RecoveryRequired {
            turn_id: "blocked".into(),
        }),
    );
    let submit = |product: &mut ProductStore, command: &str, item: &str| {
        product
            .write_submission(
                &record.id,
                &Submission {
                    command_id: command.into(),
                    kind: SubmissionKind::HostWake,
                    canonical_command: "instruction".into(),
                    queue_item_id: item.into(),
                    queue_item_revision: 1,
                    submitted_at_ms: 1,
                    target_turn_id: None,
                },
            )
            .unwrap();
    };
    submit(&mut product, "plak:resume-loss:root:pending", "loss");
    assert!(!status(&paths).contains(&record.id));
    submit(&mut product, "wake:mail", "ordinary");
    assert!(status(&paths).contains("recovery_required: 1 pending mail/wake item;"));
}

fn recovered_pause(
    product: &mut ProductStore,
    id: &str,
    held_loss: bool,
) -> (SessionViews, RootStopAnchor) {
    let turn_id = "interrupted".to_owned();
    let stop = RootStopAnchor::Turn {
        turn_id: turn_id.clone(),
    };
    let views = project_pause(
        product,
        id,
        &SessionViews::empty(),
        Some(PauseCause::RecoveryRequired { turn_id }),
    );
    if held_loss {
        product
            .write_root_interrupt(
                id,
                &AnchoredIntent {
                    anchor: views.cursor.get(),
                    ordinal: 0,
                    value: RootInterrupt {
                        anchor: stop.clone(),
                        cause: RootInterruptCause::OwnerLost,
                    },
                },
            )
            .unwrap();
    }
    (views, stop)
}

fn consent_to_resume(product: &mut ProductStore, id: &str, anchor: u64, stop: RootStopAnchor) {
    product
        .write_root_continuation(
            id,
            &AnchoredIntent {
                anchor,
                ordinal: 0,
                value: RootContinuation {
                    anchor: stop,
                    cause: RootContinuationCause::OwnerLost,
                    host_generation: 1,
                    round: 1,
                    decided_at_ms: 1,
                    decision: RootContinuationDecision::NewTurn {
                        command_id: "plak:resume-loss:root:fixture".to_owned(),
                        withdrawn: Vec::new(),
                    },
                },
            },
        )
        .unwrap();
}

fn project_pause(
    product: &mut ProductStore,
    id: &str,
    before: &SessionViews,
    cause: Option<PauseCause>,
) -> SessionViews {
    let mut after = before.clone();
    let anchor = before.cursor.get() + 1;
    after.cursor = FeedSeq::new(anchor);
    after.state.pause = Some(Anchored {
        anchor,
        value: cause,
    });
    product
        .commit_projection_views(id, &ViewWrites::between(before, &after), &after.state)
        .unwrap();
    after
}

fn finish_child(tree: &mut ProductAgentTree) {
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.to_owned(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
}

fn status(paths: &crate::home::paths::PlakPaths) -> String {
    let mut out = Vec::new();
    crate::session_attention::write_status(paths, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}
