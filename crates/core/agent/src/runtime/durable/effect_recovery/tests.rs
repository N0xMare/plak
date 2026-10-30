use plakous_model::{
    AttemptId, BranchId, CallId, Digest, DigestDomain, SessionId, ToolExecutionOwner, TurnId,
};

use super::{
    COMPLETED_TOOL_OUTCOME_TAG, DurableEffectEvidenceIdentity,
    DurableEffectEvidenceRecoveryRequest, DurableSessionAddress, ToolCallBlock,
    ToolExecutionResult, ToolExecutorId, ToolOutcome, ToolResultBlock, ToolResultKind,
    effect_evidence_matches_ledger, effect_evidence_topology_matches, retained_evidence_matches,
    terminal_tool_execution,
};
use crate::runtime::operation_record::operation_binding_component;
use crate::{
    CheckpointId, DurableOperationKind, DurableOperationRecord, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent,
};

type TestError = Box<dyn std::error::Error>;

fn digest(label: &[u8]) -> Result<Digest, TestError> {
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        2,
        &[label],
    )?)
}

fn component(label: &[u8]) -> Result<ReconstructiveComponent, TestError> {
    Ok(operation_binding_component(digest(label)?)?)
}

fn checkpoint(value: u128) -> Result<CheckpointId, TestError> {
    Ok(CheckpointId::try_from(uuid::Uuid::from_u128(value))?)
}

/// One effect record carried through the given states, in order.
fn effect_record(states: Vec<DurableOperationState>) -> Result<DurableOperationRecord, TestError> {
    let kind = DurableOperationKind::Effect {
        turn_id: TurnId::try_from(uuid::Uuid::from_u128(900))?,
        attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(901))?,
        call_id: CallId::try_from(uuid::Uuid::from_u128(902))?,
        source_index: 0,
    };
    let mut states = states.into_iter().enumerate();
    let (_, first) = states
        .next()
        .ok_or("the fixture needs at least one state")?;
    let mut record = DurableOperationRecord::new(
        1,
        kind,
        component(b"request")?,
        vec![DurableOperationTransition::new(checkpoint(910)?, first)],
    )?;
    for (index, state) in states {
        let at = checkpoint(910_u128.saturating_add(index as u128))?;
        record = record.append_transition(DurableOperationTransition::new(at, state))?;
    }
    Ok(record)
}

/// A claim the caller can present, and the evidence the ledger holds for it.
fn claimed() -> Result<(ReconstructiveComponent, Digest), TestError> {
    let evidence = component(b"claim")?;
    let integrity = evidence.integrity();
    Ok((evidence, integrity))
}

#[test]
fn a_prepared_effect_matches_only_an_unclaimed_request() -> Result<(), TestError> {
    let record = effect_record(vec![DurableOperationState::Prepared])?;
    // No claim was ever taken, so no claim digest is the only truthful one.
    assert!(effect_evidence_matches_ledger(&record, None, false));
    assert!(effect_evidence_matches_ledger(&record, None, true));
    assert!(!effect_evidence_matches_ledger(
        &record,
        Some(digest(b"invented")?),
        false
    ));
    Ok(())
}

#[test]
fn a_claimed_effect_matches_its_own_claim_and_no_other() -> Result<(), TestError> {
    let (evidence, integrity) = claimed()?;
    let record = effect_record(vec![
        DurableOperationState::Prepared,
        DurableOperationState::Claimed(evidence),
    ])?;
    assert!(effect_evidence_matches_ledger(
        &record,
        Some(integrity),
        false
    ));
    // A tampered digest is the case the predicate exists for.
    assert!(!effect_evidence_matches_ledger(
        &record,
        Some(digest(b"tampered")?),
        false
    ));
    // A missing claim is just as wrong as a wrong one.
    assert!(!effect_evidence_matches_ledger(&record, None, false));
    Ok(())
}

#[test]
fn a_dispatched_effect_needs_both_the_prior_claim_and_the_dispatch() -> Result<(), TestError> {
    let (evidence, integrity) = claimed()?;
    let record = effect_record(vec![
        DurableOperationState::Prepared,
        DurableOperationState::Claimed(evidence),
        DurableOperationState::Dispatched,
    ])?;
    assert!(effect_evidence_matches_ledger(
        &record,
        Some(integrity),
        true
    ));
    // The ledger says the request went out; a caller claiming it did not is
    // contradicting durable state, not supplying evidence.
    assert!(!effect_evidence_matches_ledger(
        &record,
        Some(integrity),
        false
    ));
    assert!(!effect_evidence_matches_ledger(
        &record,
        Some(digest(b"tampered")?),
        true
    ));
    assert!(!effect_evidence_matches_ledger(&record, None, true));
    Ok(())
}

#[test]
fn an_unknown_outcome_is_matched_the_same_way_as_a_dispatch() -> Result<(), TestError> {
    let (evidence, integrity) = claimed()?;
    let record = effect_record(vec![
        DurableOperationState::Prepared,
        DurableOperationState::Claimed(evidence),
        DurableOperationState::Dispatched,
        DurableOperationState::OutcomeUnknown(component(b"unknown")?),
    ])?;
    assert!(effect_evidence_matches_ledger(
        &record,
        Some(integrity),
        true
    ));
    assert!(!effect_evidence_matches_ledger(
        &record,
        Some(integrity),
        false
    ));
    Ok(())
}

fn address(checkpoint: u128) -> Result<DurableSessionAddress, TestError> {
    Ok(DurableSessionAddress::new(
        SessionId::try_from(uuid::Uuid::from_u128(700))?,
        BranchId::try_from(uuid::Uuid::from_u128(701))?,
        CheckpointId::try_from(uuid::Uuid::from_u128(checkpoint))?,
    ))
}

fn identity(source: DurableSessionAddress) -> Result<DurableEffectEvidenceIdentity, TestError> {
    Ok(DurableEffectEvidenceIdentity {
        source,
        recovery_operation: 1,
        workflow: digest(b"workflow")?,
        evidence_checkpoint: checkpoint(720)?,
    })
}

fn topology(
    source: DurableSessionAddress,
    checkpoint_id: CheckpointId,
    parent: Option<CheckpointId>,
) -> Result<crate::RuntimeTopology, TestError> {
    Ok(crate::RuntimeTopology {
        session_id: source.session_id,
        branch_id: source.branch_id,
        checkpoint_id,
        parent_checkpoint_id: parent,
        branch_revision: 1,
        fork: None,
        lease_fence: 1,
        commit_identity: digest(b"commit")?,
    })
}

#[test]
fn only_the_named_successor_of_the_named_source_answers_for_evidence() -> Result<(), TestError> {
    let source = address(710)?;
    let identity = identity(source)?;
    let evidence = checkpoint(720)?;

    let exact = topology(source, evidence, Some(source.checkpoint_id))?;
    assert!(effect_evidence_topology_matches(&exact, identity));

    // A checkpoint that is not the named successor cannot answer, even
    // though it sits on the same branch.
    let other_successor = topology(source, checkpoint(721)?, Some(source.checkpoint_id))?;
    assert!(!effect_evidence_topology_matches(
        &other_successor,
        identity
    ));

    // The successor must descend from the exact source the caller named.
    let other_parent = topology(source, evidence, Some(checkpoint(719)?))?;
    assert!(!effect_evidence_topology_matches(&other_parent, identity));

    // A root has no parent at all, so it is nobody's evidence successor.
    let rootless = topology(source, evidence, None)?;
    assert!(!effect_evidence_topology_matches(&rootless, identity));

    // A same-shaped checkpoint on another session is the case the gate
    // exists for: the store answered with something else entirely.
    let mut foreign = topology(source, evidence, Some(source.checkpoint_id))?;
    foreign.session_id = SessionId::try_from(uuid::Uuid::from_u128(799))?;
    assert!(!effect_evidence_topology_matches(&foreign, identity));

    let mut other_branch = topology(source, evidence, Some(source.checkpoint_id))?;
    other_branch.branch_id = BranchId::try_from(uuid::Uuid::from_u128(798))?;
    assert!(!effect_evidence_topology_matches(&other_branch, identity));
    Ok(())
}

fn execution(text: &str, dispatched: bool) -> Result<ToolExecutionResult, TestError> {
    let result = ToolResultBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(902))?,
        ToolExecutionOwner::Local,
        ToolResultKind::Success,
        vec![plakous_model::ToolResultPart::Text(
            plakous_model::TextBlock::new(text, None)?,
        )],
        plakous_model::ToolCorrelation::default(),
    )?;
    Ok(ToolExecutionResult::new(result, dispatched))
}

#[test]
fn retained_evidence_answers_only_for_the_same_outcome() -> Result<(), TestError> {
    let expected = execution("the tool wrote the file", true)?;
    let same = execution("the tool wrote the file", true)?;
    assert!(retained_evidence_matches(
        COMPLETED_TOOL_OUTCOME_TAG,
        &same,
        &expected
    ));

    // Tampered content under the right tag is the conflict this catches.
    let tampered = execution("the tool wrote a different file", true)?;
    assert!(!retained_evidence_matches(
        COMPLETED_TOOL_OUTCOME_TAG,
        &tampered,
        &expected
    ));

    // The same result claimed as never dispatched is a different fact.
    let undispatched = execution("the tool wrote the file", false)?;
    assert!(!retained_evidence_matches(
        COMPLETED_TOOL_OUTCOME_TAG,
        &undispatched,
        &expected
    ));

    // Decodable bytes under another tag are not this record.
    assert!(!retained_evidence_matches(
        "completed-tool-outcome-v2",
        &same,
        &expected
    ));
    assert!(!retained_evidence_matches("", &same, &expected));
    Ok(())
}

fn recovery_call() -> Result<ToolCallBlock, TestError> {
    Ok(ToolCallBlock::new(
        CallId::try_from(uuid::Uuid::from_u128(902))?,
        plakous_model::ToolName::new("await_agents")?,
        ToolExecutionOwner::Local,
        plakous_model::ToolCallState::Requested,
        plakous_model::ToolCorrelation::default(),
        "{}",
    )?)
}

fn recovery_request(
    subject: Option<&str>,
) -> Result<DurableEffectEvidenceRecoveryRequest, TestError> {
    let outcome = ToolOutcome::success_text("woken")?;
    let outcome = match subject {
        Some(subject) => outcome.with_presentation_subject(subject),
        None => outcome,
    };
    Ok(DurableEffectEvidenceRecoveryRequest::new(
        "effect-evidence-1",
        address(710)?,
        1,
        None,
        true,
        ToolExecutorId::new("local.executor")?,
        outcome,
    )?)
}

#[tokio::test]
async fn prepared_undispatched_cut_refuses_external_success_evidence() -> Result<(), TestError> {
    use crate::runtime::continuation::runtime_component;
    use crate::runtime::durable::tests::{
        SessionUpdateFixtureMode, landed_round_abandonment::completed_tool_round_cut,
    };
    use crate::runtime::durable::turn_recovery::review_tests::checkpoint_with_review_state;
    let source =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
    let effect = source
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .find(|record| matches!(record.kind(), DurableOperationKind::Effect { .. }))
        .ok_or("effect")?;
    let call = super::decode_effect_call(effect)?.into_call();
    let request: crate::runtime::DurableUndispatchedToolRequest = (
        "proven-not-dispatched-tool-v1".into(),
        digest(b"undispatched")?,
        call,
        plakous_model::EntryId::try_from(uuid::Uuid::from_u128(0xEFFF))?,
        execution("not dispatched", false)?,
    );
    let rejected = DurableOperationRecord::new(
        effect.operation_id(),
        effect.kind().clone(),
        runtime_component(
            crate::RuntimeComponentKind::Operation,
            serde_json::to_vec(&request)?,
        )?,
        vec![DurableOperationTransition::new(
            source.id(),
            DurableOperationState::Prepared,
        )],
    )?;
    let records = source
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .map(|record| {
            if record.operation_id() == effect.operation_id() {
                rejected.clone()
            } else {
                record.clone()
            }
        })
        .collect();
    let cut = checkpoint_with_review_state(
        &source,
        records,
        source.manifest().canonical().history().clone(),
        false,
    )?;
    let topology = cut.manifest().topology();
    let external = DurableEffectEvidenceRecoveryRequest::new(
        "evidence:undispatched",
        DurableSessionAddress::new(topology.session_id, topology.branch_id, cut.id()),
        effect.operation_id(),
        None,
        true,
        ToolExecutorId::new("probe.executor")?,
        ToolOutcome::success_text("invented external success")?,
    )?;
    let identity = DurableEffectEvidenceIdentity::new(&external)?;
    assert!(matches!(
        super::prepare_effect_evidence_publication(&cut, identity, &external)?,
        super::EffectEvidencePreparation::Conflict
    ));
    Ok(())
}

#[test]
fn undispatched_request_decoder_checks_the_recorded_tag() -> Result<(), TestError> {
    for tag in ["proven-not-dispatched-tool-v1", "other-tool-v1"] {
        let request: crate::runtime::DurableUndispatchedToolRequest = (
            tag.into(),
            digest(b"request")?,
            recovery_call()?,
            plakous_model::EntryId::try_from(uuid::Uuid::from_u128(0xEFFF))?,
            execution("not dispatched", false)?,
        );
        let original = effect_record(vec![DurableOperationState::Prepared])?;
        let record = DurableOperationRecord::new(
            original.operation_id(),
            original.kind().clone(),
            crate::runtime::continuation::runtime_component(
                crate::RuntimeComponentKind::Operation,
                serde_json::to_vec(&request)?,
            )?,
            original.transitions().to_vec(),
        )?;
        assert_eq!(
            super::decode_effect_call(&record).is_ok(),
            tag == "proven-not-dispatched-tool-v1"
        );
    }
    Ok(())
}

#[test]
fn recovered_terminal_execution_carries_the_presentation_subject() -> Result<(), TestError> {
    let call = recovery_call()?;
    let execution = terminal_tool_execution(&call, &recovery_request(Some("c1,c2"))?)?;
    assert_eq!(execution.presentation_subject.as_deref(), Some("c1,c2"));

    let bare = terminal_tool_execution(&call, &recovery_request(None)?)?;
    assert_eq!(bare.presentation_subject.as_deref(), None);
    // Same canonical result and dispatch. The subject is not evidence.
    assert!(retained_evidence_matches(
        COMPLETED_TOOL_OUTCOME_TAG,
        &bare,
        &execution
    ));
    Ok(())
}

#[test]
fn an_already_terminal_effect_matches_nothing() -> Result<(), TestError> {
    let (evidence, integrity) = claimed()?;
    let record = effect_record(vec![
        DurableOperationState::Prepared,
        DurableOperationState::Claimed(evidence),
        DurableOperationState::Dispatched,
        DurableOperationState::Completed(component(b"outcome")?),
    ])?;
    // A completed effect is answered by the receipt path, never by fresh
    // evidence; letting evidence match here would overwrite a settled fact.
    assert!(!effect_evidence_matches_ledger(
        &record,
        Some(integrity),
        true
    ));
    assert!(!effect_evidence_matches_ledger(&record, None, false));
    Ok(())
}
