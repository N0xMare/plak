//! The abandonment report a live cancel records when its turn carries an
//! abandonment cause.
//!
//! There is one claim law: the report is the recovery closure's own, read
//! from the same fold ([`super::abandonment_fold`]) over the live head when
//! the cancel is observed, with the restored hosted baton the cancel's
//! terminal finalizes treated as that closure treats a baton it closes. The
//! note is the closure's note for the same fold. The cancel does not close
//! the operations the fold names; the live turn settles them as any cancel
//! does, and the report says what the turn had open.

use super::{
    DecisionBarrier, RootClosure, abandoned_turn_claim, abandoned_turn_note, abandonment_fold,
    abandonment_rows, closure_facts,
};
use crate::runtime::{
    AbandonedTurnClaim, AbandonedTurnReport, AbandonmentCause, OperationId, TurnShared,
};
use crate::{AgentError, AgentErrorKind, RuntimeCheckpoint, RuntimeLifecycle, RuntimeWorkingRoot};
use plakous_model::{ContentBlock, TurnId};

/// The report and the history note for a cancel of `turn_id` abandoned with
/// `cause`. `closing` is the restored hosted baton the terminal finalizes.
///
/// # Errors
///
/// A head the recovery fold cannot classify (an obligation no closure row
/// admits) or a record that does not decode is typed session integrity.
pub(in crate::runtime) fn live_cancel_report(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
    cause: AbandonmentCause,
    closing: Option<OperationId>,
) -> Result<(AbandonedTurnReport, String), AgentError> {
    let active = matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id: root, .. },
            ..
        } if *root == turn_id
    );
    if !active {
        // A queued root never started: its cancel closes only its own
        // prepared command, and its prompt is not in history.
        let claim = AbandonedTurnClaim::NothingStarted;
        return Ok((
            AbandonedTurnReport::new(Vec::new(), Vec::new(), Vec::new(), claim, cause),
            abandoned_turn_note(claim, cause).to_owned(),
        ));
    }
    let mut fold = abandonment_fold(checkpoint, DecisionBarrier::Close(cause), closing)?;
    fold.barrier_cause = Some(cause);
    let claim = abandoned_turn_claim(&fold);
    let segment =
        abandonment_rows::abandonment_segment(&closure_facts(checkpoint, &fold, claim, cause)?)?;
    let note = match segment
        .items()
        .first()
        .map(plakous_model::ConversationItem::blocks)
    {
        Some([ContentBlock::Text(text)]) => text.text().to_owned(),
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "the abandonment note is not one text block",
            ));
        }
    };
    let report = crate::runtime::checkpoint::owner::root_closure_report(
        checkpoint,
        &RootClosure::Abandon { fold, claim },
    )?
    .ok_or_else(|| AgentError::new(AgentErrorKind::Session, "the abandonment has no report"))?;
    Ok((report, note))
}

/// Records the abandonment report once, when the cancel that carries the
/// cause is observed on a turn still running. A plain cancel has no cause
/// and records nothing. A report that cannot be built does not stop the
/// cancel: the failure is kept for the caller, and the terminal is then a
/// plain cancel's.
pub(in crate::runtime) fn observe_live_abandonment(
    checkpoint: &RuntimeCheckpoint,
    shared: &TurnShared,
    command: &crate::CommandId,
    turn_id: TurnId,
    closing: Option<OperationId>,
) {
    let Some(cause) = shared.abandonment_cause(command) else {
        return;
    };
    match live_cancel_report(checkpoint, turn_id, cause, closing) {
        Ok((report, note)) => shared.record_abandonment(report, note),
        Err(error) => shared.fail_abandonment(&error),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit test assertions, matching the sibling durable tests"
)]
mod tests {
    use super::*;

    fn note_text(tail: crate::runtime::driver::TerminalPromptTail) -> String {
        let crate::runtime::driver::TerminalPromptTail::Note(item) = tail else {
            panic!("expected a prompt note");
        };
        item.blocks()
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.text().to_owned(),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn the_cancelled_note_names_the_abandonment_only_when_one_is_set() {
        let turn_id = TurnId::try_from(uuid::Uuid::from_u128(7)).expect("turn");
        let error = AgentError::new(AgentErrorKind::Cancelled, "turn was cancelled");
        let plain = crate::runtime::driver::failure_prompt_tail(turn_id, &error, false, None, None)
            .expect("plain note");
        assert!(
            note_text(plain).contains("the user cancelled the request above"),
            "a plain cancel keeps the operator note"
        );
        let abandoned = crate::runtime::driver::failure_prompt_tail(
            turn_id,
            &error,
            false,
            None,
            Some("this turn was abandoned after its tool round completed."),
        )
        .expect("abandonment note");
        let text = note_text(abandoned);
        assert!(!text.contains("the user cancelled"));
        assert_eq!(
            text,
            "this turn was abandoned after its tool round completed."
        );
    }

    #[test]
    fn the_durable_cancel_record_carries_the_report_and_a_plain_cancel_stays_null() {
        use crate::runtime::terminal::{
            DurableAgentError, DurableTerminalOutcome, PendingTerminalResultRecord,
            TERMINAL_RESULT_RECORD_VERSION, decode_terminal_result_record,
            encode_pending_terminal_record, finalized_operation_evidence,
            finalized_operation_evidence_with,
        };
        use crate::{CheckpointId, RuntimeComponentKind, TurnUsage};
        use plakous_model::{Digest, DigestDomain};

        let turn_id = TurnId::try_from(uuid::Uuid::from_u128(8)).expect("turn");
        let checkpoint_id = CheckpointId::try_from(uuid::Uuid::from_u128(9)).expect("checkpoint");
        let conversation =
            Digest::sha256(DigestDomain::CanonicalHistory, 1, &[b"cancel-record"]).expect("digest");
        let cancelled = PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: "cancel:1".to_owned(),
            turn_id,
            operation_id: OperationId(3),
            outcome: DurableTerminalOutcome::Cancelled(
                DurableAgentError::from_error(&AgentError::new(
                    AgentErrorKind::Cancelled,
                    "turn was cancelled",
                ))
                .expect("error"),
            ),
            usage: TurnUsage::default(),
        };
        let evidence = crate::runtime::continuation::runtime_component(
            RuntimeComponentKind::Operation,
            encode_pending_terminal_record(cancelled).expect("pending"),
        )
        .expect("component");
        let plain =
            finalized_operation_evidence(&evidence, checkpoint_id, conversation).expect("plain");
        let plain_record = decode_terminal_result_record(plain.bytes()).expect("plain record");
        assert!(plain_record.abandonment.is_none());
        assert!(
            String::from_utf8_lossy(plain.bytes()).contains("\"abandonment\":null"),
            "a plain cancel must keep the null field"
        );
        let report = AbandonedTurnReport::new(
            vec![3],
            Vec::new(),
            Vec::new(),
            AbandonedTurnClaim::NothingStarted,
            AbandonmentCause::ProcessLost,
        );
        let with = finalized_operation_evidence_with(
            &evidence,
            checkpoint_id,
            conversation,
            Some(report.clone()),
        )
        .expect("with");
        let with_record = decode_terminal_result_record(with.bytes()).expect("with record");
        assert_eq!(with_record.abandonment.as_ref(), Some(&report));
        let failed = PendingTerminalResultRecord {
            remedy: None,
            version: TERMINAL_RESULT_RECORD_VERSION,
            command_id: "fail:1".to_owned(),
            turn_id,
            operation_id: OperationId(4),
            outcome: DurableTerminalOutcome::Failed(
                DurableAgentError::from_error(&AgentError::new(AgentErrorKind::Internal, "failed"))
                    .expect("error"),
            ),
            usage: TurnUsage::default(),
        };
        let failed_evidence = crate::runtime::continuation::runtime_component(
            RuntimeComponentKind::Operation,
            encode_pending_terminal_record(failed).expect("pending"),
        )
        .expect("component");
        let ignored = finalized_operation_evidence_with(
            &failed_evidence,
            checkpoint_id,
            conversation,
            Some(report),
        )
        .expect("failed");
        let ignored_record = decode_terminal_result_record(ignored.bytes()).expect("failed record");
        assert!(
            ignored_record.abandonment.is_none(),
            "a non-cancel terminal must not take the report"
        );
    }
}
