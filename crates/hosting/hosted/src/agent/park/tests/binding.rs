use super::*;

#[tokio::test]
async fn placement_hold_cut_prepared_wake_binding_withdraws_unread_batch_on_release_and_stop() {
    for stop in [false, true] {
        let vcs = Arc::new(crate::occupancy_vcs::fake::FakeVcs::new());
        let fixture = fixture(
            json!([
                start_job("read line"), {"output": {"text": "park"}}, held("wake")
            ]),
            |tree| {
                tree.set_vcs(vcs.clone());
                leaf(tree)
            },
        )
        .await;
        fixture.until("park", parked).await;
        let path = fixture
            .runtime
            .read(|tree| tree.node(&fixture.child).unwrap().path.clone())
            .unwrap();
        let reached = Arc::new(tokio::sync::Notify::new());
        let once = std::sync::atomic::AtomicBool::new(false);
        hooks::set_before_wake_binding(
            &fixture.child,
            Arc::new({
                let (vcs, path, reached) = (vcs.clone(), path.clone(), reached.clone());
                move || {
                    if !once.swap(true, std::sync::atomic::Ordering::AcqRel) {
                        vcs.set_placement_ready(path.clone(), false);
                        reached.notify_one();
                    }
                    Box::pin(async {})
                }
            }),
        );
        fixture
            .mutate(|tree| {
                tree.send_agent_message(
                    "root",
                    &SendAgentMessageRequest {
                        acknowledge_recovery: None,
                        refresh: None,
                        agent_id: fixture.child.clone(),
                        message: "binding-cut mail".into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            })
            .await;
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        fixture
            .until("unadmitted wake withdrawn", |tree, child| {
                tree.commands().values().any(|command| {
                    command.recipient == child
                        && command.kind == crate::agent_tree::AgentCommandKind::Wake
                        && command.withdrawn
                })
            })
            .await;
        let withdrawn = fixture
            .runtime
            .read(|tree| {
                tree.commands()
                    .values()
                    .find(|command| {
                        command.recipient == fixture.child
                            && command.kind == crate::agent_tree::AgentCommandKind::Wake
                            && command.withdrawn
                    })
                    .unwrap()
                    .command_id
                    .clone()
            })
            .unwrap();
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(tree.attempt(&fixture.child).unwrap().usage.turns, 1);
                assert_eq!(tree.wake_mail(&fixture.child).len(), 1);
                assert_eq!(
                    tree.commands()
                        .values()
                        .filter(|command| command.recipient == fixture.child
                            && command.kind == crate::agent_tree::AgentCommandKind::Wake
                            && command.unresolved())
                        .count(),
                    0
                );
            })
            .unwrap();
        if stop {
            fixture
                .mutate(|tree| {
                    tree.interrupt_agent(
                        "root",
                        &crate::agent_tree_protocol::AttemptScopedRequest {
                            agent_id: fixture.child.clone(),
                            expected_attempt_id: None,
                        },
                        None,
                    )
                    .unwrap();
                })
                .await;
            fixture.runtime.cancel_live(&fixture.child).await.unwrap();
            fixture
                .until("stopped waiter", |tree, child| !tree.has_turn_waiter(child))
                .await;
            fixture
                .runtime
                .read(|tree| {
                    assert_eq!(
                        tree.node(&fixture.child).unwrap().status,
                        NodeStatus::Interrupted
                    );
                    assert_eq!(tree.wake_mail(&fixture.child).len(), 1);
                    assert_eq!(tree.pending_mail("root").len(), 0);
                })
                .unwrap();
        } else {
            vcs.set_placement_ready(path, true);
            fixture.held_reached().await;
            fixture.until("one wake", turns(2)).await;
            fixture
                .runtime
                .read(|tree| {
                    let accepted = tree
                        .commands()
                        .values()
                        .filter(|command| {
                            command.recipient == fixture.child
                                && command.kind == crate::agent_tree::AgentCommandKind::Wake
                                && command.fact == Some(crate::agent_tree::CommandFact::Accepted)
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(accepted.len(), 1);
                    assert_ne!(
                        accepted[0].command_id, withdrawn,
                        "a withdrawn receipt was readmitted"
                    );
                    assert!(!accepted[0].withdrawn);
                    assert!(accepted[0].text.contains("binding-cut mail"));
                })
                .unwrap();
            fixture.submit().await;
            fixture.until("submitted", settled).await;
        }
        fixture
            .registry
            .stop_all(Duration::from_millis(250))
            .await
            .unwrap();
    }
}
