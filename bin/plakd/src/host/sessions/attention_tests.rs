use super::*;
use plakous::sessions::feed::FeedSeq;
use plakous_hosted::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
use plakous_hosted::sessions::projector::{
    Anchored, AnchoredIntent, PauseCause, RootContinuation, RootContinuationCause,
    RootContinuationDecision, RootInterrupt, RootInterruptCause, RootStopAnchor, SessionViews,
    ViewWrites,
};

#[test]
fn status_and_directory_show_blocked_mail_and_clear_with_operator_action() {
    for held_loss in [false, true] {
        let (temp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
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
        let stop = RootStopAnchor::Turn {
            turn_id: "interrupted".to_owned(),
        };
        let mut before = SessionViews::empty();
        if held_loss {
            product
                .write_root_interrupt(
                    &record.id,
                    &AnchoredIntent {
                        anchor: 0,
                        ordinal: 0,
                        value: RootInterrupt {
                            anchor: stop.clone(),
                            cause: RootInterruptCause::OwnerLost,
                        },
                    },
                )
                .unwrap();
        } else {
            before = project_pause(
                &mut product,
                &record.id,
                &before,
                Some(PauseCause::RecoveryRequired {
                    turn_id: "interrupted".to_owned(),
                }),
            );
        }
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);
        assert!(!status(&paths).contains(&record.id));

        finish_child(&mut tree);
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();

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
            consent_to_resume(&mut product, &record.id, stop);
        } else {
            project_pause(&mut product, &record.id, &before, None);
        }
        // Clearing the blocker is sufficient; mail need not have been read yet.
        assert!(!status(&paths).contains(&record.id));
        assert!(!sessions.rows(None, None, false).unwrap()[0].needs_operator);
    }
}

fn consent_to_resume(product: &mut ProductStore, id: &str, stop: RootStopAnchor) {
    product
        .write_root_continuation(
            id,
            &AnchoredIntent {
                anchor: 0,
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
