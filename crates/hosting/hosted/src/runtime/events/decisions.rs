//! Effect decisions and approval intents.

use super::{EventApplier, timestamp_ms};
use crate::runtime::logging::log_event;
use crate::runtime::turn::{flush_assistant_delta, take_pending_approval};
use crate::runtime::{APPROVAL_REFUSED_NOTE, PendingApproval, Sender, TurnProjection};
use crate::session_log::{SessionEvent, redact_text_preview};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous::agent::{AgentEventKind, EffectDecisionResolution, TurnControl};

/// Maps one typed approval intent back onto the runtime decision handle.
///
/// Decision authority stays in Plakous. Surfaces only
/// ever name a host-minted `approval_id`; the `EffectDecisionRef` and the
/// audit id are held here, and an unknown or already-submitted handle is
/// refused without contacting the runtime at all.
/// Returns true only after the runtime accepted the durable decision.
pub(in crate::runtime) async fn resolve_approval_intent<
    P: plakous_host_protocol::ProtocolExtension,
>(
    control: &TurnControl,
    events: &Sender<P>,
    projection: &mut TurnProjection<P>,
    approval_id: &str,
    choice: plakous_host_protocol::ApprovalChoice,
) -> bool {
    let Some(pending) = take_pending_approval(projection, approval_id) else {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Approval,
            APPROVAL_REFUSED_NOTE,
        ));
        return false;
    };
    // The decision→handle map survives the intent so the runtime's own
    // resolution event still retires the row truthfully.
    let outcome = match choice {
        plakous_host_protocol::ApprovalChoice::Allow => {
            let Ok(audit) = plakous::tools::EffectDecisionAuditId::new(approval_id.to_owned())
            else {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Approval,
                    "approval refused: the host decision identity was not portable",
                ));
                return false;
            };
            control.allow_effect(pending.decision.clone(), audit).await
        }
        plakous_host_protocol::ApprovalChoice::Deny => {
            control
                .deny_tool(
                    pending.decision.clone(),
                    plakous::tools::ToolPolicyDenial::new("operator denied this tool"),
                )
                .await
        }
    };
    if let Err(error) = outcome {
        if matches!(
            error.kind(),
            plakous::agent::ControlErrorKind::Backpressure
                | plakous::agent::ControlErrorKind::LedgerFull
                | plakous::agent::ControlErrorKind::StoragePressure
        ) {
            // These definite refusals leave the runtime decision pending. Do
            // not strand the offered handle; success still consumes it once.
            projection.approvals.insert(approval_id.to_owned(), pending);
        }
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Approval,
            format!("approval was not applied: {error}"),
        ));
        return false;
    }
    true
}

#[cfg(test)]
mod tests;

impl<P: plakous_host_protocol::ProtocolExtension> EventApplier<'_, P> {
    /// An effect that waits for the operator's decision.
    pub(super) fn effect_decision_required(self) {
        let Self {
            event,
            call_id_str,
            events,
            delta_buf,
            last_flush,
            tool_subjects,
            projection,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::EffectDecisionRequired {
            identity,
            decision,
            prompt,
            expires_at,
        } = event.kind()
        else {
            return;
        };
        flush_assistant_delta(delta_buf, last_flush, events, true);
        // this is the *only* constructor of an approval row.
        // The decision address stays host-side; the TUI receives an opaque,
        // session-scoped handle it can only echo back.
        let approval_id = format!("approval-{}", uuid::Uuid::new_v4());
        projection.approvals.insert(
            approval_id.clone(),
            PendingApproval {
                decision: decision.clone(),
            },
        );
        projection
            .approval_by_decision
            .insert(decision.clone(), approval_id.clone());
        let tool_name = identity.canonical_name().as_str().to_owned();
        let info = tool_subjects.peek(&tool_name, call_id_str.as_deref());
        log_event(
            log,
            observed_session_id,
            SessionEvent::new("effect.decision.required")
                .str("name", &tool_name)
                .opt_str("call_id", call_id_str.as_deref()),
        );
        let _ = events.send(ContentEvent::ApprovalRequired {
            approval_id,
            call_id: call_id_str,
            tool_name,
            subject: info.map(|item| redact_text_preview(&item.subject)),
            prompt: prompt
                .as_ref()
                .map(|prompt| redact_text_preview(prompt.description().as_str())),
            expires_at_ms: timestamp_ms(*expires_at),
        });
    }

    /// The operator's or the policy's decision on an effect.
    pub(super) fn effect_decision_resolved(self) {
        let Self {
            event,
            events,
            projection,
            log,
            observed_session_id,
            ..
        } = self;
        let AgentEventKind::EffectDecisionResolved {
            decision,
            resolution,
            ..
        } = event.kind()
        else {
            return;
        };
        let Some(approval_id) = projection.approval_by_decision.remove(decision) else {
            return;
        };
        projection.approvals.remove(&approval_id);
        let resolution = match resolution {
            EffectDecisionResolution::Allowed => "allowed",
            EffectDecisionResolution::Denied => "denied",
            EffectDecisionResolution::Expired => "expired",
            EffectDecisionResolution::Cancelled => "cancelled",
            EffectDecisionResolution::Shutdown => "shutdown",
            EffectDecisionResolution::Invalidated => "invalidated",
            // The runtime enum is `#[non_exhaustive]`. A resolution this
            // host does not know is reported as unknown rather than
            // mislabelled as one of the six it does.
            _ => "unknown",
        };
        log_event(
            log,
            observed_session_id,
            SessionEvent::new("effect.decision.resolved").str("resolution", resolution),
        );
        let _ = events.send(ContentEvent::ApprovalResolved {
            approval_id,
            resolution: resolution.to_owned(),
        });
    }
}
