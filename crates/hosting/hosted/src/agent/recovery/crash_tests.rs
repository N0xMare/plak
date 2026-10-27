//! Real child owners killed during a parked wake and an initial model turn.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "process crash fixture"
)]

use std::io::{BufRead, Read, Write};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use plakous::agent::CommandId;
use plakous::sessions::{CreateSessionOutcome, CreateSessionRequest};
use plakous_offline_crash_harness::gate::GateHandle;
use plakous_offline_crash_harness::script::ScriptPlan;
use plakous_offline_crash_harness::{BoundaryController, scripted_pi_runtime_binding};
use plakous_recursive_subagents_store::SqliteTreeStore;
use serde_json::json;

use crate::agent::children::{ChildTurnIdentity, spawn_child_operation_id};
use crate::agent::{
    ApiTopologyParent, AttachedCodingSession, NATIVE_RUN_MAX_MODEL_ROUNDS, ProductStateRoots,
    SpawnParent,
};
use crate::agent_tree::{ProductAgentTree, TreeStore as _};
use crate::agent_tree_protocol::{
    NodeStatus, SendAgentMessageRequest, SpawnAgentRequest, WriterMode,
};
use crate::binding::{CompositionSelection, ResolvedProtocol};
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger};

const ROOT_ENV: &str = "PLAK_CHILD_REOPEN_FIXTURE_ROOT";
const MODE_ENV: &str = "PLAK_CHILD_REOPEN_FIXTURE_MODE";
const READY: &str = "CHILD_CUT_READY";

type LiveFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct CancelOnly(crate::agent::children::ProductLiveCanceller);

impl crate::agent_tree_tools::ChildController for CancelOnly {
    fn services(&self) -> crate::agent_tree_tools::ChildServices {
        crate::agent_tree_tools::ChildServices::CANCEL
    }
    fn cancel_agents(
        &self,
        ids: Vec<String>,
        signalled: tokio::sync::oneshot::Sender<()>,
    ) -> LiveFuture<
        '_,
        Result<
            crate::agent_tree_tools::LiveCancelOutcome,
            crate::agent_tree_tools::LiveCancelFailure,
        >,
    > {
        self.0.cancel_agents(ids, signalled)
    }
}

/// Holds a real runtime's release result before the hub can publish its feed.
struct UnpublishedRelease {
    inner: Box<dyn crate::topology_runtime::LiveChildHandle>,
    released: Arc<tokio::sync::Notify>,
}

impl crate::topology_runtime::LiveChildHandle for UnpublishedRelease {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        self.inner.agent()
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        self.inner.turn_id()
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        self.inner.turn_control()
    }
    fn follow_up(
        &self,
        command: CommandId,
        body: String,
    ) -> LiveFuture<'static, Result<(), String>> {
        self.inner.follow_up(command, body)
    }
    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        self.inner.follow_up_barrier()
    }
    fn restart(
        &mut self,
        driver: crate::agent_tree_tools::TreeDriver,
        command: CommandId,
        prompt: String,
        delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> LiveFuture<'_, Result<plakous::agent::TurnResultHandle, String>> {
        self.inner.restart(driver, command, prompt, delivery)
    }
    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> LiveFuture<'static, crate::topology_runtime::LiveRelease> {
        self.inner.release(driver)
    }
    fn cancel_and_release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> LiveFuture<'static, crate::topology_runtime::LiveRelease> {
        Box::pin(async move {
            let release = self.inner.cancel_and_release(driver, dispatched).await;
            assert!(release.failure.is_none());
            self.released.notify_one();
            std::future::pending().await
        })
    }
}

struct Fixture {
    runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
    parent: SpawnParent,
    root: PathBuf,
    tree_id: String,
    plan: Arc<ScriptPlan>,
    reopen_attempts: Arc<AtomicUsize>,
    retry_started: Arc<tokio::sync::Notify>,
}

impl Fixture {
    fn new(root: &Path, mode: &str, gate: Option<&GateHandle>, restored: bool) -> Self {
        std::fs::create_dir_all(root.join("plak")).unwrap();
        let tree_id = format!("child-sigkill-{mode}");
        let gate = gate.map_or_else(
            || json!({"file": root.join("never-release")}),
            |gate| serde_json::to_value(gate.script()).unwrap(),
        );
        let held = json!({"stream": [{"gate": gate}], "output": {"text": "must not redispatch"}});
        let certified = mode == "certified";
        let steps = if certified && restored {
            json!([{"output": {"text": "certified terminal"}}])
        } else if certified {
            json!([
                {"output": {"tool": {"name": "write", "arguments": {"path": "child.txt", "content": "one effect"}}}, "finish": "tool_calls"},
                {"output": {"text": "certified terminal"}}
            ])
        } else if mode == "wake" && restored {
            json!([{"output": {"text": "relaunch terminal"}}])
        } else if mode == "wake" {
            json!([{"output": {"text": "park point"}}, held])
        } else if matches!(mode, "idle" | "wake-prepared") {
            json!([{"output": {"text": "park point"}}, {"output": {"text": "relaunch terminal"}}])
        } else {
            json!([held])
        };
        let plan: Arc<ScriptPlan> = Arc::new(
            serde_json::from_value(json!({"version": 1, "steps": steps,
                "tool_policy": if certified { "decision_required" } else { "allow" }
            }))
            .unwrap(),
        );
        let roots = ProductStateRoots::with_doubles(root.join("plak"), root.join("state"));
        let reopen_attempts = Arc::new(AtomicUsize::new(0));
        let retry_started = Arc::new(tokio::sync::Notify::new());
        let binding = crate::runtime::test_kit::SwitchableChildRuntime::new({
            let root = root.to_owned();
            let plan = plan.clone();
            let reopen_attempts = reopen_attempts.clone();
            let retry_started = retry_started.clone();
            Arc::new(move |_, _, _| {
                if restored && reopen_attempts.fetch_add(1, Ordering::SeqCst) >= 1 {
                    retry_started.notify_one();
                }
                let controller = BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.join("dispatch.jsonl"),
                )
                .map_err(|error| error.to_string())?;
                scripted_pi_runtime_binding(&root, controller, &plan)
                    .map_err(|error| error.to_string())
            })
        });
        let roots = binding.with_roots(roots);
        let store = Arc::new(SqliteTreeStore::open(&root.join("tree.sqlite3")).unwrap());
        let tree = if restored {
            ProductAgentTree::reopen(store.load(&tree_id).unwrap().unwrap())
        } else {
            ProductAgentTree::bootstrap_root_for_test(
                &tree_id,
                "root",
                "owner",
                root.to_owned(),
                "selected-tuple",
                WriterMode::SingleLease,
            )
            .unwrap()
        };
        let runtime =
            crate::topology_runtime::insert_root_with(&tree_id, tree, store, None).unwrap();
        let parent = SpawnParent::Api(ApiTopologyParent {
            created_output_bound: None,
            prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
            product_home: None,
            tool_selection: crate::tool_plan::NodeToolSelection::default(),
            workspace_root: root.to_owned(),
            cwd: root.to_owned(),
            plan_state_authority: tree_id.clone(),
            composition: CompositionSelection::Direct,
            topology_tree_id: Some(tree_id.clone()),
            topology_caller_id: None,
            state_roots: roots,
            model: "vendor/parent".into(),
            effort: None,
            target: crate::binding::ApiTarget::OpenRouter {
                wire: ResolvedProtocol::Chat,
            },
            scheduled_compaction: true,
            max_model_rounds: NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_enabled: true,
        });
        runtime.remember_rejoin_spec(Arc::new(parent.clone()));
        Self {
            runtime,
            parent,
            root: root.to_owned(),
            tree_id,
            plan,
            reopen_attempts,
            retry_started,
        }
    }

    fn binding(&self) -> plakous::sessions::SessionRuntimeBinding {
        let controller = BoundaryController::quiet_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::None,
            self.root.join("dispatch.jsonl"),
        )
        .unwrap();
        scripted_pi_runtime_binding(&self.root, controller, &self.plan).unwrap()
    }

    async fn cut(&self, mode: &str, gate: &GateHandle) {
        let id = self
            .runtime
            .mutate_then_emit(false, |tree| {
                let name = crate::agent_tree_protocol::TemplateName::parse("review").unwrap();
                let mut binding = tree.snapshot.binding(&name).unwrap().clone();
                binding.max_turns = Some(if matches!(mode, "wake" | "idle" | "wake-prepared") {
                    2
                } else {
                    1
                });
                tree.pin_template("review", binding);
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map(|spawned| spawned.agent_id)
                    .map_err(|error| error.message)
            })
            .unwrap();
        let sessions = self.parent.state_roots().runtime_sessions().unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                spawn_child_operation_id(&self.tree_id, &id).unwrap(),
                self.binding(),
            ))
            .await
            .unwrap()
        else {
            panic!("new child")
        };
        std::fs::write(
            self.root.join("created-locator"),
            session.locator().to_string(),
        )
        .unwrap();
        self.runtime
            .mutate_then_emit(false, |tree| {
                tree.record_child_session(&id, &session.locator().to_string())
                    .map_err(|error| error.message)?;
                tree.record_command(crate::agent_tree::AgentCommand::intent(
                    "child-crash:start",
                    &id,
                    tree.attempt(&id).map(|attempt| attempt.attempt_id.clone()),
                    crate::agent_tree::AgentCommandKind::Start,
                    "assignment",
                    Vec::new(),
                ))
                .map_err(|error| error.message)
            })
            .unwrap();
        let submission = session
            .agent()
            .start(CommandId::new("child-crash:start").unwrap(), "assignment")
            .await
            .unwrap();
        let result = submission.result().unwrap().clone();
        let turn = result.turn_id();
        let control = submission.turn().unwrap().control();
        if matches!(mode, "wake" | "idle" | "wake-prepared") {
            result.result().await.unwrap();
            self.runtime
                .mutate_then_emit(false, |tree| {
                    if mode != "idle" {
                        tree.send_agent_message(
                            "root",
                            &SendAgentMessageRequest {
                                refresh: None,
                                agent_id: id.clone(),
                                message: "wake now".into(),
                                purpose: None,
                                priority: None,
                                in_reply_to: None,
                                output_schema_json: None,
                            },
                            None,
                        )
                        .map_err(|error| error.message)?;
                    }
                    tree.record_child_finalization(
                        &id,
                        &turn.to_string(),
                        crate::agent_tree::ChildFinalizationDecision::Park,
                        true,
                        None,
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
        } else if mode == "certified" {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let view = session
                        .agent()
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
        } else {
            gate.entered().await;
        }
        let feed = crate::agent::read_tree_feed(&sessions, session.locator().session_id(), 0)
            .await
            .unwrap();
        self.runtime
            .mutate_then_emit(false, |tree| {
                feed.apply(tree, &id)
                    .map(|_| ())
                    .map_err(|error| error.message)
            })
            .unwrap();
        if matches!(mode, "idle" | "wake-prepared") {
            if mode == "wake-prepared" {
                let command = self
                    .runtime
                    .read(|tree| crate::agent::park::wake_command(tree, &id).unwrap())
                    .unwrap();
                crate::agent::ledger::ChildLedger::new(self.runtime.driver(), &id)
                    .prepare(CommandIntent {
                        command_id: &command,
                        kind: CommandKind::Wake,
                        base_text: crate::child_prompts::PLAK_CHILD_WAKE_PROMPT,
                        batch: BatchRule::WakeAll,
                    })
                    .unwrap();
            }
            cut_ready(&id).await;
        }
        let identity = ChildTurnIdentity {
            attempt: self
                .runtime
                .read(|tree| tree.attempt(&id).unwrap().attempt_id.clone())
                .unwrap(),
            turn,
        };
        let attached = AttachedCodingSession {
            output_bound_rederived: None,
            reachability_probe: None,
            sessions: sessions.clone(),
            locator: session.locator(),
            opened: Some(session),
            recovered_turn: None,
            topology_owner: None,
        };
        let released = Arc::new(tokio::sync::Notify::new());
        let live: Box<dyn crate::topology_runtime::LiveChildHandle> = Box::new(
            crate::agent::live::ProductLiveChild::new(attached, turn, control),
        );
        let live = if matches!(mode, "retire-unpublished" | "retire-tool") {
            Box::new(UnpublishedRelease {
                inner: live,
                released: released.clone(),
            }) as Box<dyn crate::topology_runtime::LiveChildHandle>
        } else {
            live
        };
        self.runtime
            .try_occupy_live(id.clone(), live)
            .await
            .map_err(|_| "live slot")
            .unwrap();
        if mode.starts_with("retire-") {
            if mode == "retire-tool" {
                let outcome = crate::agent_tree_tools::invoke_granted_topology_tool_on(
                    self.runtime.driver().clone(),
                    "root".into(),
                    "close_agent",
                    &json!({"agent_id": id}),
                    "retirement-cut".into(),
                    crate::agent_tree_tools::TopologyHostPorts {
                        controller: Some(Arc::new(CancelOnly(
                            crate::agent::children::ProductLiveCanceller {
                                runtime: self.runtime.clone(),
                            },
                        ))),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                assert_eq!(outcome.kind(), plakous::tools::ToolOutcomeKind::Success);
                released.notified().await;
                self.runtime
                    .read(|tree| {
                        let receipt = &tree.operations()["retirement-cut"];
                        assert!(receipt.terminal_response.is_none());
                        assert!(receipt.terminal_code.is_none());
                    })
                    .unwrap();
                cut_ready(&id).await;
            }
            let request = crate::agent_tree_protocol::AttemptScopedRequest {
                agent_id: id.clone(),
                expected_attempt_id: Some(identity.attempt.clone()),
            };
            let closed = self
                .runtime
                .mutate_then_emit(false, |tree| {
                    tree.prepare_operation("retirement-cut", "root", "close_agent", &request)
                        .map_err(|error| error.message)?;
                    tree.close_agent("root", &request, None)
                        .map_err(|error| error.message)
                })
                .unwrap();
            if mode == "retire-intent" {
                cut_ready(&id).await;
            }
            let join = self
                .runtime
                .begin_cancel_live_many(std::slice::from_ref(&id))
                .await;
            if mode == "retire-unpublished" {
                released.notified().await;
                assert!(self.runtime.live.lock().await.contains_key(&id));
                cut_ready(&id).await;
            }
            assert!(join.await.is_clean());
            self.runtime
                .mutate_then_emit(false, |tree| {
                    tree.remember_success(
                        "retirement-cut",
                        "root",
                        "close_agent",
                        &request,
                        &closed,
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
            cut_ready(&id).await;
        }
        if mode == "wake" {
            if let Some(trace) = std::env::var_os("PLAK_TEST_COMMIT_TRACE") {
                let ordinal = trace_rows(Path::new(&trace))
                    .into_iter()
                    .filter(|row| row["store"] == "runtime" && row["indexed"] == true)
                    .filter_map(|row| row["store_ordinal"].as_u64())
                    .max()
                    .unwrap();
                std::fs::write(self.root.join("before-wake-ordinal"), ordinal.to_string()).unwrap();
                std::fs::write(self.root.join("child-id"), &id).unwrap();
            }
            assert!(matches!(
                crate::agent::park::turn_end(
                    self.runtime.driver(),
                    &self.runtime.live,
                    &id,
                    &identity,
                    &crate::agent::park::Arrivals::new()
                )
                .await,
                crate::agent::park::TurnEnd::Park
            ));
            assert!(matches!(
                crate::agent::park::park_until_wake(
                    self.runtime.driver(),
                    &self.runtime.live,
                    &id,
                    &identity,
                    &crate::agent::park::Arrivals::new()
                )
                .await,
                crate::agent::park::ParkEnd::Woken(..)
            ));
            gate.entered().await;
            cut_ready(&id).await;
        }
        cut_ready(&id).await;
    }

    async fn recover(&self, id: &str, abandoned: bool) {
        crate::runtime::resume_tree(&self.tree_id).unwrap();
        super::super::children::reattach_inflight_children(&self.parent, &self.runtime).await;
        let _ = super::run_tree_recovery_pass(&self.tree_id).await;
        self.until_settled(id).await;
        let commands = self.commands();
        let attempts = self.runtime.read(|tree| tree.attempts().len()).unwrap();
        let recoverer = super::ProductChildRecoverer {
            parent: self.parent.clone(),
            runtime: self.runtime.clone(),
        };
        self.runtime
            .read(|tree| {
                assert!(tree.attempt(id).unwrap().status.is_stopped());
                if abandoned {
                    assert_eq!(tree.node(id).unwrap().status, NodeStatus::Failed);
                    assert_eq!(
                        tree.attempt(id).unwrap().failure.as_ref().unwrap().kind,
                        "turn_abandoned"
                    );
                }
                assert_eq!(tree.pending_mail("root").len(), 1);
            })
            .unwrap();
        let before = self.abandonments(id).await;
        assert_eq!(before, usize::from(abandoned));
        assert!(!recoverer.recover(id.into(), None).await.unwrap().recovered);
        let _ = super::run_tree_recovery_pass(&self.tree_id).await;
        assert_eq!(self.abandonments(id).await, before);
        assert_eq!(self.commands(), commands);
        self.runtime
            .read(|tree| {
                assert_eq!(tree.pending_mail("root").len(), 1);
                assert_eq!(tree.attempts().len(), attempts);
            })
            .unwrap();
        crate::topology_runtime::force_release(&self.tree_id).await;
    }

    fn commands(&self) -> Vec<crate::agent_tree::AgentCommand> {
        self.runtime
            .read(|tree| tree.commands().values().cloned().collect())
            .unwrap()
    }

    async fn until_settled(&self, id: &str) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let changed = self.runtime.read(ProductAgentTree::waiters).unwrap();
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self
                    .runtime
                    .read(|tree| tree.attempt(id).is_some_and(|a| a.status.is_stopped()))
                    .unwrap()
                {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}: {error}: {:?}",
                self.tree_id,
                self.runtime
                    .read(|tree| (
                        tree.node(id).cloned(),
                        tree.attempt(id).cloned(),
                        tree.waiting_on(id).cloned(),
                        tree.commands().clone()
                    ))
                    .unwrap()
            )
        });
    }

    async fn abandonments(&self, id: &str) -> usize {
        use plakous::sessions::feed::{
            FeedCancelCause, FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedSeq,
            FeedTurnOutcome,
        };
        let locator: plakous::sessions::SessionLocator = self
            .runtime
            .read(|tree| {
                tree.node(id)
                    .unwrap()
                    .session_locator
                    .as_ref()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .unwrap();
        let sessions = self.parent.state_roots().runtime_sessions().unwrap();
        let mut cursor = FeedSeq::ZERO;
        let mut found = 0;
        loop {
            let FeedPage::Records { records, .. } = sessions
                .feed_page(FeedPageRequest::after(FeedCursor::new(
                    locator.session_id(),
                    cursor,
                )))
                .await
                .unwrap()
            else {
                break;
            };
            if records.is_empty() {
                break;
            }
            for record in records {
                cursor = record.header().seq;
                found += record
                    .facts()
                    .iter()
                    .filter(|fact| {
                        matches!(
                            fact,
                            FeedFact::TurnTerminal {
                                outcome: FeedTurnOutcome::Cancelled {
                                    cause: FeedCancelCause::Abandoned { .. }
                                },
                                ..
                            }
                        )
                    })
                    .count();
            }
        }
        found
    }
}

async fn cut_ready(id: &str) -> ! {
    println!("{READY} {id}");
    std::io::stdout().flush().unwrap();
    std::future::pending::<()>().await;
    unreachable!()
}

fn kill_owner(root: &Path, mode: &str) -> String {
    let mut child = spawn_owner(root, mode);
    let id = await_owner(&mut child, mode);
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(9));
    id
}

fn spawn_owner(root: &Path, mode: &str) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "agent::recovery::crash_tests::child_process",
            "--ignored",
            "--nocapture",
        ])
        .env(ROOT_ENV, root)
        .env(MODE_ENV, mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if root.join("trace-enabled").exists() {
        command.env("PLAK_TEST_COMMIT_TRACE", root.join("commits.jsonl"));
    }
    if let Ok(cut) = std::fs::read_to_string(root.join("kill-ordinal")) {
        command.env("PLAK_TEST_COMMIT_KILL", format!("runtime:{}", cut.trim()));
    }
    command.spawn().unwrap()
}

fn trace_rows(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn child_wake_admitted_before_dispatch_rejoins_without_duplicate_admission() {
    let pilot = tempfile::tempdir().unwrap();
    std::fs::write(pilot.path().join("trace-enabled"), "").unwrap();
    kill_owner(pilot.path(), "wake");
    let previous: u64 = std::fs::read_to_string(pilot.path().join("before-wake-ordinal"))
        .unwrap()
        .parse()
        .unwrap();
    let cut = trace_rows(&pilot.path().join("commits.jsonl"))
        .into_iter()
        .find(|row| {
            row["store"] == "runtime"
                && row["indexed"] == true
                && row["store_ordinal"].as_u64().unwrap() > previous
        })
        .unwrap()["store_ordinal"]
        .as_u64()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("trace-enabled"), "").unwrap();
    std::fs::write(root.path().join("kill-ordinal"), cut.to_string()).unwrap();
    let output = spawn_owner(root.path(), "wake").wait_with_output().unwrap();
    assert_eq!(
        output.status.signal(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = std::fs::read_to_string(root.path().join("child-id")).unwrap();
    let count = || {
        plakous_offline_crash_harness::evidence::read_evidence(root.path().join("dispatch.jsonl"))
            .unwrap()
            .into_iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
    };
    assert_eq!(
        count(),
        1,
        "wake dispatch must not have happened at the cut"
    );
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new(root.path(), "wake", None, true);
        let locator = fixture
            .runtime
            .read(|tree| {
                tree.node(&id)
                    .unwrap()
                    .session_locator
                    .as_ref()
                    .unwrap()
                    .parse::<plakous::sessions::SessionLocator>()
                    .unwrap()
            })
            .unwrap();
        let sessions = fixture.parent.state_roots().runtime_sessions().unwrap();
        let feed = crate::agent::read_tree_feed(&sessions, locator.session_id(), 0)
            .await
            .unwrap();
        assert_eq!(
            feed.records
                .iter()
                .flat_map(plakous::sessions::feed::FeedRecord::facts)
                .filter(|fact| matches!(
                    fact,
                    plakous::sessions::feed::FeedFact::CommandAccepted { .. }
                ))
                .count(),
            2,
            "the cut must follow durable wake admission"
        );
        fixture.recover(&id, true).await;
    });
    assert_eq!(
        count(),
        1,
        "an admitted but unstarted wake is honestly abandoned, not replayed"
    );
}

fn await_owner(child: &mut std::process::Child, mode: &str) -> String {
    let stdout = child.stdout.take().unwrap();
    let (sent, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if let Some((_, id)) = line.unwrap().split_once(&format!("{READY} ")) {
                let _ = sent.send(id.trim().to_owned());
            }
        }
    });
    ready
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| {
            let _ = child.kill();
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("{mode}: {error}: {stderr}")
        })
}

#[test]
fn child_rejoin_preserves_certified_decision_and_settles_waits() {
    let root = tempfile::tempdir().unwrap();
    let id = kill_owner(root.path(), "certified");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new(root.path(), "certified", None, true);
        crate::runtime::resume_tree(&fixture.tree_id).unwrap();
        fixture
            .runtime
            .mutate_then_emit(false, |tree| {
                let mail = tree
                    .send_agent_message(
                        "root",
                        &SendAgentMessageRequest {
                            refresh: None,
                            agent_id: id.clone(),
                            message: "retained receipt".into(),
                            purpose: None,
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .map_err(|error| error.message)?;
                let envelope = tree.pending_mail(&id).pop().unwrap();
                tree.remember_success(
                    "child-retained-wait",
                    &id,
                    "wait_agent",
                    &json!({}),
                    &json!({"output": {}, "acknowledged": [envelope], "newly_delivered": []}),
                )
                .map_err(|error| error.message)?;
                tree.acknowledge_mail(&mail.message_id)
                    .map_err(|error| error.message)
            })
            .unwrap();
        crate::runtime::resume_tree(&fixture.tree_id).unwrap();
        let locator = std::fs::read_to_string(root.path().join("created-locator")).unwrap();
        let path = root.path().to_string_lossy().into_owned();
        let super::super::children::ChildReopen::Attached { session, recovered } =
            super::super::children::reopen_spawned_child(
                &fixture.parent,
                &id,
                &path,
                &locator,
                "certified-rejoin",
                super::super::children::ChildReopenMode {
                    lease_wait: super::CHILD_LEASE_WAIT,
                    recovery: false,
                    rejoin: true,
                },
            )
            .await
            .unwrap()
        else {
            panic!("certified child did not reopen")
        };
        let restored = recovered.expect("RestoredWorking must preserve the child turn");
        let turn = restored.turn_id();
        let control = restored.control();
        let decision = restored.pending_decision().unwrap().clone();
        let _ = super::super::children::rejoin_recovered_start(
            &fixture.runtime,
            &id,
            "not-own-mail",
            *session,
            turn,
            control.clone(),
            restored.result(),
        )
        .await;
        assert!(fixture.runtime.live.lock().await.contains_key(&id));
        assert_eq!(
            fixture
                .runtime
                .read(|tree| tree.retained_wait_receipts(&id).len())
                .unwrap(),
            1
        );
        control
            .allow_effect(
                decision,
                plakous::tools::EffectDecisionAuditId::new("child:restored-approval").unwrap(),
            )
            .await
            .unwrap();
        fixture.until_settled(&id).await;
        let locator = fixture
            .runtime
            .read(|tree| {
                tree.node(&id)
                    .unwrap()
                    .session_locator
                    .as_ref()
                    .unwrap()
                    .parse::<plakous::sessions::SessionLocator>()
                    .unwrap()
            })
            .unwrap();
        let sessions = fixture.parent.state_roots().runtime_sessions().unwrap();
        let feed = crate::agent::read_tree_feed(&sessions, locator.session_id(), 0)
            .await
            .unwrap();
        assert_eq!(
            feed.records
                .iter()
                .flat_map(plakous::sessions::feed::FeedRecord::facts)
                .filter(|fact| matches!(fact,
            plakous::sessions::feed::FeedFact::TurnTerminal { turn_id, .. } if *turn_id == turn))
                .count(),
            1
        );
        assert_eq!(
            fixture
                .runtime
                .read(|tree| tree.attempt(&id).unwrap().usage.turns)
                .unwrap(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("child.txt")).unwrap(),
            "one effect"
        );
        assert!(
            fixture
                .runtime
                .read(|tree| tree.retained_wait_receipts(&id).is_empty())
                .unwrap()
        );
        assert_eq!(
            fixture
                .runtime
                .read(|tree| tree.pending_mail(&id).len())
                .unwrap(),
            1
        );
        fixture.recover(&id, false).await;
    });
    let evidence =
        plakous_offline_crash_harness::evidence::read_evidence(root.path().join("dispatch.jsonl"))
            .unwrap();
    assert_eq!(
        evidence
            .iter()
            .filter(|row| row.kind
                == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched)
            .count(),
        2
    );
}

#[test]
fn child_retirement_sigkill_cuts_recover_closed_once_without_redispatch() {
    for mode in [
        "retire-intent",
        "retire-unpublished",
        "retire-returned",
        "retire-tool",
    ] {
        let root = tempfile::tempdir().unwrap();
        let id = kill_owner(root.path(), mode);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let fixture = Fixture::new(root.path(), mode, None, true);
            let recoverer = super::ProductChildRecoverer {
                parent: fixture.parent.clone(), runtime: fixture.runtime.clone(),
            };
            recoverer.recover(id.clone(), None).await.unwrap();
            let before = fixture.runtime.read(|tree| {
                assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Closed);
                assert_eq!(tree.attempt(&id).unwrap().status, crate::agent_tree_protocol::AttemptStatus::Closed);
                assert_eq!(tree.pending_mail("root"), [] as [crate::agent_tree::MailRecord; 0]);
                let receipt = &tree.operations()["retirement-cut"];
                assert_eq!(receipt.terminal_response.is_some(), mode == "retire-returned");
                (tree.attempts().clone(), tree.messages().clone())
            }).unwrap();
            assert!(!recoverer.recover(id.clone(), None).await.unwrap().recovered);
            assert_eq!(fixture.runtime.read(|tree| (tree.attempts().clone(), tree.messages().clone())).unwrap(), before);
            let evidence = plakous_offline_crash_harness::evidence::read_evidence(root.path().join("dispatch.jsonl")).unwrap();
            assert_eq!(evidence.iter().filter(|row| row.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched).count(), 1);
            crate::topology_runtime::force_release(&fixture.tree_id).await;
        });
    }
}

#[test]
fn recover_agent_waits_for_a_still_held_child_lease() {
    let root = tempfile::tempdir().unwrap();
    let mut owner = spawn_owner(root.path(), "mid");
    let id = await_owner(&mut owner, "held lease");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let fixture = Fixture::new(root.path(), "mid", None, true);
        fixture
            .runtime
            .record_then_apply(&id, |tree| {
                tree.record_host_retirement(
                    &id,
                    "retirement could not release the live lease".into(),
                )
            })
            .unwrap();
        let recoverer = super::ProductChildRecoverer {
            parent: fixture.parent.clone(),
            runtime: fixture.runtime.clone(),
        };
        let began = tokio::time::Instant::now();
        let recovering = recoverer.recover(id.clone(), None);
        tokio::pin!(recovering);
        // Do not release on a timer: under load the first open might not
        // have reached the live lease yet. The second binding proves the
        // first attempt actually refused and the outer retry ran.
        let early = tokio::select! {
            outcome = &mut recovering => Some(outcome),
            () = fixture.retry_started.notified() => None,
        };
        owner.kill().unwrap();
        owner.wait().unwrap();
        let outcome = match early {
            Some(outcome) => outcome,
            None => recovering.await,
        };
        assert!(fixture.reopen_attempts.load(Ordering::SeqCst) >= 2);
        assert!(
            outcome.unwrap().recovered,
            "recover_agent must poll rather than immediately refusing a held lease"
        );
        assert!(began.elapsed() >= super::CHILD_LEASE_POLL);
        assert_eq!(fixture.abandonments(&id).await, 1);
        let commands = fixture.commands();
        let attempts = fixture.runtime.read(|tree| tree.attempts().len()).unwrap();
        assert!(!recoverer.recover(id.clone(), None).await.unwrap().recovered);
        assert_eq!(fixture.commands(), commands);
        fixture
            .runtime
            .read(|tree| {
                assert_eq!(tree.pending_mail("root").len(), 1);
                assert_eq!(tree.attempts().len(), attempts);
            })
            .unwrap();
        crate::topology_runtime::force_release(&fixture.tree_id).await;
    });
}

#[test]
#[ignore = "subprocess helper for child SIGKILL matrix"]
fn child_process() {
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("fixture root"));
    let mode = std::env::var(MODE_ENV).expect("fixture mode");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let gate = GateHandle::new();
        let fixture = Fixture::new(&root, &mode, Some(&gate), false);
        fixture.cut(&mode, &gate).await;
    });
}

#[test]
fn child_sigkill_wake_and_mid_turn_recover_once_without_redispatch() {
    for mode in ["wake", "mid", "idle", "wake-prepared"] {
        let root = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::recovery::crash_tests::child_process",
                "--ignored",
                "--nocapture",
            ])
            .env(ROOT_ENV, root.path())
            .env(MODE_ENV, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let ready_prefix = format!("{READY} ");
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if let Some((_, id)) = line.split_once(ready_prefix.as_str()) {
                    let _ = sent.send(id.trim().to_owned());
                }
            }
        });
        let ready = received.recv_timeout(Duration::from_secs(45));
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        reader.join().unwrap();
        let id = ready.unwrap_or_else(|error| {
            panic!(
                "{mode}: {error}; {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(output.status.signal(), Some(9));
        let count = || {
            plakous_offline_crash_harness::evidence::read_evidence(
                root.path().join("dispatch.jsonl"),
            )
            .unwrap()
            .into_iter()
            .filter(|row| {
                row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
            })
            .count()
        };
        let before = count();
        assert_eq!(
            before,
            match mode {
                "wake" => 2,
                _ => 1,
            }
        );
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let fixture = Fixture::new(root.path(), mode, None, true);
            fixture.recover(&id, matches!(mode, "wake" | "mid")).await;
        });
        assert_eq!(
            count(),
            before + usize::from(matches!(mode, "idle" | "wake-prepared")),
            "only never-admitted work can dispatch once"
        );
    }
}
