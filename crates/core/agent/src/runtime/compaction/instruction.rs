//! Closed summary instructions, selected from durable harness-fold evidence.

use super::{COMPACTION_INSTRUCTION, DurableCompactionRequestRecord};
use crate::{
    DurableOperationKind, DurableOperationRecord, DurableOperationState, RuntimeOperationLedger,
};
use plakous_model::{FinishReason, ModelOperationRoot, ModelResponse};

const TIGHT_INSTRUCTION: &str = "Summarize the conversation above so the work can continue from your summary alone. The summary replaces the conversation, including any earlier summary, in your own working context; it is not a reply to the user, so instructions about what a reply may show do not apply to it. Begin with the exact line `Summary of earlier work:` on its own, then write each section below once, in this order, with exact names:\nRequests: the user's requests, oldest first; this instruction is not one of them. Quote only the newest request that is not in progress verbatim, up to about 150 words each; for a longer request, quote up to that limit and keep every requirement in the user's own words. Merge all earlier requests into at most 5 lines. Mark each request done, interrupted, unanswered or in progress. For a done request, copy exactly the values it produced: what it wrote, reported or answered. If its line cannot hold those values, put them in Values; never drop values to meet a line cap. A request still in progress continues verbatim after this summary; name it without quoting it again.\nConstraints: every rule, preference or requirement the user set in any request that still applies, quoted exactly. List each one here even if it is also quoted under Requests.\nValues: every value the remaining work still needs, copied exactly: identifiers, tokens, numbers, short file contents and text to write or report later.\nWork done: only changed files with paths and open failures; one line each, so finished work is not repeated.\nDecisions: each decision and why.\nOpen errors: each error still open.\nPlan: each item with its status.\nNext step: the next action, or the question awaiting the user's answer.\nMerge an earlier summary into these sections rather than copying it: keep what still applies, and shorten finished requests to their one-line entries. Keep the whole summary under about 800 words. Write each item once; never repeat a request, a section or a list. Return only the summary text; do not call tools, continue the conversation, or add commentary.";

#[derive(Clone, Copy)]
pub(in crate::runtime) enum CompactionInstruction {
    Standard,
    Tight,
}

impl CompactionInstruction {
    pub(in crate::runtime) const ALL: [Self; 2] = [Self::Standard, Self::Tight];

    pub(in crate::runtime) const fn text(self) -> &'static str {
        match self {
            Self::Standard => COMPACTION_INSTRUCTION,
            Self::Tight => TIGHT_INSTRUCTION,
        }
    }

    pub(in crate::runtime) fn for_ledger(ledger: &RuntimeOperationLedger) -> Self {
        let overran = latest_harness_fold(ledger)
            .is_some_and(|(_, _, finish)| finish == FinishReason::Length);
        if overran { Self::Tight } else { Self::Standard }
    }
}

/// The newest finalized harness fold carrying a completed response decides
/// the next instruction. Keep this pair through pruning; response-free errors,
/// ordinary rounds and native folds do not change that decision.
pub(in crate::runtime) fn latest_harness_fold(
    ledger: &RuntimeOperationLedger,
) -> Option<(
    &DurableOperationRecord,
    &DurableOperationRecord,
    FinishReason,
)> {
    let operations = ledger.operations();
    for compaction in operations.iter().rev() {
        let DurableOperationKind::Compaction { workflow_id } = compaction.kind() else {
            continue;
        };
        if !matches!(compaction.state(), DurableOperationState::Finalized(_)) {
            continue;
        }
        let Ok(request) =
            serde_json::from_slice::<DurableCompactionRequestRecord>(compaction.request().bytes())
        else {
            continue;
        };
        if request.version != 2 {
            continue;
        }
        let Some(provider) = operations.iter().rev().find(|record| {
            matches!(record.kind(), DurableOperationKind::Provider {
                root: ModelOperationRoot::SessionWorkflow(root), ..
            } if root == workflow_id)
                && matches!(record.state(), DurableOperationState::Finalized(_))
        }) else {
            continue;
        };
        let Some(response) = provider.transitions().iter().rev().find_map(|transition| {
            let DurableOperationState::Completed(evidence) = transition.state() else {
                return None;
            };
            serde_json::from_slice::<ModelResponse>(evidence.bytes()).ok()
        }) else {
            continue;
        };
        return Some((compaction, provider, response.finish().reason()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::compaction::record::compaction_operation_component;
    use crate::{DurableOperationTransition, RuntimeComponentKind, SessionWorkflowId};
    use plakous_model::{
        AttemptId, CanonicalHistory, CheckpointId, Digest, DigestDomain, FinishDetails,
        ModelRequest, RequestMetadata, RunId, Timestamp,
    };
    use uuid::Uuid;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn request(workflow_id: SessionWorkflowId) -> TestResult<DurableCompactionRequestRecord> {
        let checkpoint = crate::checkpoint_bundle::tests::fixture_with_history(
            CanonicalHistory::new(Vec::new())?,
            10,
            false,
        )?;
        let manifest = checkpoint.manifest();
        let topology = manifest.topology();
        let digest = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"instruction-selection"])?;
        Ok(DurableCompactionRequestRecord {
            version: 2,
            workflow_id,
            turn_fence: false,
            operation_id: "instruction-selection".to_owned(),
            source_session: topology.session_id,
            source_branch: topology.branch_id,
            source_checkpoint: checkpoint.id(),
            source_history: manifest.canonical().history().identity(),
            cut: digest,
            request: ModelRequest::from_parts(
                manifest.canonical().stable_prefix().clone(),
                manifest.canonical().projection(),
                RequestMetadata::for_session_workflow(
                    RunId::try_from(Uuid::from_u128(11))?,
                    workflow_id,
                    AttemptId::try_from(Uuid::from_u128(12))?,
                    Timestamp::new(1, 0)?,
                ),
            )?,
            provider_configuration: manifest.provider_configuration().clone(),
            source_harness_plan: digest,
            before_measurement: digest,
            before_status: digest,
            before_input_tokens: 1,
            target_affinity_entropy: [0; 32],
        })
    }

    #[test]
    fn latest_harness_fold_requires_finalized_compaction_record() -> TestResult {
        let workflow = SessionWorkflowId::try_from([1; 32])?;
        let request = request(workflow)?;
        let checkpoint = request.source_checkpoint;
        let dispatched = CheckpointId::try_from(Uuid::from_u128(14))?;
        let completed = CheckpointId::try_from(Uuid::from_u128(15))?;
        let finalization = CheckpointId::try_from(Uuid::from_u128(16))?;
        let response = compaction_operation_component(&ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Length, None),
            None,
            None,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"response"])?,
        ))?;
        assert_eq!(response.kind(), RuntimeComponentKind::Operation);
        let transitions = vec![
            DurableOperationTransition::new(checkpoint, DurableOperationState::Prepared),
            DurableOperationTransition::new(dispatched, DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                completed,
                DurableOperationState::Completed(response.clone()),
            ),
        ];
        let mut finalized = transitions.clone();
        finalized.push(DurableOperationTransition::new(
            finalization,
            DurableOperationState::Finalized(response.clone()),
        ));
        let ledger = RuntimeOperationLedger::new(
            Vec::new(),
            vec![
                DurableOperationRecord::new(
                    1,
                    DurableOperationKind::Compaction {
                        workflow_id: workflow,
                    },
                    compaction_operation_component(&request)?,
                    transitions,
                )?,
                DurableOperationRecord::new(
                    2,
                    DurableOperationKind::Provider {
                        root: ModelOperationRoot::SessionWorkflow(workflow),
                        attempt_id: AttemptId::try_from(Uuid::from_u128(12))?,
                    },
                    compaction_operation_component(&request)?,
                    finalized,
                )?,
            ],
        )?;
        assert!(latest_harness_fold(&ledger).is_none());
        let ledger = ledger.transition_operation(
            1,
            DurableOperationTransition::new(
                finalization,
                DurableOperationState::Finalized(response),
            ),
        )?;
        let (compaction, provider, finish) =
            latest_harness_fold(&ledger).ok_or("finalized pair missing")?;
        assert_eq!(
            (compaction.operation_id(), provider.operation_id(), finish),
            (1, 2, FinishReason::Length)
        );
        Ok(())
    }

    #[test]
    fn tight_instruction_pin() {
        let text = CompactionInstruction::Tight.text();
        assert_eq!(text.len(), 2057);
        assert_eq!(
            plakous_model::sha256_hex(&[text.as_bytes()]),
            "4a2a372567a48ec74ed0c4a272023d179117ada0e8d5e7f0727c813ba38f9b6b"
        );
    }
}
