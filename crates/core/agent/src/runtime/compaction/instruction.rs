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
mod tests;
#[cfg(test)]
pub(in crate::runtime) use tests::retirement_fold_pair;
