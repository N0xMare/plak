//! Compaction identities, request records and receipt validation.

use crate::runtime::OperationId;
use crate::runtime::checkpoint::identity::{CheckpointCommitFacts, checkpoint_commit_identity};
use crate::runtime::compaction::fold::{
    compaction_summary_segment, fold_compaction_success, fold_native_compaction_success,
    native_compaction_successor, reconstruct_compaction_cut, record_summarized_turns,
};
use crate::runtime::compaction::{
    DurableCompactionIdentity, DurableCompactionPublicationEvidence, DurableCompactionRequestRecord,
};
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::durable::branch::validate_session_operation_id;
use crate::runtime::durable::compaction_recovery::{
    decode_compaction_failure_evidence, decode_paid_compaction_operations,
};
use crate::runtime::operation_record::compaction_receipt_lifecycle_ok;
use crate::{
    AgentError, AgentErrorKind, CheckpointId, CommittedCheckpoint, ContextEvaluationAudit,
    DurableOperationState, ReconstructiveComponent, RuntimeComponentKind, RuntimeLifecycle,
    RuntimeManifest, RuntimeWorkingRoot, SessionStoreError, SessionWorkflowId, StoredCheckpoint,
};
use plakous_model::{
    AttemptId, Digest, DigestDomain, ModelCallRecoveryResult, ModelResponse, PreparedCallRecord,
    ProviderCallRecoveryLocator,
};

/// Encodes exact operation evidence with a bounded encoded buffer.
pub(in crate::runtime) fn compaction_operation_component(
    value: &impl serde::Serialize,
) -> Result<ReconstructiveComponent, AgentError> {
    struct BoundedJson {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl std::io::Write for BoundedJson {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len()
                > crate::manifest::MAX_RUNTIME_OPERATION_COMPONENT_BYTES - self.bytes.len()
            {
                self.exceeded = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "operation byte budget exceeded",
                ));
            }
            let required = self.bytes.len() + bytes.len();
            if required > self.bytes.capacity() {
                let capacity = required
                    .max(self.bytes.capacity() * 2)
                    .min(crate::manifest::MAX_RUNTIME_OPERATION_COMPONENT_BYTES);
                self.bytes.reserve_exact(capacity - self.bytes.len());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut encoded = BoundedJson {
        bytes: Vec::new(),
        exceeded: false,
    };
    if serde_json::to_writer(&mut encoded, value).is_err() {
        return Err(AgentError::new(
            AgentErrorKind::Internal,
            if encoded.exceeded {
                "runtime reconstruction component violates its byte bound: compaction operation exceeds 67108864 bytes"
            } else {
                "compaction operation JSON could not be encoded"
            },
        ));
    }
    runtime_component(RuntimeComponentKind::Operation, encoded.bytes)
}

impl DurableCompactionIdentity {
    pub(in crate::runtime) fn new(
        operation_id: &str,
        source: DurableSessionAddress,
    ) -> Result<Self, AgentError> {
        validate_session_operation_id(operation_id)?;
        let workflow = Digest::sha256(
            DigestDomain::ProviderOperation,
            3,
            &[
                b"plakous-agent/session-compaction-workflow-v1",
                operation_id.as_bytes(),
                source.session_id.as_uuid().as_bytes(),
                source.branch_id.as_uuid().as_bytes(),
                source.checkpoint_id.as_uuid().as_bytes(),
            ],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "compaction workflow identity failed",
            )
        })?;
        let workflow_id = SessionWorkflowId::new(*workflow.bytes()).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "compaction workflow identity was reserved",
            )
        })?;
        Ok(Self {
            source,
            workflow_id,
            attempt_id: AttemptId::try_from(compaction_uuid(workflow_id, b"provider-attempt")?)
                .map_err(|error| AgentError::from_identity(&error))?,
            prepared_checkpoint: compaction_checkpoint_id(workflow_id, b"prepared")?,
            prepared_locator_checkpoint: compaction_checkpoint_id(
                workflow_id,
                b"prepared-locator-bound",
            )?,
            dispatched_checkpoint: compaction_checkpoint_id(workflow_id, b"dispatched")?,
            learned_locator_checkpoint: compaction_checkpoint_id(
                workflow_id,
                b"learned-locator-bound",
            )?,
            completed_checkpoint: compaction_checkpoint_id(workflow_id, b"completed")?,
            failure_evidence_checkpoint: compaction_checkpoint_id(
                workflow_id,
                b"failure-evidence",
            )?,
            abandonment_evidence_checkpoint: compaction_checkpoint_id(
                workflow_id,
                b"abandonment-evidence",
            )?,
            success_checkpoint: compaction_checkpoint_id(workflow_id, b"success")?,
            failure_checkpoint: compaction_checkpoint_id(workflow_id, b"failed")?,
        })
    }
}

pub(super) fn compaction_uuid(
    workflow_id: SessionWorkflowId,
    role: &[u8],
) -> Result<uuid::Uuid, AgentError> {
    let digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/session-compaction-derived-id-v1",
            workflow_id.as_bytes(),
            role,
        ],
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Identity, "compaction identity failed"))?;
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes))
}

pub(in crate::runtime) fn compaction_checkpoint_id(
    workflow_id: SessionWorkflowId,
    role: &[u8],
) -> Result<CheckpointId, AgentError> {
    CheckpointId::try_from(compaction_uuid(workflow_id, role)?)
        .map_err(|error| AgentError::from_identity(&error))
}

pub(in crate::runtime) fn compaction_attempt_id(
    workflow_id: SessionWorkflowId,
    role: &[u8],
) -> Result<AttemptId, AgentError> {
    AttemptId::try_from(compaction_uuid(workflow_id, role)?)
        .map_err(|error| AgentError::from_identity(&error))
}

impl DurableCompactionRequestRecord {
    pub(in crate::runtime) fn source(&self) -> DurableSessionAddress {
        DurableSessionAddress::new(
            self.source_session,
            self.source_branch,
            self.source_checkpoint,
        )
    }

    pub(in crate::runtime) fn before_audit(&self) -> Result<ContextEvaluationAudit, AgentError> {
        ContextEvaluationAudit::new(
            self.before_measurement,
            self.before_status,
            self.before_input_tokens,
        )
        .map_err(runtime_manifest_error)
    }
}

fn finalized_compaction_ledger_evidence(
    manifest: &RuntimeManifest,
    operation: OperationId,
) -> Result<&ReconstructiveComponent, AgentError> {
    manifest
        .ledger()
        .operation(operation.0)
        .and_then(|record| match record.state() {
            DurableOperationState::Finalized(evidence) => Some(evidence),
            _ => None,
        })
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))
}

/// Which deterministic finalization of a compaction a receipt names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum CompactionReceiptKind {
    Success,
    Failure,
    /// The one-commit closure of an interrupted workflow.
    Abandonment,
}

pub(in crate::runtime) fn validate_compaction_receipt(
    recovered: &CommittedCheckpoint,
    stored: &StoredCheckpoint,
    source_stored: &StoredCheckpoint,
    previous_stored: &StoredCheckpoint,
    identity: DurableCompactionIdentity,
    kind: CompactionReceiptKind,
) -> Result<(), AgentError> {
    let successful = kind == CompactionReceiptKind::Success;
    let checkpoint = stored.checkpoint();
    let manifest = checkpoint.manifest();
    let topology = manifest.topology();
    let operation = recovered.operation();
    let receipt = recovered.receipt();
    let previous = receipt.previous();
    let head = receipt.head();
    let expected_target = match kind {
        CompactionReceiptKind::Success => identity.success_checkpoint,
        CompactionReceiptKind::Failure => identity.failure_checkpoint,
        CompactionReceiptKind::Abandonment => identity.abandonment_evidence_checkpoint,
    };
    let expected_commit = checkpoint_commit_identity(CheckpointCommitFacts {
        checkpoint_id: topology.checkpoint_id,
        parent_checkpoint_id: topology
            .parent_checkpoint_id
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?,
        branch_revision: topology.branch_revision,
        lifecycle: manifest.lifecycle(),
        history_identity: manifest.canonical().history().identity(),
        ledger: manifest.ledger(),
        context_identity: manifest.canonical().context().identity(),
        family_artifact: manifest.family_artifact(),
    })?;
    let bundle = stored.bundle();
    if operation.expected() != previous
        || operation.lease().session() != identity.source.session()
        || operation.checkpoint() != stored.reference()
        || operation.checkpoint_commit_identity() != topology.commit_identity
        || receipt.checkpoint() != stored.reference()
        || head.checkpoint() != stored.reference()
        || previous.checkpoint() != previous_stored.reference()
        || previous.session() != identity.source.session()
        || head.session() != identity.source.session()
        || head.revision().get()
            != previous
                .revision()
                .get()
                .checked_add(1)
                .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?
        || topology.checkpoint_id != expected_target
        || topology.checkpoint_id != stored.reference().checkpoint_id()
        || topology.parent_checkpoint_id != Some(previous.checkpoint().checkpoint_id())
        || topology.branch_revision != head.revision().get()
        || topology.lease_fence != operation.lease().fence().get()
        || topology.commit_identity != expected_commit
        || (topology.session_id, topology.branch_id)
            != (identity.source.session_id, identity.source.branch_id)
        || checkpoint.integrity() != stored.reference().integrity()
        || bundle.root() != stored.reference().root()
        || bundle.checkpoint_integrity() != stored.reference().integrity()
        || !matches!(
            manifest.lifecycle(),
            RuntimeLifecycle::Quiescent | RuntimeLifecycle::Working { .. }
        )
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }

    let source_checkpoint = source_stored.checkpoint();
    let source_manifest = source_checkpoint.manifest();
    let source_topology = source_manifest.topology();
    if source_stored.reference().checkpoint_id() != identity.source.checkpoint_id
        || source_topology.checkpoint_id != identity.source.checkpoint_id
        || (source_topology.session_id, source_topology.branch_id)
            != (identity.source.session_id, identity.source.branch_id)
        || !compaction_receipt_lifecycle_ok(
            source_manifest.lifecycle(),
            manifest.lifecycle(),
            successful,
        )
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }

    let operations = decode_paid_compaction_operations(checkpoint, identity)?;
    let previous_operations =
        decode_paid_compaction_operations(previous_stored.checkpoint(), identity)?;
    if operations.request.version < 3 {
        reconstruct_compaction_cut(source_manifest, identity, &operations.request)?;
    }
    if operations.compaction != previous_operations.compaction
        || operations.provider != previous_operations.provider
        || operations.prepared != previous_operations.prepared
        || serde_json::to_vec(&operations.request).ok()
            != serde_json::to_vec(&previous_operations.request).ok()
        || operations.request.source_history != source_manifest.canonical().history().identity()
        || source_manifest
            .canonical()
            .stable_prefix()
            .resolved_harness_plan_identity()
            != Some(operations.request.source_harness_plan)
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }

    if manifest.harness_plan_component() != source_manifest.harness_plan_component()
        || manifest.binding() != source_manifest.binding()
        || manifest.tools() != source_manifest.tools()
        || manifest.canonical().stable_prefix().identity()
            != source_manifest.canonical().stable_prefix().identity()
        || manifest.durability() != source_manifest.durability()
    {
        return Err(AgentError::from_store(SessionStoreError::corrupt()));
    }

    let compaction_evidence =
        finalized_compaction_ledger_evidence(manifest, operations.compaction)?;
    let provider_evidence = finalized_compaction_ledger_evidence(manifest, operations.provider)?;
    if successful {
        if previous.checkpoint().checkpoint_id() != identity.completed_checkpoint
            || manifest.family_artifact().is_some()
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let response: ModelResponse = serde_json::from_slice(provider_evidence.bytes())
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let canonical_provider = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&response)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        )?;
        response
            .validate_for_commit()
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        if canonical_provider != *provider_evidence
            || response.plan_digest() != operations.prepared.projection().plan()
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let publication: DurableCompactionPublicationEvidence =
            serde_json::from_slice(compaction_evidence.bytes())
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        let canonical_publication = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&publication)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        )?;
        let after = ContextEvaluationAudit::new(
            publication.after_measurement,
            publication.after_status,
            publication.after_input_tokens,
        )
        .map_err(runtime_manifest_error)?;
        let provenance = manifest
            .canonical()
            .context()
            .compaction_provenance()
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let summary = compaction_summary_segment(identity, &response)?;
        let native = operations.request.version >= 3;
        let (summary, successor, expected_cut, expected) = if native {
            let successor =
                native_compaction_successor(source_manifest.canonical().history(), summary.clone())
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let (summary, successor) =
                record_summarized_turns(source_manifest.canonical().history(), summary, successor)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let expected = fold_native_compaction_success(
                previous_stored.checkpoint().manifest(),
                identity,
                operations.compaction,
                operations.provider,
                &operations.request,
                &summary,
                successor.clone(),
                &response,
                after,
            )?;
            (summary, successor, operations.request.cut, expected)
        } else {
            let cut = reconstruct_compaction_cut(source_manifest, identity, &operations.request)?;
            let successor = cut
                .build_successor(&summary)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let (summary, successor) =
                record_summarized_turns(source_manifest.canonical().history(), summary, successor)
                    .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
            let expected = fold_compaction_success(
                previous_stored.checkpoint().manifest(),
                identity,
                operations.compaction,
                operations.provider,
                &operations.request,
                &cut,
                &summary,
                successor.clone(),
                &response,
                provenance.before_audit(),
                after,
            )?;
            (summary, successor, cut.identity(), expected)
        };
        if canonical_publication != *compaction_evidence
            || publication.version != 1
            || publication.workflow_id != identity.workflow_id
            || publication.source_checkpoint != identity.source.checkpoint_id
            || publication.cut != expected_cut
            || publication.successor_history != successor.identity()
            || provenance.source_checkpoint() != identity.source.checkpoint_id
            || provenance.source_history_identity() != operations.request.source_history
            || provenance.operation_identity() != publication.operation
            || provenance.summary_segment_identity() != summary.identity()
            || provenance.after_audit() != after
            // Publication proved a strict shrink; a receipt without one was
            // never a valid fold.
            || after.input_tokens() >= provenance.before_audit().input_tokens()
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        if !manifest
            .jobs()
            .is_terminal_extension_of(previous_stored.checkpoint().manifest().jobs())
            || manifest.canonical().history() != &expected.history
            || manifest.ledger() != &expected.ledger
            || manifest.canonical().context() != &expected.context
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
    } else {
        // A failure publishes from its recorded outcome; an abandonment closes
        // the interrupted workflow from any head before that outcome.
        let previous_ok = match kind {
            CompactionReceiptKind::Abandonment => matches!(
                previous_stored.checkpoint().manifest().lifecycle(),
                RuntimeLifecycle::Working {
                    root: RuntimeWorkingRoot::SessionWorkflow(workflow_id),
                    ..
                } if *workflow_id == identity.workflow_id
            ),
            _ => [
                identity.completed_checkpoint,
                identity.failure_evidence_checkpoint,
            ]
            .contains(&previous.checkpoint().checkpoint_id()),
        };
        if !previous_ok
            || (kind == CompactionReceiptKind::Abandonment
                && (!matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent)
                    || compaction_evidence != provider_evidence))
            || !manifest
                .jobs()
                .is_terminal_extension_of(previous_stored.checkpoint().manifest().jobs())
            || manifest.canonical().history() != source_manifest.canonical().history()
            || manifest.canonical().context() != source_manifest.canonical().context()
            || manifest.family_artifact() != source_manifest.family_artifact()
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
        let failure = decode_compaction_failure_evidence(compaction_evidence)?;
        if failure.kind() == AgentErrorKind::OutcomeUnknown
            || !provider_final_evidence_is_exact(
                manifest,
                operations.provider,
                provider_evidence,
                &operations.prepared,
            )?
        {
            return Err(AgentError::from_store(SessionStoreError::corrupt()));
        }
    }
    Ok(())
}

fn provider_final_evidence_is_exact(
    manifest: &RuntimeManifest,
    provider: OperationId,
    evidence: &ReconstructiveComponent,
    prepared: &PreparedCallRecord,
) -> Result<bool, AgentError> {
    if let Ok(response) = serde_json::from_slice::<ModelResponse>(evidence.bytes()) {
        return Ok(runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&response)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        )? == *evidence
            && response.validate_for_commit().is_ok()
            && response.plan_digest() == prepared.projection().plan());
    }
    if let Ok(result) = serde_json::from_slice::<ModelCallRecoveryResult>(evidence.bytes()) {
        let record = manifest
            .ledger()
            .operation(provider.0)
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let locator = record
            .latest_locator()
            .and_then(|component| {
                serde_json::from_slice::<ProviderCallRecoveryLocator>(component.bytes()).ok()
            })
            .or_else(|| prepared.recovery_locator().cloned())
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let request = prepared
            .recovery_request_with_locator(locator)
            .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?;
        return Ok(runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&result)
                .map_err(|_| AgentError::from_store(SessionStoreError::corrupt()))?,
        )? == *evidence
            && result.validate_for_request(&request).is_ok());
    }
    Ok(decode_compaction_failure_evidence(evidence).is_ok())
}

#[cfg(test)]
mod encoding_tests {
    use super::compaction_operation_component;

    #[test]
    fn bounded_operation_encoding_preserves_exact_json() -> Result<(), Box<dyn std::error::Error>> {
        let value = serde_json::json!({"text": "quotes: \"; escaped: \0; Unicode: λ"});
        let component = compaction_operation_component(&value)?;
        assert_eq!(component.bytes(), serde_json::to_vec(&value)?);
        Ok(())
    }

    #[test]
    fn operation_encoding_failure_does_not_disclose_serializer_details()
    -> Result<(), Box<dyn std::error::Error>> {
        struct Failing;
        impl serde::Serialize for Failing {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("private serializer detail"))
            }
        }
        let error = compaction_operation_component(&Failing)
            .err()
            .ok_or("serializer must fail")?;
        assert_eq!(
            error.message(),
            "compaction operation JSON could not be encoded"
        );
        Ok(())
    }
}
