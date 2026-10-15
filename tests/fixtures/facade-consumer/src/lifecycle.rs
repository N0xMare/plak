//! Executable offline embedder proof: no repository test-support dependency.
use plakous::{
    agent::{AgentBuilder, TurnResultOutcome},
    construction::bind_custom_model_authority,
    model::{
        Actor, BlockStart, BlockStarted, CallId, Capability, CapabilityConstraints, CapabilitySet,
        CapacityCertificate, CapacityRule, ConfiguredLanguageModel, ContentBlock, ContextContract,
        ContextEvidenceSource, ConversationItem, CountCertificateKind, CountObservation, Digest,
        DigestDomain, EntryId, EvidenceFreshness, FactProvenance, FactSource, FeatureSupport,
        FinishDetails, FinishReason, LanguageModel, MeasurementCertificate, ModelAdapterProfileId,
        ModelError, ModelEvent, ModelFamilyId, ModelId, ModelPreparationContext,
        ModelPreparationError, ModelResponse, ModelStream, OutputLocation, PreparedCacheUse,
        PreparedCallError, PreparedCallRecord, PreparedModelCall, PreparedModelProjection,
        PreparedTokenProjection, ProtocolId, Provenance, ResolvedModelDescriptor, ResponseStarted,
        RouteId, RuntimeModelAuthority, RuntimeModelAuthorityError, RuntimeModelAuthorityOperation,
        Support, TextBlock, Timestamp, ToolCallBlock, ToolCallState, ToolCallerLineage,
        ToolCorrelation, ToolExecutionOwner, ToolName, ValidatedRequestPlan,
    },
    sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId, SessionRuntimeBinding,
    },
    tools::WorkspacePolicy,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
#[path = "lifecycle_configuration.rs"]
mod configuration;
type ResultBox<T> = Result<T, Box<dyn std::error::Error>>;

struct ConsumerModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    calls: AtomicUsize,
    started: tokio::sync::Notify,
}
impl ConsumerModel {
    fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }
    fn new() -> ResultBox<Self> {
        let (protocol, route, model, family, profile) = (
            "consumer-protocol",
            "consumer-route",
            "consumer-model",
            "consumer-family",
            "consumer-profile",
        );
        let time = Timestamp::new(1, 0)?;
        let features = [
            Capability::Tools,
            Capability::Continuation,
            Capability::Streaming,
        ]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    FactProvenance::new(FactSource::TestFixture, time, None),
                ),
            )
        })
        .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new(protocol)?,
            RouteId::new(route)?,
            ModelId::new(model)?,
            None,
            Some(ModelFamilyId::new(family)?),
            ModelAdapterProfileId::new(profile)?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"offline-consumer"])?;
        // One authority, not two: the helper mints the class binding the
        // certificates need and re-binds after the contract changes the digest.
        let (descriptor, authority) = bind_custom_model_authority(
            descriptor,
            binding,
            binding,
            1,
            time,
            None,
            |class| -> ResultBox<ContextContract> {
                let freshness = EvidenceFreshness::new(time, None)?;
                Ok(ContextContract::new(
                    ProtocolId::new(protocol)?,
                    "consumer-accounting-v1",
                    "consumer-projection-v1",
                    "consumer-reservation-v1",
                    CapacityCertificate::custom_envelope(
                        "consumer-capacity",
                        "v1",
                        CapacityRule::ProviderReservedInput {
                            reported_context_window: None,
                            maximum_projected_input: 1_000_000,
                            auto_compact_input: None,
                        },
                        class,
                        freshness,
                        binding,
                        "v1",
                    )?,
                    vec![MeasurementCertificate::new(
                        "consumer-counter",
                        "v1",
                        "wire-bytes",
                        "v1",
                        ContextEvidenceSource::CallerDeclaration,
                        CountCertificateKind::CustomEnvelopeUpperBound {
                            declaration: binding,
                        },
                        Some(class),
                        freshness,
                    )?],
                )?)
            },
        )?;
        Ok(Self {
            descriptor,
            authority,
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
        })
    }
}
impl LanguageModel for ConsumerModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            self.authority.validate(&self.descriptor, checked_at)?;
            Ok(self.authority.clone())
        })
    }
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        if !self.authority.same_selected_authority(context.authority()) {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let wire =
            serde_json::to_vec(&(plan.request().model_input_identity(), plan.decisions())).unwrap();
        let size = wire.len() as u64;
        let digest =
            Digest::sha256(DigestDomain::Blob, 1, &[&wire]).map_err(PreparedCallError::from)?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "consumer-lowering-v1",
            "consumer-projection-v1",
            "consumer-accounting-v1",
            digest,
            size,
            PreparedTokenProjection::new(
                digest,
                size,
                Some(CountObservation::new("consumer-counter", "v1", size).unwrap()),
            )?,
            PreparedCacheUse::None,
            None,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        Ok(PreparedModelCall::new(record, move |call_context| {
            Box::pin(async move {
                let index = self.calls.fetch_add(1, Ordering::SeqCst);
                if matches!(index, 2 | 4) {
                    self.started.notify_one();
                    return Ok(Box::pin(futures_util::stream::once(async move {
                        call_context.cancellation().cancelled().await;
                        Err(ModelError::cancelled())
                    })) as ModelStream);
                }
                let invocation = (index == 0).then_some(("consumer_tool", "consumer-input"));
                let block = match invocation {
                    Some((name, input)) => ContentBlock::ToolCall(
                        ToolCallBlock::new(
                            CallId::try_from(uuid::Uuid::from_u128(100 + index as u128)).unwrap(),
                            ToolName::new(name).unwrap(),
                            ToolExecutionOwner::Local,
                            ToolCallState::Requested,
                            ToolCorrelation::new(None, None, ToolCallerLineage::Direct).unwrap(),
                            serde_json::to_string(input).unwrap(),
                        )
                        .unwrap(),
                    ),
                    None => ContentBlock::Text(TextBlock::new("consumer completed", None).unwrap()),
                };
                let output = ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::from_u128(1000 + index as u128)).unwrap(),
                    Actor::Assistant,
                    vec![block.clone()],
                    Provenance::ModelOutput,
                )
                .unwrap();
                let finish = if invocation.is_some() {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                let events = vec![
                    Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                        None,
                        plan.digest(),
                    ))),
                    Ok(ModelEvent::BlockStarted(BlockStarted::new(
                        output.id(),
                        OutputLocation::new(0, 0),
                        BlockStart::Complete(block),
                    ))),
                    Ok(ModelEvent::ResponseCompleted(ModelResponse::new(
                        vec![output],
                        FinishDetails::new(finish, None),
                        None,
                        None,
                        plan.digest(),
                    ))),
                ];
                Ok(Box::pin(futures_util::stream::iter(events)) as ModelStream)
            })
        }))
    }
}
fn runtime(model: Arc<ConsumerModel>, path: &std::path::Path) -> ResultBox<SessionRuntimeBinding> {
    let tools = super::custom_tool::register_custom_tool()?;
    let configuration = configuration::provider_configuration(&model, tools.definitions())?;
    let configured =
        ConfiguredLanguageModel::custom(model.clone(), configuration, model.authority.clone())?;
    Ok(SessionRuntimeBinding::new(AgentBuilder::new(
        configured,
        tools,
        Arc::new(WorkspacePolicy::host(path, ".")?),
    )))
}
pub(crate) async fn run() -> ResultBox<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), lifecycle()).await??;
    println!(
        "external facade lifecycle: custom model/tool, cancellation, SQLite reopen, joined shutdown passed"
    );
    Ok(())
}
async fn lifecycle() -> ResultBox<()> {
    let directory = tempfile::tempdir()?;
    let db = directory.path().join("runtime.sqlite3");
    let model = Arc::new(ConsumerModel::new()?);
    let sessions = AgentSessions::new(SessionBackend::sqlite(&db)?)?;
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("consumer-create")?,
            runtime(model.clone(), directory.path())?,
        ))
        .await?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("unexpected create outcome".into());
    };
    let result = session
        .agent()
        .prompt("call consumer_tool")
        .await?
        .result()
        .await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    let history = serde_json::to_string(&result.conversation().items().iter().collect::<Vec<_>>())?;
    assert!(
        history.contains("consumer tool"),
        "tool result must be in canonical history"
    );
    let turn = session.agent().prompt("wait for cancellation").await?;
    model.started.notified().await;
    turn.control().cancel().await?;
    assert!(matches!(
        turn.result().await?.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    let before = serde_json::to_string(
        &session
            .agent()
            .conversation()
            .items()
            .iter()
            .collect::<Vec<_>>(),
    )?;
    let locator = session.release().await?;
    drop(sessions);
    let sessions = AgentSessions::new(SessionBackend::sqlite(&db)?)?;
    let opened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("consumer-reopen")?,
            locator,
            runtime(model.clone(), directory.path())?,
        ))
        .await?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("reopen did not restore a quiescent session".into());
    };
    assert_eq!(
        before,
        serde_json::to_string(
            &session
                .agent()
                .conversation()
                .items()
                .iter()
                .collect::<Vec<_>>()
        )?
    );
    assert!(matches!(
        session
            .agent()
            .prompt("continue after reopen")
            .await?
            .result()
            .await?
            .outcome(),
        TurnResultOutcome::Completed(_)
    ));
    let agent = session.agent().clone();
    let active = agent.prompt("wait for shutdown").await?;
    model.started.notified().await;
    session.release().await?;
    assert!(matches!(
        active.result().await?.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    agent.shutdown().await?;
    assert!(agent.prompt("after shutdown").await.is_err());
    Ok(())
}
#[allow(dead_code)]
async fn process_exit_release(
    session: plakous::sessions::OpenedSession,
    peers: &[plakous::agent::Agent],
    sessions: &plakous::sessions::AgentSessions,
    job_budget: std::time::Duration,
) -> Result<plakous::sessions::SessionLocator, plakous::sessions::AgentSessionsError> {
    sessions.passive_checkpoint().await?;
    plakous::sessions::abandon_and_release(peers, job_budget).await?;
    session.abandon_and_release(peers, job_budget).await
}

#[allow(dead_code)]
async fn same_branch_owner_transfer(
    session: plakous::sessions::OpenedSession,
) -> Result<plakous::sessions::SessionLocator, plakous::sessions::AgentSessionsError> {
    session.release_for_transfer().await
}
