use super::*;
use plakous_offline_crash_harness::gate::GateHandle;

#[tokio::test]
async fn live_child_lease_fence_continues_once_then_holds_if_its_continuation_fences() {
    let fixture = Fixture::new("live-child-lease-fence");
    saturated_retask::pin_native_template(&fixture).await;
    fixture.runtime.set_continue_after_loss(true);
    let first = GateHandle::new();
    let continued = GateHandle::new();
    let binding =
        |gate: &GateHandle| {
            let workspace = fixture.root.path().to_path_buf();
            let evidence = fixture.evidence.clone();
            let script = Arc::new(serde_json::from_value(json!({
            "version": 1, "steps": [{"gate": gate.script(), "output": {"text": "response"}}],
        })).unwrap());
            move || {
                let controller =
                    BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)
                        .unwrap();
                plakous_offline_crash_harness::projected_scripted_runtime_binding(
                    &workspace,
                    controller,
                    Arc::clone(&script),
                )
                .map_err(|error| error.to_string())
            }
        };
    let reopen = binding(&continued);
    let _installed = fixture.binding.install(Arc::new(move |_, _, _| reopen()));
    let id = fixture.spawn();
    let (session_id, turn) = watch_child(&fixture, &id, binding(&first)().unwrap()).await;
    tokio::time::timeout(Duration::from_secs(30), first.entered())
        .await
        .unwrap();
    plakous::test_session_lease::expire_next_publication_renewal(session_id);
    first.release();
    tokio::time::timeout(Duration::from_secs(30), continued.entered())
        .await
        .expect("the live waiter must start the first lease-loss continuation");
    assert_eq!(continuation_decisions(&fixture), 1);
    plakous::test_session_lease::expire_next_publication_renewal(session_id);
    continued.release();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let hold = fixture
                .runtime
                .read(|tree| tree.loss_continuation(&id))
                .unwrap();
            if let Some(loss) = hold
                && loss.state == crate::agent_tree::LossContinuationState::Held
                && loss.interrupted_continuation
            {
                assert!(
                    matches!(loss.anchor, crate::agent_tree::ContinuationAnchor::Turn { turn_id }
                    if turn_id != turn.to_string())
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the second loss holds on the continuation turn");
    recover_tree(&fixture.tree_id).await;
    assert_eq!(continuation_decisions(&fixture), 1);
    assert_eq!(fixture.dispatches(), 2, "no third provider dispatch");
    crate::topology_runtime::force_release(&fixture.tree_id).await;
}

async fn watch_child(
    fixture: &Fixture,
    id: &str,
    binding: SessionRuntimeBinding,
) -> (plakous::model::SessionId, plakous::model::TurnId) {
    let sessions = fixture.sessions();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            spawn_child_operation_id(&fixture.tree_id, id).unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("expected the first session");
    };
    let handle = session
        .agent()
        .start(CommandId::new("test:start").unwrap(), "start")
        .await
        .unwrap()
        .result()
        .unwrap()
        .clone();
    let turn = handle.turn_id();
    let control = session.agent().turn_control(turn).unwrap();
    let locator = session.locator();
    let session_id = locator.session_id();
    fixture.bind_turn(id, &locator, turn);
    let live = crate::agent::live::ChildActorHandle::new(
        &fixture.runtime.driver().release_owners(),
        AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions,
            opened: Some(session),
            recovered_turn: None,
            locator,
            topology_owner: None,
        },
        turn,
        control,
    );
    let watcher = live.watcher();
    assert!(
        fixture
            .runtime
            .try_occupy_live(id.to_owned(), Box::new(live))
            .await
            .is_ok()
    );
    watcher.watch(
        crate::agent::children::TurnWaiterRegistration::register(fixture.runtime.driver(), id)
            .await
            .unwrap(),
        Arc::clone(&fixture.runtime.live),
        None,
        None,
        handle,
    );
    (session_id, turn)
}
