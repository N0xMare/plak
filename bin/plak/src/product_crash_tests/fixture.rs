use std::error::Error;
use std::future;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use crate::test_support::{
    ActualHostAction, ActualHostBoundary, ActualHostInjection, ActualHostWorkflow,
    InjectedRuntimeRole, ProductJournal, run_actual_host,
};
use plakous::model::ContextStatusKind;
use plakous::sessions::{
    AgentSessions, CompactReason, CompactSessionOutcome, CompactSessionRequest, ContextQuery,
    ContextStatusRequest, CreateSessionOutcome, CreateSessionRequest, ForkSessionOutcome,
    ForkSessionRequest, OpenSessionOutcome, OpenSessionRequest, RebindSessionOutcome,
    RebindSessionRequest, SessionBackend, SessionLocator, SessionOperationId,
};
use serde::{Deserialize, Serialize};

use crate::product_paths::PlakPaths;
use crate::product_sessions::SessionWorkflowKind;
use plakous_hosted::session_store::SessionRecordStore;
use plakous_hosted::sessions::projector::WorkflowRequest;

use super::protocol::{
    ChildReply, CrashBoundary, PROTOCOL_VERSION, ProductCrashBoundary, ProductProbeReport,
    ProductWorkflow, write_fsynced_json, write_reply,
};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::{
    BoundaryController, compaction_runtime_binding,
    compaction_runtime_binding_with_projected_input, runtime_binding, runtime_binding_for_account,
};

const ACTUAL_HOST_COMPACT_INPUT_CEILING: u64 = 5_700;

type ProductResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Debug, Deserialize, Serialize)]
struct ProductState {
    version: u16,
    workflow: ProductWorkflow,
    source_product_session_id: String,
    target_product_session_id: Option<String>,
    runtime_target: Option<String>,
}

fn host_workflow(workflow: ProductWorkflow) -> ActualHostWorkflow {
    match workflow {
        ProductWorkflow::RootDelivery => ActualHostWorkflow::RootDelivery,
        ProductWorkflow::SendNow => ActualHostWorkflow::SendNow,
        ProductWorkflow::Fork => ActualHostWorkflow::Fork,
        ProductWorkflow::Rebind => ActualHostWorkflow::Rebind,
        ProductWorkflow::Compact => ActualHostWorkflow::Compact,
    }
}

fn host_boundary(boundary: ProductCrashBoundary) -> ActualHostBoundary {
    match boundary {
        ProductCrashBoundary::Prepared => ActualHostBoundary::Prepared,
        ProductCrashBoundary::RuntimeReturned => ActualHostBoundary::RuntimeReturned,
        ProductCrashBoundary::ProductPublished => ActualHostBoundary::ProductPublished,
        ProductCrashBoundary::ProductCommitted => ActualHostBoundary::ProductCommitted,
        ProductCrashBoundary::Finalization
        | ProductCrashBoundary::CompletedToolRound
        | ProductCrashBoundary::FinalizationPendingTerminal => ActualHostBoundary::TerminalReady,
    }
}

/// Present only for the finalization cuts: the live Grok shapes they reproduce
/// seal every round, while the live completed-tool-round shape (Claude) sealed
/// none.
const FINALIZATION_SEAL_MARKER: &str = "finalization-seal";

fn root_decision_cut_sql(boundary: ProductCrashBoundary) -> String {
    let (trigger, decision, state) = match boundary {
        ProductCrashBoundary::Finalization | ProductCrashBoundary::FinalizationPendingTerminal => {
            ("stop_terminal", "Terminal", "03")
        }
        ProductCrashBoundary::CompletedToolRound => ("stop_next_model", "ToolBatch", "06"),
        _ => unreachable!("only recorded-decision cuts install a runtime trigger"),
    };
    // The receipt's predecessor is the recorded-decision head. The candidate
    // may already be the branch head inside this transaction; never read it
    // as evidence that the decision's consequence was committed.
    // The outcome payload excludes its 9-byte component header and 38-byte digest.
    format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON commit_receipts
         WHEN EXISTS (
            WITH RECURSIVE head(kind,codec,digest) AS (
                SELECT root_kind,root_codec,root_digest FROM checkpoints
                WHERE checkpoint_id=NEW.expected_checkpoint
                UNION SELECT child_kind,child_codec,child_digest
                FROM runtime_object_edges JOIN head
                ON parent_kind=head.kind AND parent_codec=head.codec AND parent_digest=head.digest
            )
            SELECT 1 FROM head JOIN runtime_objects AS operation USING(kind,codec,digest)
            JOIN runtime_object_bytes AS operation_payload ON operation_payload.id=operation.id
            JOIN runtime_objects AS evidence
            ON evidence.kind=8 AND evidence.digest=substr(operation_payload.bytes,length(operation_payload.bytes)-31,32)
            JOIN runtime_object_bytes AS evidence_payload ON evidence_payload.id=evidence.id
            WHERE head.kind=14 AND substr(operation_payload.bytes,9,1)=X'02'
            AND substr(operation_payload.bytes,length(operation_payload.bytes)-38,1)=X'{state}'
            AND CASE WHEN json_valid(substr(evidence_payload.bytes,10,length(evidence_payload.bytes)-47)) THEN
                json_extract(CAST(substr(evidence_payload.bytes,10,length(evidence_payload.bytes)-47) AS TEXT),'$[0]')='provider-outcome-v2'
                AND json_extract(CAST(substr(evidence_payload.bytes,10,length(evidence_payload.bytes)-47) AS TEXT),'$[1].decision')='{decision}'
                ELSE 0 END
         )
         BEGIN SELECT RAISE(FAIL, 'injected recorded-decision consequence cut'); END;"
    )
}

fn actual_runtime_binding(
    workspace: &Path,
    controller: BoundaryController,
) -> ProductResult<plakous::sessions::SessionRuntimeBinding> {
    let script = workspace.join("finalization-script.json");
    if !script.exists() {
        return runtime_binding(workspace, controller);
    }
    let script: Arc<plakous_offline_crash_harness::script::ScriptPlan> =
        Arc::new(serde_json::from_slice(&std::fs::read(script)?)?);
    if workspace.join(FINALIZATION_SEAL_MARKER).exists() {
        Ok(
            plakous_offline_crash_harness::scripted_artifact_runtime_binding(
                workspace, controller, &script,
            )?,
        )
    } else {
        Ok(plakous_offline_crash_harness::scripted_runtime_binding(
            workspace, controller, script,
        )?)
    }
}

fn actual_host_injection(
    workspace: &Path,
    controller: &BoundaryController,
    selected: Option<(ActualHostWorkflow, ActualHostBoundary)>,
    compact_primary: bool,
    finalization: Option<ProductCrashBoundary>,
    completed_tool_round: bool,
    database: &Path,
) -> ProductResult<ActualHostInjection> {
    let workspace = workspace.to_path_buf();
    let target =
        runtime_binding_for_account(&workspace, controller.clone(), b"offline-loopback-rebind")?
            .target_binding();
    let runtime_controller = controller.clone();
    let runtime_workspace = workspace.clone();
    let runtime = Arc::new(move |role| {
        let result = match role {
            InjectedRuntimeRole::Primary if compact_primary => {
                compaction_runtime_binding_with_projected_input(
                    &runtime_workspace,
                    runtime_controller.clone(),
                    ACTUAL_HOST_COMPACT_INPUT_CEILING,
                )
            }
            InjectedRuntimeRole::Primary => {
                actual_runtime_binding(&runtime_workspace, runtime_controller.clone())
            }
            InjectedRuntimeRole::Compact if compact_primary => {
                compaction_runtime_binding_with_projected_input(
                    &runtime_workspace,
                    runtime_controller.clone(),
                    ACTUAL_HOST_COMPACT_INPUT_CEILING,
                )
            }
            InjectedRuntimeRole::Compact => {
                compaction_runtime_binding(&runtime_workspace, runtime_controller.clone())
            }
            InjectedRuntimeRole::Rebind => runtime_binding_for_account(
                &runtime_workspace,
                runtime_controller.clone(),
                b"offline-loopback-rebind",
            ),
        };
        result.map_err(|error| error.to_string())
    });
    let database = database.to_path_buf();
    let observe = Arc::new(move |workflow, boundary| {
        if completed_tool_round
            && workflow == ActualHostWorkflow::RootDelivery
            && boundary == ActualHostBoundary::Prepared
        {
            rusqlite::Connection::open(&database)
                .unwrap()
                .execute_batch(&root_decision_cut_sql(
                    ProductCrashBoundary::CompletedToolRound,
                ))
                .unwrap();
        }
        if let Some(finalization) = finalization
            && workflow == ActualHostWorkflow::RootDelivery
            && boundary == ActualHostBoundary::Prepared
        {
            rusqlite::Connection::open(&database)
                .unwrap()
                .execute_batch(&root_decision_cut_sql(finalization))
                .unwrap();
        }

        if selected == Some((workflow, boundary)) {
            let _ = write_reply(&ChildReply::ProductBoundary {
                version: PROTOCOL_VERSION,
                workflow: match workflow {
                    ActualHostWorkflow::RootDelivery => ProductWorkflow::RootDelivery,
                    ActualHostWorkflow::SendNow => ProductWorkflow::SendNow,
                    ActualHostWorkflow::Fork => ProductWorkflow::Fork,
                    ActualHostWorkflow::Rebind => ProductWorkflow::Rebind,
                    ActualHostWorkflow::Compact => ProductWorkflow::Compact,
                },
                boundary: match boundary {
                    ActualHostBoundary::Prepared => ProductCrashBoundary::Prepared,
                    ActualHostBoundary::RuntimeReturned => ProductCrashBoundary::RuntimeReturned,
                    ActualHostBoundary::ProductPublished => ProductCrashBoundary::ProductPublished,
                    ActualHostBoundary::ProductCommitted => ProductCrashBoundary::ProductCommitted,
                    ActualHostBoundary::TerminalReady if finalization.is_some() => {
                        finalization.unwrap()
                    }
                    ActualHostBoundary::TerminalReady if completed_tool_round => {
                        ProductCrashBoundary::CompletedToolRound
                    }
                    ActualHostBoundary::TerminalReady => {
                        unreachable!(
                            "only finalization or completed-tool-round cuts observe TerminalReady"
                        )
                    }
                },
            });
            loop {
                std::thread::park();
            }
        }
    });
    Ok(ActualHostInjection::new(runtime, target, observe))
}

pub(crate) async fn run_actual_host_fixture(
    home: &Path,
    workspace: &Path,
    state_path: &Path,
    workflow: ProductWorkflow,
    cut: ProductCrashBoundary,
) -> ProductResult<ProductProbeReport> {
    std::fs::create_dir_all(home)?;
    std::fs::create_dir_all(workspace)?;
    let paths = PlakPaths::explicit(home.to_path_buf());
    paths.ensure_layout().map_err(std::io::Error::other)?;
    let controller = BoundaryController::from_evidence_path(
        CrashBoundary::None,
        home.join("product-physical.jsonl"),
    )?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    if matches!(
        cut,
        ProductCrashBoundary::Finalization
            | ProductCrashBoundary::CompletedToolRound
            | ProductCrashBoundary::FinalizationPendingTerminal
    ) {
        let script = serde_json::json!({"version": 1, "process_tools": true, "steps": [{"output": {"tool": {"name": "process_session_start", "arguments": {"command": "echo $$ > finalization-job.pid; read line", "workdir": ".", "login": false, "tty": false, "keep_stdin_open": true, "yield_time_ms": 100, "max_output_bytes": 8192}}}, "finish": "tool_calls"}]});
        std::fs::write(
            workspace.join("finalization-script.json"),
            serde_json::to_vec(&script)?,
        )?;
        if cut != ProductCrashBoundary::CompletedToolRound {
            std::fs::write(workspace.join(FINALIZATION_SEAL_MARKER), b"")?;
        }
    }
    let initial_runtime = if workflow == ProductWorkflow::Compact {
        compaction_runtime_binding_with_projected_input(
            workspace,
            controller.clone(),
            ACTUAL_HOST_COMPACT_INPUT_CEILING,
        )?
    } else {
        actual_runtime_binding(workspace, controller.clone())?
    };
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(format!("actual-host:create:{workflow:?}"))?,
            initial_runtime,
        ))
        .await?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("actual host fixture create was not new".into());
    };
    if workflow == ProductWorkflow::Compact {
        for prompt in [
            "older compactable intent".to_owned(),
            format!("latest protected intent {}", "x".repeat(2_000)),
        ] {
            let _ = session.agent().prompt(prompt).await?.result().await?;
        }
        let status = sessions
            .context_status(ContextStatusRequest::new(
                SessionOperationId::new("actual-host:compact-precondition")?,
                session.locator(),
                ContextQuery::Current,
            ))
            .await?;
        let plakous::sessions::SessionContextStatusKind::Available(status) = status.kind() else {
            return Err(format!("compact fixture context was unavailable: {status:?}").into());
        };
        if !matches!(status.kind(), ContextStatusKind::WouldOverflow(_)) {
            return Err(format!(
                "compact fixture did not establish an eligible overflow: kind={:?}, measurement={:?}",
                status.kind(),
                status.measurement().map(plakous::model::ContextMeasurement::bound)
            )
            .into());
        }
    }
    let locator = session.release().await?;
    let mut journal = ProductJournal::create(home.to_path_buf(), workspace.to_path_buf())?;
    journal.bind_locator(locator)?;
    journal.set_product_binding("codex", "offline-loopback-v1", "medium")?;
    let product_id = journal.product_session_id().to_owned();
    let actions = match workflow {
        ProductWorkflow::RootDelivery => vec![ActualHostAction::Prompt(
            "perform the loopback effect".into(),
        )],
        ProductWorkflow::SendNow => {
            let (id, revision) = journal.enqueue("steer at the next safe boundary")?;
            vec![
                ActualHostAction::Prompt("hold active for steering".into()),
                ActualHostAction::SendQueuedNow { id, revision },
            ]
        }
        ProductWorkflow::Fork => vec![ActualHostAction::Fork],
        // The binding changes only through a profile fork, so the matrix
        // seeds rebind rows through the product fixture and reconciles them
        // with the actual host.
        ProductWorkflow::Rebind => {
            return Err("actual-host rebind rows are seeded by the product fixture".into());
        }
        ProductWorkflow::Compact => vec![ActualHostAction::Compact],
    };
    write_fsynced_json(
        state_path,
        &ProductState {
            version: PROTOCOL_VERSION,
            workflow,
            source_product_session_id: product_id.clone(),
            target_product_session_id: None,
            runtime_target: None,
        },
    )?;
    let host_home = home.to_path_buf();
    let injection = actual_host_injection(
        workspace,
        &controller,
        Some((host_workflow(workflow), host_boundary(cut))),
        workflow == ProductWorkflow::Compact,
        matches!(
            cut,
            ProductCrashBoundary::Finalization | ProductCrashBoundary::FinalizationPendingTerminal
        )
        .then_some(cut),
        cut == ProductCrashBoundary::CompletedToolRound,
        &paths.runtime_database(),
    )?;
    std::thread::spawn(move || run_actual_host(host_home, &product_id, injection, actions))
        .join()
        .map_err(|_| "actual host thread panicked")??;
    probe_product_fixture(home, state_path)
}

pub(crate) fn recover_actual_host_fixture(
    home: &Path,
    workspace: &Path,
    state_path: &Path,
    next_prompt: bool,
) -> ProductResult<ProductProbeReport> {
    let state: ProductState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(state_path)?))?;
    let controller = BoundaryController::from_evidence_path(
        if next_prompt && !workspace.join("finalization-job.pid").exists() {
            CrashBoundary::ProviderDispatchOne
        } else {
            CrashBoundary::None
        },
        home.join("product-physical.jsonl"),
    )?;
    let selected = None;
    let host_home = home.to_path_buf();
    let product_id = state.source_product_session_id;
    let injection = actual_host_injection(
        workspace,
        &controller,
        selected,
        state.workflow == ProductWorkflow::Compact,
        None,
        false,
        &PlakPaths::explicit(home.to_path_buf()).runtime_database(),
    )?;
    let actions = if next_prompt && workspace.join("finalization-job.pid").exists() {
        vec![ActualHostAction::PromptOnce(
            "follow-up after finalization recovery".into(),
        )]
    } else if next_prompt {
        vec![ActualHostAction::Prompt(
            "next prompt on adopted target".into(),
        )]
    } else {
        vec![ActualHostAction::Quit]
    };
    std::thread::spawn(move || run_actual_host(host_home, &product_id, injection, actions))
        .join()
        .map_err(|_| "actual host recovery thread panicked")??;
    probe_product_fixture(home, state_path)
}

struct ProductBoundaryController {
    workflow: ProductWorkflow,
    cut: ProductCrashBoundary,
}

impl ProductBoundaryController {
    async fn reach(&self, boundary: ProductCrashBoundary) -> ProductResult {
        write_reply(&ChildReply::ProductBoundary {
            version: PROTOCOL_VERSION,
            workflow: self.workflow,
            boundary,
        })?;
        if self.cut == boundary {
            future::pending::<()>().await;
        }
        Ok(())
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one fixture dispatch keeps the three workflow setup paths comparable"
)]
pub(crate) async fn run_product_fixture(
    home: &Path,
    workspace: &Path,
    state_path: &Path,
    workflow: ProductWorkflow,
    cut: ProductCrashBoundary,
) -> ProductResult<ProductProbeReport> {
    std::fs::create_dir_all(workspace)?;
    std::fs::create_dir_all(home)?;
    let paths = PlakPaths::explicit(home.to_path_buf());
    paths.ensure_layout().map_err(std::io::Error::other)?;
    let model = BoundaryController::from_evidence_path(
        CrashBoundary::None,
        home.join("product-physical.jsonl"),
    )?;
    let backend = SessionBackend::sqlite(paths.runtime_database())?;
    let sessions = AgentSessions::new(backend)?;
    let initial_runtime = if workflow == ProductWorkflow::Compact {
        compaction_runtime_binding(workspace, model.clone())?
    } else {
        runtime_binding(workspace, model.clone())?
    };
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(format!("product-harness:create:{workflow:?}"))?,
            initial_runtime,
        ))
        .await?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("product fixture create was not new".into());
    };
    if workflow == ProductWorkflow::Compact {
        for prompt in ["older compactable intent", "latest protected intent"] {
            let _ = session.agent().prompt(prompt).await?.result().await?;
        }
    }
    let source = session.release().await?;
    let mut journal = ProductJournal::create(home.to_path_buf(), workspace.to_path_buf())?;
    journal.bind_locator(source)?;
    let source_product_session_id = journal.product_session_id().to_owned();
    write_fsynced_json(
        state_path,
        &ProductState {
            version: PROTOCOL_VERSION,
            workflow,
            source_product_session_id,
            target_product_session_id: None,
            runtime_target: None,
        },
    )?;
    let cuts = ProductBoundaryController { workflow, cut };

    match workflow {
        ProductWorkflow::Fork => {
            Box::pin(run_fork(
                &sessions,
                &mut journal,
                source,
                home,
                workspace,
                model,
                state_path,
                &cuts,
            ))
            .await?;
        }
        ProductWorkflow::Rebind => {
            Box::pin(run_rebind(
                &sessions,
                &mut journal,
                source,
                home,
                workspace,
                model,
                state_path,
                &cuts,
            ))
            .await?;
        }
        ProductWorkflow::Compact => {
            Box::pin(run_compact(
                &sessions,
                &mut journal,
                source,
                workspace,
                home,
                state_path,
                &cuts,
            ))
            .await?;
        }
        _ => return Err("root and steer require the actual-host fixture".into()),
    }
    probe_product_fixture(home, state_path)
}

fn submit_branch_request(
    home: &Path,
    journal: &ProductJournal,
    state_path: &Path,
    request: &WorkflowRequest,
) -> ProductResult {
    let store = crate::store::session_records::SqliteSessionStore::new(PlakPaths::explicit(
        home.to_path_buf(),
    ));
    let mut record = store.load(journal.product_session_id())?;
    store.submit_workflow_request(&mut record, request)?;
    let mut state: ProductState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(state_path)?))?;
    state.target_product_session_id = Some(request.successor_product_id());
    write_fsynced_json(state_path, &state)?;
    Ok(())
}

async fn project_branch_request(
    sessions: &AgentSessions,
    home: &Path,
    product_id: &str,
) -> ProductResult {
    let store: Arc<dyn SessionRecordStore> =
        Arc::new(crate::store::session_records::SqliteSessionStore::new(
            PlakPaths::explicit(home.to_path_buf()),
        ));
    let mut record = store.load(product_id)?;
    plakous_hosted::runtime::project_session(sessions, &store, &record).await?;
    store.withdraw_unadmitted_workflows(&mut record)?;
    Ok(())
}

/// Plak's `/fork`: a recompiling fork of the source's own binding.
#[allow(
    clippy::too_many_arguments,
    reason = "process fixture carries exact source, target, runtime, state, and cut authorities"
)]
async fn run_fork(
    sessions: &AgentSessions,
    journal: &mut ProductJournal,
    source: SessionLocator,
    home: &Path,
    workspace: &Path,
    model: BoundaryController,
    state_path: &Path,
    cuts: &ProductBoundaryController,
) -> ProductResult {
    const OPERATION: &str = "plak:fork:crash-harness";
    submit_branch_request(
        home,
        journal,
        state_path,
        &WorkflowRequest {
            operation_id: OPERATION.into(),
            source_locator: source,
            kind: SessionWorkflowKind::Fork,
        },
    )?;
    cuts.reach(ProductCrashBoundary::Prepared).await?;
    let outcome = Box::pin(sessions.fork(ForkSessionRequest::recompiling(
        SessionOperationId::new(OPERATION)?,
        source,
        runtime_binding(workspace, model)?,
    )))
    .await?;
    let ForkSessionOutcome::Forked { target, .. } = outcome else {
        return Err(format!("fork did not publish: {outcome:?}").into());
    };
    record_runtime_target(state_path, target)?;
    cuts.reach(ProductCrashBoundary::RuntimeReturned).await?;
    project_branch_request(sessions, home, journal.product_session_id()).await?;
    cuts.reach(ProductCrashBoundary::ProductPublished).await?;
    cuts.reach(ProductCrashBoundary::ProductCommitted).await
}

#[allow(
    clippy::too_many_arguments,
    reason = "process fixture carries exact source, target, runtime, state, and cut authorities"
)]
async fn run_rebind(
    sessions: &AgentSessions,
    journal: &mut ProductJournal,
    source: SessionLocator,
    home: &Path,
    workspace: &Path,
    model: BoundaryController,
    state_path: &Path,
    cuts: &ProductBoundaryController,
) -> ProductResult {
    const OPERATION: &str = "plak:rebind:crash-harness";
    let runtime = runtime_binding_for_account(workspace, model, b"offline-loopback-rebind")?;
    let binding = runtime.target_binding();
    submit_branch_request(
        home,
        journal,
        state_path,
        &WorkflowRequest {
            operation_id: OPERATION.into(),
            source_locator: source,
            kind: SessionWorkflowKind::Rebind(Box::new(crate::product_sessions::RebindSelection {
                provider: "codex".into(),
                account: "offline-rebind".into(),
                model: "offline-loopback-v1".into(),
                effort: Some("medium".into()),
                composition: crate::product_config::ProductCompositionSelection::FollowCatalog,
                target_selection: crate::product_config::ProductTargetSelection::Native,
                protocol: binding.protocol().as_str().into(),
                route: binding.route().as_str().into(),
                requested_model: binding.model().as_str().into(),
                selected_configuration: serde_json::to_string(
                    &binding.selected_provider_configuration(),
                )?,
                effective_configuration: serde_json::to_string(
                    &binding.effective_provider_configuration(),
                )?,
                account_binding: serde_json::to_string(&binding.account_binding())?,
            })),
        },
    )?;
    cuts.reach(ProductCrashBoundary::Prepared).await?;
    let outcome = sessions
        .rebind(RebindSessionRequest::recompiling_target(
            SessionOperationId::new(OPERATION)?,
            source,
            binding,
            runtime,
        ))
        .await?;
    let RebindSessionOutcome::Rebound { target, .. } = outcome else {
        return Err(format!("rebind did not publish: {outcome:?}").into());
    };
    record_runtime_target(state_path, target)?;
    cuts.reach(ProductCrashBoundary::RuntimeReturned).await?;
    project_branch_request(sessions, home, journal.product_session_id()).await?;
    cuts.reach(ProductCrashBoundary::ProductPublished).await?;
    cuts.reach(ProductCrashBoundary::ProductCommitted).await
}

async fn run_compact(
    sessions: &AgentSessions,
    journal: &mut ProductJournal,
    source: SessionLocator,
    workspace: &Path,
    home: &Path,
    state_path: &Path,
    cuts: &ProductBoundaryController,
) -> ProductResult {
    const OPERATION: &str = "plak:compact:crash-harness";
    let store = crate::store::session_records::SqliteSessionStore::new(PlakPaths::explicit(
        home.to_path_buf(),
    ));
    let mut record = store.load(journal.product_session_id())?;
    store.submit_workflow_request(
        &mut record,
        &WorkflowRequest {
            operation_id: OPERATION.into(),
            source_locator: source,
            kind: SessionWorkflowKind::Compact(
                crate::store::sessions::ProductCompactReason::Manual,
            ),
        },
    )?;
    cuts.reach(ProductCrashBoundary::Prepared).await?;
    let evidence = BoundaryController::from_evidence_path(
        CrashBoundary::None,
        home.join("product-physical.jsonl"),
    )?;
    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("product-harness:compact-open")?,
            source,
            compaction_runtime_binding(workspace, evidence.clone())?,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("compact source did not attach".into());
    };
    let outcome = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new(OPERATION)?,
            source,
            CompactReason::Manual,
            compaction_runtime_binding(workspace, evidence)?,
        ))
        .await?;
    if let Some(target) = compact_target(&outcome) {
        record_runtime_target(state_path, target)?;
    }
    cuts.reach(ProductCrashBoundary::RuntimeReturned).await?;
    project_branch_request(sessions, home, journal.product_session_id()).await?;
    cuts.reach(ProductCrashBoundary::ProductPublished).await?;
    let _ = session.release().await;
    cuts.reach(ProductCrashBoundary::ProductCommitted).await
}

fn compact_target(outcome: &CompactSessionOutcome) -> Option<SessionLocator> {
    match outcome {
        CompactSessionOutcome::Compacted { target, .. }
        | CompactSessionOutcome::Failed { target, .. }
        | CompactSessionOutcome::Abandoned { target, .. } => Some(*target),
        CompactSessionOutcome::RecoveryRequired { recovery, .. } => Some(recovery.locator()),
        _ => None,
    }
}

fn record_runtime_target(path: &Path, target: SessionLocator) -> ProductResult {
    let mut state: ProductState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(path)?))?;
    state.runtime_target = Some(target.to_string());
    write_fsynced_json(path, &state)?;
    Ok(())
}

pub(crate) async fn recover_product_fixture(
    home: &Path,
    workspace: &Path,
    state_path: &Path,
) -> ProductResult<ProductProbeReport> {
    let state: ProductState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(state_path)?))?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        PlakPaths::explicit(home.to_path_buf()).runtime_database(),
    )?)?;
    let mut journal = ProductJournal::open(home.to_path_buf(), &state.source_product_session_id)?;
    match state.workflow {
        ProductWorkflow::Fork => {
            project_branch_request(&sessions, home, journal.product_session_id()).await?;
        }
        ProductWorkflow::Rebind => {
            project_branch_request(&sessions, home, journal.product_session_id()).await?;
        }
        ProductWorkflow::Compact => {
            Box::pin(recover_compact(&sessions, &mut journal, home, workspace)).await?;
        }
        _ => return Err("root and steer require actual-host recovery".into()),
    }
    probe_product_fixture(home, state_path)
}

pub(crate) async fn recover_compact(
    sessions: &AgentSessions,
    journal: &mut ProductJournal,
    home: &Path,
    workspace: &Path,
) -> ProductResult {
    let store: Arc<dyn SessionRecordStore> =
        Arc::new(crate::store::session_records::SqliteSessionStore::new(
            PlakPaths::explicit(home.to_path_buf()),
        ));
    let record = store.load(journal.product_session_id())?;
    plakous_hosted::runtime::project_session(sessions, &store, &record).await?;
    let record = store.load(journal.product_session_id())?;
    let source = record
        .plakous_locator
        .ok_or("compact recovery source missing")?;
    let evidence = BoundaryController::from_evidence_path(
        CrashBoundary::None,
        home.join("product-physical.jsonl"),
    )?;
    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("product-harness:compact-recover-open")?,
            source,
            compaction_runtime_binding(workspace, evidence)?,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("compact recovery source did not attach".into());
    };
    session.release().await?;
    project_branch_request(sessions, home, journal.product_session_id()).await
}

pub(crate) fn probe_product_fixture(
    home: &Path,
    state_path: &Path,
) -> ProductResult<ProductProbeReport> {
    let state: ProductState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(state_path)?))?;
    if state.version != PROTOCOL_VERSION {
        return Err("unsupported product state version".into());
    }
    let source = ProductJournal::open(home.to_path_buf(), &state.source_product_session_id)?;
    let snapshot = source.snapshot();
    let target = state
        .target_product_session_id
        .as_deref()
        .and_then(|id| ProductJournal::open(home.to_path_buf(), id).ok())
        .map(|journal| journal.snapshot());
    let evidence = read_evidence(home.join("product-physical.jsonl"))?;
    Ok(ProductProbeReport {
        workflow: state.workflow,
        product_session_id: snapshot.product_session_id,
        locator: snapshot.locator,
        queue_items: snapshot.queue_items,
        queue_paused: snapshot.queue_paused,
        target_product_session_id: target
            .as_ref()
            .map(|value| value.product_session_id.clone()),
        target_locator: target.as_ref().and_then(|value| value.locator.clone()),
        source_product_session_id: target
            .as_ref()
            .and_then(|value| value.source_product_session_id.clone()),
        source_workflow_operation_id: target.and_then(|value| value.source_workflow_operation_id),
        provider_dispatches: evidence
            .iter()
            .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
            .count(),
        effect_dispatches: evidence
            .iter()
            .filter(|record| record.kind == EvidenceKind::EffectDispatched)
            .count(),
    })
}
