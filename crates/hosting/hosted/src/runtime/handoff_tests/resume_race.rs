use super::*;
use crate::agent_tree::{
    CommandFact, ContinuationIntent, MemoryTreeStore, ProductAgentTree, TreeStore,
};
use crate::agent_tree_protocol::{AttemptStatus, MessagePurpose, SpawnAgentRequest, WriterMode};
use crate::runtime::root_loss_open_tests::Lost;
use crate::runtime::test_kit::{scripted_caller_topology_tools, scripted_root_topology};
use crate::topology_runtime::{
    ProductTopologyRuntime, TopologyHubAttach, resolve_topology_runtime,
};
use plakous::agent::{CommandId, TurnResultOutcome};
use plakous::sessions::feed::{FeedFact, FeedRecord, FeedTurnOutcome};
use plakous::sessions::{
    CreateSessionOutcome, CreateSessionRequest, SessionLocator, SessionOperationId,
};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, scripted_pi_runtime_binding_with_extensions,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const RESULT: &str = r#"{"ok":true}"#;

/// Register the scripted binding before opening each host, through the
/// ordinary custody take and recovery path.
async fn attach(
    config: &crate::runtime::HostConfig<TestExtension>,
    store: Arc<dyn TreeStore>,
    child: Arc<crate::runtime::ScriptedChildBinding>,
) -> Arc<ProductTopologyRuntime> {
    let id = &config.session.id;
    let runtime = resolve_topology_runtime(
        id,
        true,
        TopologyHubAttach::AttachRoot,
        || Ok(store),
        None,
        |store| async move {
            let vcs = Arc::new(crate::occupancy_vcs::fake::FakeVcs::new());
            let mut tree = if let Some(saved) = store.load(id)? {
                ProductAgentTree::reopen(saved)
            } else {
                ProductAgentTree::bootstrap_root_for_test_with(
                    vcs.clone(),
                    id,
                    "root",
                    "owner",
                    config.cwd.clone(),
                    "test-tuple",
                    WriterMode::SingleLease,
                )
                .unwrap()
            };
            tree.set_vcs(vcs);
            tree.apply_reopen_law();
            let revision = store.save_in_custody(&tree)?;
            tree.acknowledge_revision(revision);
            Ok(tree)
        },
    )
    .await
    .unwrap();
    scripted_root_topology(
        runtime.clone(),
        id,
        &config.cwd,
        config.state_roots.clone(),
        child,
        config.resume_after_loss == crate::runtime::ResumeAfterLoss::Continue,
    )
    .await
    .unwrap();
    runtime
}

async fn park_child(
    runtime: &ProductTopologyRuntime,
    roots: &crate::agent::ProductStateRoots,
    binding: &crate::runtime::ScriptedChildBinding,
) -> String {
    let id = runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map(|spawn| spawn.agent_id)
                .map_err(|error| error.message)
        })
        .await
        .unwrap();
    let path = runtime.read(|tree| tree.nodes()[&id].path.clone()).unwrap();
    let CreateSessionOutcome::Created { session, .. } = roots
        .child_runtime_sessions()
        .unwrap()
        .create(CreateSessionRequest::new(
            SessionOperationId::new("parked-child").unwrap(),
            binding(&id, path.to_str().unwrap(), roots).unwrap(),
        ))
        .await
        .unwrap()
    else {
        panic!("the child session was not created");
    };
    let submission = session
        .agent()
        .start_instruction(CommandId::new("assignment").unwrap(), "review the diff")
        .await
        .unwrap();
    let turn = submission.acknowledgement().turn_id().unwrap().to_string();
    let result = submission.result().unwrap().clone().result().await.unwrap();
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let locator = session.release().await.unwrap().to_string();
    runtime
        .mutate_then_emit(false, {
            let id = id.clone();
            move |tree| {
                tree.bind_session_locator(&id, locator)
                    .and_then(|()| tree.bind_attempt_turn(&id, turn))
                    .and_then(|()| tree.release_attempt_turn(&id))
                    .and_then(|()| tree.park_child(&id, Vec::new()))
                    .map_err(|error| error.message)
            }
        })
        .await
        .unwrap();
    id
}

fn child_binding(
    tree: String,
    evidence: std::path::PathBuf,
    steps: impl Fn() -> serde_json::Value + Send + Sync + 'static,
) -> Arc<crate::runtime::ScriptedChildBinding> {
    Arc::new(move |agent, path, roots| {
        let workspace = std::path::Path::new(path);
        std::fs::create_dir_all(workspace).unwrap();
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence).unwrap();
        let runtime = crate::topology_runtime::lookup(&tree).unwrap();
        let tools =
            scripted_caller_topology_tools(runtime, agent, workspace, roots.clone()).unwrap();
        scripted_pi_runtime_binding_with_extensions(workspace, controller, &script(&steps()), tools)
            .map_err(|error| error.to_string())
    })
}

#[test]
fn operator_resume_cannot_overtake_the_handed_off_child_recovery() {
    for manual in [false, true] {
        handoff_child_while_recovery_is_pending(if manual {
            ResumeScenario::Overtake
        } else {
            ResumeScenario::Automatic
        });
    }
}

#[test]
fn operator_resume_retries_a_deferred_continuation_with_its_original_command() {
    handoff_child_while_recovery_is_pending(ResumeScenario::Retry);
}

#[derive(Clone, Copy, PartialEq)]
enum ResumeScenario {
    Automatic,
    Overtake,
    Retry,
}

fn handoff_child_while_recovery_is_pending(scenario: ResumeScenario) {
    let retry = scenario == ResumeScenario::Retry;
    let home = tempfile::tempdir().unwrap();
    let (config, evidence) = scripted_host_with(
        home.path(),
        script(&json!([{"output": {"text": "ready for handoff"}}])),
    );
    let lost = Lost {
        release_error: None,
        store: config.store.clone(),
        state_roots: config.state_roots.clone(),
        id: config.session.id.clone(),
        evidence,
        seen: Vec::new(),
    };
    let trees: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
    let child_evidence = home.path().join("child-evidence.jsonl");
    let first = child_binding(
        lost.id.clone(),
        child_evidence.clone(),
        || json!([{"output": {"text": "park before handoff"}}]),
    );
    let executor = tokio::runtime::Runtime::new().unwrap();
    let runtime = executor.block_on(attach(&config, trees.clone(), first.clone()));
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, "first host idle", idle);
    send_attached_action(&actions, UserAction::Prompt("save the root session".into()));
    wait_for(&events, &mut seen, "first host turn", turn_completed);
    wait_for(&events, &mut seen, "first host idle after the turn", idle);
    let child = executor.block_on(park_child(&runtime, &lost.state_roots, &*first));
    assert!(runtime.read(|tree| tree.is_parked(&child)).unwrap());
    if retry {
        let child = child.clone();
        executor
            .block_on(runtime.mutate_then_emit(false, move |tree| {
                tree.mark_owner_lost(&child);
                Ok(())
            }))
            .unwrap();
    }
    actions.request_handoff_and_release();
    worker.join().unwrap().unwrap();
    drop(runtime);

    let mut next = next_host(home.path(), &lost, &json!([]), lost.store.clone());
    next.resume_after_loss = if retry {
        crate::runtime::ResumeAfterLoss::Hold
    } else {
        crate::runtime::ResumeAfterLoss::Continue
    };
    let (entered, release) = crate::agent::resume_race_hook::install(lost.id.clone());
    let ready = release.subscribe();
    let answer_gate = home.path().join("answer-after-bind");
    let gate = answer_gate.clone();
    // Model a provider that cannot settle work in the reattach window.
    // The schedule, not the provider fault itself, is what Resume changes.
    let second = child_binding(lost.id.clone(), child_evidence.clone(), move || {
        if *ready.borrow() {
            json!([
                {"gate": {"file": gate}, "output": {"tool": {"name": "submit_result", "arguments": {
                    "turn_token": 1, "output_json": RESULT
                }}}, "finish": "tool_calls"},
                {"output": {"text": "done"}}
            ])
        } else {
            json!([{"fault": {"at": "in_band", "kind": "outcome_unknown"}}])
        }
    });
    let refuse = AtomicBool::new(retry);
    let second: Arc<crate::runtime::ScriptedChildBinding> = Arc::new(move |agent, path, roots| {
        if refuse.swap(false, Ordering::AcqRel) {
            return Err("injected child reopen refusal".into());
        }
        second(agent, path, roots)
    });
    let runtime = executor.block_on(attach(&next, trees.clone(), second));
    assert_eq!(
        runtime
            .read(|tree| tree.loss_continuation(&child).map(|hold| hold.cause))
            .unwrap(),
        Some(if retry {
            ContinuationCause::OwnerLost
        } else {
            ContinuationCause::Handoff
        })
    );
    let (actions, events, worker) = spawn_host_worker(next).unwrap();
    executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), entered.notified())
            .await
            .unwrap();
    });
    let mut seen = Vec::new();
    let resume = || {
        send_attached_action(
            &actions,
            UserAction::ResumeLossContinuation {
                agent_id: Some(child.clone()),
            },
        );
    };
    if scenario == ResumeScenario::Overtake {
        resume();
        wait_for(&events, &mut seen, "the explicit Resume answer", |event| {
            notice_tone(std::slice::from_ref(event), "already continuing").is_some()
                || notice_tone(std::slice::from_ref(event), "resumed ").is_some()
        });
    }
    release.send(true).unwrap();
    let deferred = if retry {
        resume();
        wait_for(&events, &mut seen, "deferred Resume", |event| {
            notice_tone(std::slice::from_ref(event), "injected child reopen refusal").is_some()
        });
        let command = runtime
            .read(|tree| {
                tree.loss_continuation(&child)
                    .filter(|loss| loss.state == crate::agent_tree::LossContinuationState::Pending)
                    .and_then(|loss| loss.command_id)
            })
            .unwrap()
            .expect("the refused reopen kept its consent");
        resume();
        wait_for(
            &events,
            &mut seen,
            "the retry answers already continuing",
            |event| notice_tone(std::slice::from_ref(event), "already continuing").is_some(),
        );
        Some(command)
    } else {
        None
    };
    executor.block_on(async {
        wait_until(&runtime, &child, "continuation admission", |tree| {
            continuation_commands(tree).any(|command| command.fact == Some(CommandFact::Accepted))
                || tree.attempt(&child).unwrap().status.is_stopped()
        })
        .await;
        std::fs::write(answer_gate, "").unwrap();
        wait_until(&runtime, &child, "attempt terminal", |tree| {
            tree.attempt(&child).unwrap().status.is_stopped()
        })
        .await;
    });
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let stored = ProductAgentTree::reopen(trees.load(&lost.id).unwrap().unwrap());
    assert_delivered_once(&executor, &lost, &stored, &child, &child_evidence, retry);
    if let Some(command) = deferred {
        assert_eq!(
            stored.command(&command).unwrap().fact,
            Some(CommandFact::Accepted)
        );
    }
    if scenario != ResumeScenario::Automatic {
        assert!(
            notice_tone(&seen, "already continuing").is_some(),
            "{seen:?}"
        );
    }
}

async fn wait_until(
    runtime: &ProductTopologyRuntime,
    child: &str,
    what: &str,
    condition: impl Fn(&ProductAgentTree) -> bool,
) {
    let reached = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if runtime.read(&condition).unwrap() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "{what}: {:?}",
        runtime
            .read(|tree| {
                (
                    tree.nodes()[child].status,
                    tree.attempt(child).unwrap().clone(),
                    tree.loss_continuation(child),
                    tree.commands().clone(),
                )
            })
            .unwrap()
    );
}

fn continuation_commands(
    tree: &ProductAgentTree,
) -> impl Iterator<Item = &crate::agent_tree::AgentCommand> {
    tree.commands().values().filter(|command| {
        command
            .command_id
            .starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX)
    })
}

fn assert_delivered_once(
    executor: &tokio::runtime::Runtime,
    lost: &Lost,
    stored: &ProductAgentTree,
    child: &str,
    evidence: &std::path::Path,
    owner_lost: bool,
) {
    let attempt = stored.attempt(child).unwrap();
    assert_eq!(
        attempt.status,
        AttemptStatus::Completed,
        "{:?}",
        attempt.failure
    );
    assert_eq!(attempt.output_json.as_deref(), Some(RESULT));
    assert_eq!(
        stored
            .operations()
            .values()
            .filter(|row| row.tool_name == ContinuationIntent::KIND)
            .count(),
        1
    );
    let resumptions = continuation_commands(stored).collect::<Vec<_>>();
    assert_eq!(resumptions.len(), 1);
    assert_eq!(resumptions[0].fact, Some(CommandFact::Accepted));
    let instruction = if owner_lost {
        crate::child_prompts::PLAK_CHILD_LOSS_RESUME_TEMPLATE
    } else {
        crate::child_prompts::PLAK_CHILD_HANDOFF_RESUME_TEMPLATE
    };
    assert!(resumptions[0].text.contains(instruction));
    let results: Vec<_> = stored
        .messages()
        .values()
        .filter(|mail| mail.from == child && mail.purpose == MessagePurpose::Result)
        .collect();
    let [mail] = results.as_slice() else {
        panic!("expected one result in the parent's mailbox, got {results:?}");
    };
    assert_eq!(mail.to, stored.root_agent_id);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&mail.body).unwrap()["output"],
        RESULT
    );
    assert_eq!(
        dispatches(evidence),
        3,
        "assignment, result call and continuation answer"
    );
    let locator: SessionLocator = stored.nodes()[child]
        .session_locator
        .as_ref()
        .unwrap()
        .parse()
        .unwrap();
    let sessions = lost.state_roots.runtime_sessions().unwrap();
    let feed = executor
        .block_on(crate::agent::read_tree_feed(
            &sessions,
            locator.session_id(),
            0,
        ))
        .unwrap();
    let terminals: Vec<_> = feed
        .records
        .iter()
        .flat_map(FeedRecord::facts)
        .filter_map(|fact| match fact {
            FeedFact::TurnTerminal {
                command_id,
                outcome,
                ..
            } if command_id.as_str() == resumptions[0].command_id => Some(*outcome),
            _ => None,
        })
        .collect();
    assert!(
        matches!(&terminals[..], [FeedTurnOutcome::Completed { .. }]),
        "{terminals:?}"
    );
}
