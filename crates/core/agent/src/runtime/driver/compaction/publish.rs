//! Publishing a committed compaction successor.

use crate::runtime::OperationId;
use crate::runtime::compaction::continuation::{
    with_post_compaction_note, with_turn_continuation_note,
};
use crate::runtime::compaction::fold::build_compaction_success_fold;
use crate::runtime::compaction::{
    CompactOrigin, CompactionCompletionError, DurableCompactionIdentity,
    DurableCompactionRequestRecord,
};
use crate::runtime::driver::Driver;
use crate::runtime::operation_record::{compaction_success_lifecycle, working_lifecycle};
use crate::{CheckpointId, StoreAdmission};
use plakous_model::{
    Clock, CompiledProviderConfiguration, LanguageModel, ModelResponse, PreparedCallRecord, TurnId,
};
use std::sync::Arc;
use std::time::Duration;

impl Driver {
    #[allow(
        clippy::too_many_arguments,
        reason = "the exact compaction publication fence carries its complete persisted tuple"
    )]
    pub(super) async fn publish_compaction_success(
        &mut self,
        identity: DurableCompactionIdentity,
        compaction_operation: OperationId,
        provider_operation: OperationId,
        record: &DurableCompactionRequestRecord,
        prepared: &PreparedCallRecord,
        response: &ModelResponse,
        model: Arc<dyn LanguageModel>,
        current_configuration: &CompiledProviderConfiguration,
        clock: &dyn Clock,
        authority_timeout: Duration,
        _origin: CompactOrigin,
        turn_id: Option<TurnId>,
    ) -> Result<(CheckpointId, Option<u64>), CompactionCompletionError> {
        let checkpoint = self.authority.checkpoint();
        let source_history = checkpoint.manifest().canonical().history().clone();
        let mut fold = build_compaction_success_fold(
            checkpoint.manifest(),
            identity,
            compaction_operation,
            provider_operation,
            record,
            prepared,
            response,
            model,
            current_configuration,
            clock,
            authority_timeout,
        )
        .await?;
        fold.history = with_post_compaction_note(
            fold.history,
            &source_history,
            self.post_compaction_note.as_ref(),
            || self.entry_id(),
        )
        .map_err(CompactionCompletionError::semantic)?;
        if turn_id.is_some() {
            fold.history = with_turn_continuation_note(fold.history, identity.workflow_id)
                .map_err(CompactionCompletionError::semantic)?;
        }
        // A turn-fence fold is still inside its admitted root even when every
        // operation visible in the folded ledger is already finalized.  Do
        // not infer quiescence from that ledger: the post-fold steering note
        // and resent round still belong to this turn.
        let lifecycle = match turn_id {
            Some(turn_id) => working_lifecycle(turn_id, &fold.ledger),
            None => compaction_success_lifecycle(&fold.ledger),
        }
        .map_err(CompactionCompletionError::semantic)?;
        // Semantic and native successors replace the measured request prefix.
        self.context_anchor = None;
        self.request_anchor = None;
        self.publish_checkpoint(
            StoreAdmission::Settlement,
            identity.success_checkpoint,
            lifecycle,
            fold.history,
            fold.ledger,
            fold.context,
            None,
        )
        .await
        .map_err(CompactionCompletionError::recovery)?;
        // Capture the actual published successor, including continuation and
        // post-compaction notes rather than just the semantic fold's target.
        self.last_compaction_successor = Some(
            self.authority
                .checkpoint()
                .manifest()
                .canonical()
                .history()
                .identity(),
        );
        Ok((identity.success_checkpoint, fold.occupancy_tokens_after))
    }
}
