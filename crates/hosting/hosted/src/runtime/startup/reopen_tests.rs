#![allow(clippy::unwrap_used, reason = "offline reopen assertions")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::runtime::test_kit::scripted_mut;
use plakous::sessions::{OpenSessionOutcome, OpenSessionRequest, SessionOperationId};

fn certified_config(root: &std::path::Path) -> HostConfig<crate::runtime::test_kit::TestExtension> {
    let (mut config, _) = crate::runtime::test_support::fresh_injected_attached_host(root);
    let workspace = config.cwd.clone();
    let evidence = root.join("certified-evidence.jsonl");
    let dispatched = plakous_offline_crash_harness::evidence::read_evidence(&evidence)
        .unwrap_or_default()
        .iter()
        .filter(|row| {
            row.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
        })
        .count();
    let cursor = Arc::new(std::sync::atomic::AtomicU64::new(dispatched as u64 + 1));
    scripted_mut(&mut config).runtime = Arc::new(move |_| {
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                &evidence,
            )
            .map_err(|error| error.to_string())?;
        let script = Arc::new(serde_json::from_value(serde_json::json!({
            "version": 1, "tool_policy": "decision_required", "steps": [
                {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
                {"output": {"text": "restored terminal"}}
            ]
        })).map_err(|error| error.to_string())?);
        plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
            &workspace,
            controller,
            script,
            cursor.clone(),
        )
        .map_err(|error| error.to_string())
    });
    config
}

#[test]
#[ignore = "process owner for certified root recovery"]
fn root_certified_owner() {
    use std::io::Write as _;
    let root = std::path::PathBuf::from(std::env::var_os("PLAK_ROOT_REOPEN_FIXTURE").unwrap());
    let mut config = certified_config(&root);
    ApplicationExecutor::new()
        .unwrap()
        .block_on(async {
            let opened = build_session_from_config(&mut config, &mut Vec::new())
                .await
                .unwrap();
            let handle = opened
                .agent()
                .unwrap()
                .start(
                    plakous::agent::CommandId::new("root:certified").unwrap(),
                    "one certified turn",
                )
                .await
                .unwrap()
                .into_turn()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let view = opened
                        .agent()
                        .unwrap()
                        .attach(plakous::agent::ViewRequest::metadata())
                        .await
                        .unwrap();
                    if !view.snapshot().pending_effect_decisions().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            std::fs::write(
                root.join("certified-root.json"),
                serde_json::to_vec(&serde_json::json!({
                    "id": config.session.id, "create": config.session.create_operation_id,
                    "locator": opened.locator().to_string(), "turn": handle.id().to_string()
                }))
                .unwrap(),
            )
            .unwrap();
            println!("ROOT_CERTIFIED_READY");
            std::io::stdout().flush().unwrap();
            std::future::pending::<()>().await;
        })
        .unwrap();
}

#[test]
fn root_startup_rejoins_certified_decision_once_and_settles_waits() {
    use crate::agent_tree_protocol::{SendAgentMessageRequest, SpawnAgentRequest, WriterMode};
    use std::io::BufRead as _;
    use std::process::{Command, Stdio};
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::startup::reopen_tests::root_certified_owner",
            "--ignored",
            "--nocapture",
        ])
        .env("PLAK_ROOT_REOPEN_FIXTURE", root.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sent, ready) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if line.unwrap().contains("ROOT_CERTIFIED_READY") {
                let _ = sent.send(());
            }
        }
    });
    let reached = ready.recv_timeout(Duration::from_secs(30));
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert!(
        reached.is_ok(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("certified-root.json")).unwrap())
            .unwrap();
    let mut config = certified_config(root.path());
    config.session.id = saved["id"].as_str().unwrap().into();
    config.session.create_operation_id = saved["create"].as_str().unwrap().into();
    config
        .session
        .bind_plakous_locator(saved["locator"].as_str().unwrap().parse().unwrap())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        &config.session.id,
        "root",
        "owner",
        config.cwd.clone(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let leaf = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    let mail = tree
        .send_agent_message(
            &leaf,
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "retained reply".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    let envelope = tree.pending_mail("root").pop().unwrap();
    tree.remember_success(
        "root-retained-wait",
        "root",
        "wait_agent",
        &serde_json::json!({}),
        &serde_json::json!({"output": {}, "acknowledged": [envelope], "newly_delivered": []}),
    )
    .unwrap();
    tree.acknowledge_mail(&mail.message_id).unwrap();
    let runtime =
        crate::topology_runtime::insert_root(config.session.id.clone(), tree, None).unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    executor.block_on(async {
        let mut opened = build_session_from_config(&mut config, &mut Vec::new()).await.unwrap();
        assert!(opened.has_recovered_turn(), "startup must preserve RestoredWorking");
        super::super::result_mail::settle_waits(&config.topology_rows(), &config.session, &opened).await.unwrap();
        assert_eq!(runtime.read(|tree| tree.retained_wait_receipts("root").len()).unwrap(), 1);
        let turn = opened.take_recovered_turn().unwrap();
        assert_eq!(turn.turn_id().to_string(), saved["turn"].as_str().unwrap());
        let decision = turn.pending_decision().unwrap().clone();
        turn.control().allow_effect(decision, plakous::tools::EffectDecisionAuditId::new("root:restored-approval").unwrap()).await.unwrap();
        let result = turn.result().result().await.unwrap();
        assert_eq!(result.turn_id(), turn.turn_id());
        assert!(matches!(result.outcome(), plakous::agent::TurnResultOutcome::Completed(_)), "{:?}", result.outcome());
        super::super::result_mail::settle_terminal_waits(&config.topology_rows(), &config.session, &opened,
            crate::agent::TerminalExchange::of(&result)).await.unwrap();
        assert!(runtime.read(|tree| tree.retained_wait_receipts("root").is_empty()).unwrap());
        assert_eq!(runtime.read(|tree| tree.pending_mail("root").len()).unwrap(), 1);
        let feed = crate::agent::read_tree_feed(opened.sessions(), opened.locator().session_id(), 0).await.unwrap();
        assert_eq!(feed.records.iter().flat_map(plakous::sessions::feed::FeedRecord::facts).filter(|fact| matches!(fact,
            plakous::sessions::feed::FeedFact::TurnTerminal { turn_id, .. } if *turn_id == turn.turn_id())).count(), 1);
        let locator = opened.release().await.unwrap();
        config.session.plakous_locator = Some(locator);
        let again = build_session_from_config(&mut config, &mut Vec::new()).await.unwrap();
        assert!(!again.has_recovered_turn());
        again.release().await.unwrap();
        crate::topology_runtime::force_release(&config.session.id).await;
    }).unwrap();
    let evidence = plakous_offline_crash_harness::evidence::read_evidence(
        root.path().join("certified-evidence.jsonl"),
    )
    .unwrap();
    assert_eq!(
        evidence
            .iter()
            .filter(|row| row.kind
                == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched)
            .count(),
        2
    );
    assert_eq!(
        evidence
            .iter()
            .filter(|row| row.kind
                == plakous_offline_crash_harness::evidence::EvidenceKind::EffectDispatched)
            .count(),
        1
    );
}

struct ProjectionFailure;
impl StoreFaults for ProjectionFailure {
    fn before(&self, call: StoreCall, _: Option<&SessionRecord>) -> Result<(), String> {
        if call == StoreCall::LoadViews {
            Err("injected projection race refusal".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn root_stale_open_reprojects_once_then_opens_the_new_head() {
    root_stale_race(1, false);
}

#[test]
fn root_second_stale_open_refuses_contention_without_a_third_projection() {
    root_stale_race(2, false);
}

#[test]
fn root_stale_open_with_projection_debt_refuses_without_retry() {
    root_stale_race(1, true);
}

fn root_stale_race(races: usize, projection_failure: bool) {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = crate::runtime::test_support::fresh_injected_attached_host(root.path());
    let executor = ApplicationExecutor::new().unwrap();
    let opened = executor
        .block_on(build_session_from_config(&mut config, &mut Vec::new()))
        .unwrap()
        .unwrap();
    let head = executor.block_on(opened.release()).unwrap().unwrap();
    config.session.bind_plakous_locator(head).unwrap();
    config.store.save(&mut config.session).unwrap();
    let latest = Arc::new(Mutex::new(head));
    let opens = Arc::new(AtomicUsize::new(0));
    let binding = scripted_mut(&mut config).runtime.clone();
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let jobs = config.state_roots.job_registry().clone();
    scripted_mut(&mut config).runtime = Arc::new({
        let latest = latest.clone();
        let opens = opens.clone();
        move |role| {
            let n = opens.fetch_add(1, Ordering::SeqCst);
            if n < races {
                let sessions = sessions.clone();
                let binding = binding.clone();
                let jobs = jobs.clone();
                let latest = latest.clone();
                thread::spawn(move || {
                    let executor = ApplicationExecutor::new().unwrap();
                    let head = *latest.lock().unwrap();
                    let runtime = binding(role)
                        .unwrap()
                        .with_job_host(jobs.for_agent("root", "root"));
                    executor
                        .block_on(async {
                            let OpenSessionOutcome::Opened { session, .. } = sessions
                                .open(OpenSessionRequest::new(
                                    SessionOperationId::new(format!("test:second-owner:{n}"))
                                        .unwrap(),
                                    head,
                                    runtime,
                                ))
                                .await
                                .unwrap()
                            else {
                                panic!("second owner did not open")
                            };
                            session
                                .agent()
                                .prompt("second owner commits between projection and open")
                                .await
                                .unwrap()
                                .result()
                                .await
                                .unwrap();
                            *latest.lock().unwrap() = session.release().await.unwrap();
                        })
                        .unwrap();
                })
                .join()
                .unwrap();
            }
            binding(role)
        }
    });
    if projection_failure {
        config.store = Arc::new(FaultStore::new(config.store.clone(), ProjectionFailure));
    }
    crate::runtime::projection::probe::track(&config.session.id);
    let result = executor
        .block_on(build_session_from_config(&mut config, &mut Vec::new()))
        .unwrap();
    let expected_opens = if projection_failure { 1 } else { 2 };
    assert_eq!(opens.load(Ordering::SeqCst), expected_opens);
    assert_eq!(
        crate::runtime::projection::probe::take(&config.session.id).open(),
        expected_opens
    );
    if races == 1 && !projection_failure {
        let opened = result.unwrap_or_else(|error| panic!("{}", error.message()));
        assert_eq!(opened.locator(), *latest.lock().unwrap());
        executor.block_on(opened.release()).unwrap().unwrap();
    } else {
        let error = result.err().unwrap();
        assert!(
            error.message().contains(if projection_failure {
                "projection debt is not a recovery fence"
            } else {
                "another process is using this session"
            }),
            "{}",
            error.message()
        );
    }
}
