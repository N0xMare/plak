//! A native compacted head with named state, a terminal job, fork and interpreter floor.
use plakous_agent::internal::host_move::{ProcessSessionId, ProcessSessionOwnerGeneration};
use plakous_agent::internal::{
    ContextEvaluationAudit, ForkLineage, InterpreterAvailability, InterpreterObservation,
    InterpreterResetReason, JobLedger, JobRow, JobTerminal, JobTerminalStatus,
    RuntimeCanonicalState, RuntimeCheckpoint, RuntimeManifest, SemanticCompactionPublication,
};
use plakous_model::{
    Actor, BranchId, CanonicalHistory, CheckpointId, CompactionBlock, ContentBlock,
    ConversationItem, ConversationView, Digest, DigestDomain, EntryId, HistorySegment,
    HistorySegmentKind, Provenance, SessionId, TextBlock, Timestamp, WorkingProjection,
};

type FixtureResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Rebuilds only synthetic fixture semantics; no executor or external state is invoked.
///
/// # Errors
/// Refuses a source whose manifest cannot represent the checked semantic fixture.
pub fn rich_checkpoint(source: &RuntimeCheckpoint) -> FixtureResult<RuntimeCheckpoint> {
    let prior = source.manifest();
    let canonical = canonical(source)?;
    let mut topology = prior.topology();
    topology.fork = Some(ForkLineage {
        session_id: SessionId::try_from(uuid::Uuid::from_u128(111))?,
        branch_id: BranchId::try_from(uuid::Uuid::from_u128(112))?,
        checkpoint_id: CheckpointId::try_from(uuid::Uuid::from_u128(113))?,
        branch_revision: 7,
    });
    let jobs = JobLedger::new(
        8,
        vec![JobRow {
            number: 7,
            command: "synthetic completed job".into(),
            started_at: Timestamp::new(1, 0)?,
            owner: plakous::tools::JobOwner {
                line: topology.line,
                generation: topology.line_generation,
                session_id: topology.session_id,
                branch_id: topology.branch_id,
                fence: topology.lease_fence,
            },
            session_id: ProcessSessionId::new([7; 16])?,
            owner_generation: ProcessSessionOwnerGeneration::new(3)?,
            terminal: Some(JobTerminal {
                status: JobTerminalStatus::Exited,
                exit_code: Some(7),
                stopper: None,
                elapsed: std::time::Duration::from_millis(2750),
                tail: "retained output".into(),
            }),
        }],
    )?;
    Ok(RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        topology,
        prior.lifecycle().clone(),
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        canonical,
        prior.binding().clone(),
        prior.tools().clone(),
        prior.ledger().clone(),
        prior.family_artifact().cloned(),
        Some(InterpreterObservation::new(
            17,
            InterpreterAvailability::Reset,
            Some(InterpreterResetReason::ProcessReopen),
            Some("last-cell".into()),
            None,
        )?),
        jobs,
        prior.named_state().put(
            "saved".into(),
            &serde_json::json!({"nested": [true, 42, "kept"]}),
        )?,
        prior.durability(),
    )?)?)
}

fn text(id: u128, value: &str) -> FixtureResult<HistorySegment> {
    Ok(HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(id))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(value, None)?)],
            Provenance::UserInput,
        )?],
    )?)
}

fn canonical(source: &RuntimeCheckpoint) -> FixtureResult<RuntimeCanonicalState> {
    let prior = source.manifest();
    let history = CanonicalHistory::new(vec![text(101, "retained original prompt")?])?;
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(102))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(CompactionBlock::new(
                "native summary",
            )?)],
            Provenance::ModelOutput,
        )?],
    )?;
    let successor = CanonicalHistory::new(vec![
        history.segments()[0].clone(),
        summary.clone(),
        text(103, "protected current-round tail")?,
    ])?;
    let audit = |label: &[u8]| -> FixtureResult<ContextEvaluationAudit> {
        Ok(ContextEvaluationAudit::new(
            Digest::sha256(DigestDomain::TokenMeasurement, 1, &[label])?,
            Digest::sha256(DigestDomain::ContextAdmission, 1, &[label])?,
            100,
        )?)
    };
    let publication = SemanticCompactionPublication::new(
        source.id(),
        Digest::sha256(DigestDomain::ProviderOperation, 1, &[b"move-native"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"route"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"model"])?,
        Digest::sha256(DigestDomain::ResolvedHarnessPlan, 1, &[b"plan"])?,
        audit(b"before")?,
        audit(b"after")?,
        None,
    )?;
    let context = prior.canonical().context().successful_native_compaction(
        &publication,
        &summary,
        &successor,
        history.identity(),
        ConversationView::from_shared(history, WorkingProjection::default()).identity(),
        [19; 32],
        5,
    )?;
    Ok(RuntimeCanonicalState::new(
        prior.canonical().stable_prefix().clone(),
        successor,
        WorkingProjection::default(),
        context,
    )?)
}
