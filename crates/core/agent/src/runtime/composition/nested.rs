use crate::runtime::composition::projection::{
    local_composition_nested_call_arguments, local_composition_recovery_authoritative,
    local_composition_started_error,
};
use crate::runtime::composition::{
    CompositionOwner, LocalCompositionNestedDisposition, LocalCompositionNestedReadOnlyCompletion,
    LocalCompositionNestedReady, LocalCompositionNestedReplay, NestedDeliveryReceipt,
    PendingLocalCompositionNested, PendingNestedDeliveries,
};
use crate::{AgentError, AgentErrorKind};
use plakous_model::{Digest, DigestDomain};
use std::collections::{BTreeMap, VecDeque};
fn queue_local_composition_nested_result(
    state: &mut CompositionOwner,
    responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
    result: Result<serde_json::Value, AgentError>,
) -> Result<(), AgentError> {
    queue_local_composition_nested_results(state, responder, Vec::new(), result)
}

pub(in crate::runtime) fn queue_local_composition_nested_results(
    state: &mut CompositionOwner,
    responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
    waiters: Vec<Box<dyn crate::local_composition::LocalCompositionNestedResponder>>,
    result: Result<serde_json::Value, AgentError>,
) -> Result<(), AgentError> {
    let count = waiters.len().saturating_add(1);
    if state.pending_deliveries.len().saturating_add(count)
        > state.integration.max_outstanding_nested_calls()
    {
        return Err(local_composition_started_error(
            "nested delivery acknowledgement limit exceeded",
        ));
    }
    // Canonical uncertainty fences new work immediately. Receipt cleanup is
    // still owned, but cannot defer the already-known recovery boundary.
    let recovery = result
        .as_ref()
        .err()
        .filter(|error| local_composition_recovery_authoritative(error))
        .cloned();
    let duplicates = waiters
        .into_iter()
        .map(|waiter| {
            Box::pin(deliver_local_composition_nested_result(
                waiter,
                result.clone(),
            ))
        })
        .collect::<Vec<_>>();
    state
        .pending_deliveries
        .push(Box::pin(deliver_local_composition_nested_result(
            responder, result,
        )));
    for delivery in duplicates {
        state.pending_deliveries.push(delivery);
    }
    recovery.map_or(Ok(()), Err)
}

/// Delivers one committed child outcome to its owning cell.
///
/// A refusal is the cell's own answer that it no longer owns the request, and a
/// proved non-delivery is the host's answer that the cell never saw it; both are
/// reported as receipts and never as a loss of the composition. Only an
/// acknowledgement of a genuine pending request that can be neither proved nor
/// ruled out fails the turn.
pub(in crate::runtime) async fn deliver_local_composition_nested_result(
    responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
    result: Result<serde_json::Value, AgentError>,
) -> Result<NestedDeliveryReceipt, AgentError> {
    let recovery = result
        .as_ref()
        .err()
        .filter(|error| local_composition_recovery_authoritative(error))
        .cloned();
    let delivery = match result {
        Ok(value) => responder.resolve(value).await,
        Err(error) => responder.reject(error.message().to_owned()).await,
    };
    let receipt = match delivery {
        Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted) => {
            NestedDeliveryReceipt::Accepted
        }
        Ok(crate::local_composition::LocalCompositionNestedDelivery::Refused(_)) => {
            NestedDeliveryReceipt::NotOwned
        }
        // Provable non-delivery is a knowable outcome, not an unknown one. The
        // cell never saw the result, so it cannot have acted on it; the
        // committed child outcome stands and the cell's own answer is
        // authoritative. Only an acceptance that can be neither proved nor
        // ruled out owes exact recovery.
        Ok(
            crate::local_composition::LocalCompositionNestedDelivery::NotObserved(_)
            | crate::local_composition::LocalCompositionNestedDelivery::GenerationLost,
        ) => NestedDeliveryReceipt::NotObserved,
        // The cell is still waiting on a result the host proved it never
        // received, so the composition cannot continue. It ends on an ordinary
        // definite failure the turn reports and the session survives, because
        // nothing was observed and nothing could have been acted on. A
        // recovery-authoritative child outcome is stronger and still wins.
        Ok(crate::local_composition::LocalCompositionNestedDelivery::NotDelivered(reason)) => {
            return Err(recovery.unwrap_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Tool,
                    format!("local composition could not deliver a committed nested result to its cell: {reason}"),
                )
            }));
        }
        Ok(
            crate::local_composition::LocalCompositionNestedDelivery::HostLost
            | crate::local_composition::LocalCompositionNestedDelivery::ProtocolLost,
        )
        | Err(_) => {
            return Err(local_composition_started_error(
                "nested local composition result receiver disappeared or could not prove acceptance",
            ));
        }
    };
    recovery.map_or(Ok(receipt), Err)
}

#[cfg(test)]
pub(in crate::runtime) async fn deliver_local_composition_nested_results(
    responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
    waiters: Vec<Box<dyn crate::local_composition::LocalCompositionNestedResponder>>,
    result: Result<serde_json::Value, AgentError>,
) -> Result<(), AgentError> {
    let duplicates = waiters
        .into_iter()
        .map(|waiter| deliver_local_composition_nested_result(waiter, result.clone()))
        .collect::<Vec<_>>();
    let deliveries = std::iter::once(deliver_local_composition_nested_result(responder, result))
        .chain(duplicates);
    let mut failure = None;
    for result in futures_util::future::join_all(deliveries).await {
        if let Err(error) = result {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

pub(in crate::runtime) fn take_local_composition_nested_waiters(
    state: &mut CompositionOwner,
    runtime_call_id: &str,
) -> Vec<Box<dyn crate::local_composition::LocalCompositionNestedResponder>> {
    state
        .nested_replays
        .get_mut(runtime_call_id)
        .map_or_else(Vec::new, |replay| std::mem::take(&mut replay.waiters))
}

pub(in crate::runtime) fn fence_pending_local_composition_nested(
    state: &mut CompositionOwner,
    reason: &str,
) {
    fence_pending_local_composition_nested_queue(
        &mut state.pending_nested,
        &mut state.nested_replays,
        &mut state.pending_deliveries,
        reason,
    );
}

pub(in crate::runtime) fn fence_pending_local_composition_nested_queue(
    pending_nested: &mut VecDeque<PendingLocalCompositionNested>,
    nested_replays: &mut BTreeMap<String, LocalCompositionNestedReplay>,
    pending_deliveries: &mut PendingNestedDeliveries,
    reason: &str,
) {
    // These are already bounded admitted requests. Fence all of them before
    // shutdown; never wait for one batch member to acknowledge the others.
    while let Some(pending) = pending_nested.pop_front() {
        if let Some(replay) = nested_replays.get_mut(&pending.call.runtime_call_id) {
            replay.disposition = LocalCompositionNestedDisposition::Rejected(reason.to_owned());
        }
        let waiters = nested_replays
            .get_mut(&pending.call.runtime_call_id)
            .map_or_else(Vec::new, |replay| std::mem::take(&mut replay.waiters));
        for responder in std::iter::once(pending.responder).chain(waiters) {
            pending_deliveries.push(Box::pin(deliver_local_composition_nested_result(
                responder,
                Err(AgentError::new(AgentErrorKind::InvalidInput, reason)),
            )));
        }
    }
}

pub(in crate::runtime) fn take_next_local_composition_nested_ready(
    completed: &mut BTreeMap<u32, LocalCompositionNestedReady>,
    next_sequence: &mut u32,
) -> Result<Option<LocalCompositionNestedReady>, AgentError> {
    let Some(ready) = completed.remove(next_sequence) else {
        return Ok(None);
    };
    *next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
        local_composition_started_error("nested commit sequence space was exhausted")
    })?;
    Ok(Some(ready))
}

pub(in crate::runtime) fn local_composition_nested_replay(
    replay: Option<&LocalCompositionNestedReplay>,
    request_digest: Digest,
) -> Result<Option<serde_json::Value>, AgentError> {
    let Some(replay) = replay else {
        return Ok(None);
    };
    if replay.request_digest != request_digest {
        return Err(local_composition_started_error(format!(
            "nested local composition call identity conflicted with sequence {}",
            replay.request_sequence
        )));
    }
    match &replay.disposition {
        LocalCompositionNestedDisposition::Resolved(value) => Ok(Some(value.clone())),
        LocalCompositionNestedDisposition::Rejected(message) => Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            message.clone(),
        )),
        LocalCompositionNestedDisposition::Pending
        | LocalCompositionNestedDisposition::RecoveryRequired => {
            Err(local_composition_started_error(
                "duplicate nested local composition call has no definite committed disposition",
            ))
        }
    }
}

pub(in crate::runtime) fn attach_local_composition_nested_waiter(
    replay: &mut LocalCompositionNestedReplay,
    request_digest: Digest,
    responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
) -> Result<Option<Box<dyn crate::local_composition::LocalCompositionNestedResponder>>, AgentError>
{
    if replay.request_digest != request_digest {
        return Err(local_composition_started_error(format!(
            "nested local composition call identity conflicted with sequence {}",
            replay.request_sequence
        )));
    }
    if matches!(
        replay.disposition,
        LocalCompositionNestedDisposition::Pending
    ) {
        replay.waiters.push(responder);
        Ok(None)
    } else {
        Ok(Some(responder))
    }
}

pub(in crate::runtime) fn local_composition_nested_request_digest(
    nested: &crate::local_composition::LocalCompositionNestedCall,
) -> Result<Digest, AgentError> {
    let canonical_request = serde_json::to_vec(&(
        "plakous-local-composition-child-request-v1",
        nested.cell_id.as_str(),
        nested.runtime_call_id.as_str(),
        &nested.tool_name,
        &nested.input,
    ))
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "nested local composition request could not be canonicalized",
        )
    })?;
    Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/local-composition-child-request-v1",
            &canonical_request,
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "nested local composition request identity could not be constructed",
        )
    })
}

pub(in crate::runtime) fn reserve_local_composition_nested_slot(
    outstanding: usize,
    profile_limit: usize,
    prior_sequence: u32,
) -> Result<u32, AgentError> {
    if profile_limit == 0 || profile_limit > 16 {
        return Err(local_composition_started_error(
            "local composition profile has an invalid outstanding nested-call limit",
        ));
    }
    if outstanding >= profile_limit {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            format!(
                "local composition exceeds its profile-bound limit of {profile_limit} outstanding nested calls"
            ),
        ));
    }
    let sequence = prior_sequence.checked_add(1).ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition nested-call bound was exhausted",
        )
    })?;
    if sequence > 100 {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition exceeded its profile-bound total of 100 nested calls",
        ));
    }
    Ok(sequence)
}

impl CompositionOwner {
    pub(in super::super) fn admit_nested(
        &mut self,
        call: crate::local_composition::LocalCompositionNestedCall,
        responder: Box<dyn crate::local_composition::LocalCompositionNestedResponder>,
    ) -> Result<(), AgentError> {
        if self.cell.as_ref() != Some(&call.cell_id) {
            return queue_local_composition_nested_result(
                self,
                responder,
                Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "nested local composition call belongs to a foreign cell",
                )),
            );
        }
        let request_digest = local_composition_nested_request_digest(&call)?;
        let limit = self.integration.max_outstanding_nested_calls();
        let outstanding = self
            .pending_nested
            .len()
            .saturating_add(self.active_nested)
            .saturating_add(self.completed_nested.len())
            .saturating_add(self.pending_deliveries.len())
            .saturating_add(
                self.nested_replays
                    .values()
                    .map(|replay| replay.waiters.len())
                    .sum::<usize>(),
            );
        if let Some(replay) = self.nested_replays.get_mut(&call.runtime_call_id) {
            if matches!(
                replay.disposition,
                LocalCompositionNestedDisposition::Pending
            ) && outstanding >= limit
            {
                return queue_local_composition_nested_result(
                    self,
                    responder,
                    Err(AgentError::new(
                        AgentErrorKind::InvalidInput,
                        format!(
                            "local composition exceeds its profile-bound limit of {limit} outstanding nested calls"
                        ),
                    )),
                );
            }
            let Some(responder) =
                attach_local_composition_nested_waiter(replay, request_digest, responder)?
            else {
                return Ok(());
            };
            let result =
                local_composition_nested_replay(Some(replay), request_digest)?.ok_or_else(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "nested local composition replay lost its disposition",
                    )
                });
            return queue_local_composition_nested_result(self, responder, result);
        }
        // Reject malformed custom source before assigning a child sequence.
        // The durable ledger requires every assigned sequence to have an edge;
        // a definite pre-admission refusal has no child operation to record.
        let Ok(name) = plakous_model::ToolName::new(call.tool_name.clone()) else {
            return queue_local_composition_nested_result(
                self,
                responder,
                Err(AgentError::new(
                    AgentErrorKind::InvalidInput,
                    "nested tool name is invalid",
                )),
            );
        };
        if let Err(error) = local_composition_nested_call_arguments(
            &self.descriptor.nested_projection,
            &name,
            call.input.as_ref(),
        ) {
            return queue_local_composition_nested_result(self, responder, Err(error));
        }
        let request_sequence =
            match reserve_local_composition_nested_slot(outstanding, limit, self.nested_sequence) {
                Ok(sequence) => sequence,
                Err(error) => {
                    return queue_local_composition_nested_result(self, responder, Err(error));
                }
            };
        self.nested_sequence = request_sequence;
        self.nested_replays.insert(
            call.runtime_call_id.clone(),
            LocalCompositionNestedReplay {
                request_digest,
                request_sequence,
                disposition: LocalCompositionNestedDisposition::Pending,
                waiters: Vec::new(),
            },
        );
        self.pending_nested
            .push_back(PendingLocalCompositionNested {
                call,
                responder,
                request_digest,
                request_sequence,
            });
        Ok(())
    }

    pub(in super::super) fn buffer_read_only(
        &mut self,
        completion: LocalCompositionNestedReadOnlyCompletion,
    ) -> Result<(), AgentError> {
        self.active_nested = self.active_nested.checked_sub(1).ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Internal,
                "nested scheduler active count underflowed",
            )
        })?;
        let sequence = completion.pending.request_sequence;
        if self
            .completed_nested
            .insert(
                sequence,
                LocalCompositionNestedReady::Execution(Box::new(completion)),
            )
            .is_some()
        {
            return Err(local_composition_started_error(
                "nested scheduler completed one acceptance sequence twice",
            ));
        }
        Ok(())
    }
    pub(in super::super) fn buffer_rejection(
        &mut self,
        pending: PendingLocalCompositionNested,
        error: AgentError,
    ) -> Result<(), AgentError> {
        let sequence = pending.request_sequence;
        if self
            .completed_nested
            .insert(
                sequence,
                LocalCompositionNestedReady::Rejected(Box::new((pending, error))),
            )
            .is_some()
        {
            return Err(local_composition_started_error(
                "nested scheduler completed one acceptance sequence twice",
            ));
        }
        Ok(())
    }
    pub(in super::super) fn take_serial_pending(
        &mut self,
    ) -> Result<Option<PendingLocalCompositionNested>, AgentError> {
        let Some(pending) = self.pending_nested.pop_front() else {
            return Ok(None);
        };
        if pending.request_sequence != self.next_nested_commit_sequence
            || self.active_nested != 0
            || !self.completed_nested.is_empty()
        {
            return Err(local_composition_started_error(
                "serial nested dispatch crossed an earlier acceptance sequence",
            ));
        }
        Ok(Some(pending))
    }
    pub(in super::super) fn finish_serial(
        &mut self,
        runtime_call_id: &str,
        result: &Result<serde_json::Value, AgentError>,
    ) -> Result<(), AgentError> {
        if let Some(replay) = self.nested_replays.get_mut(runtime_call_id)
            && matches!(
                replay.disposition,
                LocalCompositionNestedDisposition::Pending
            )
        {
            replay.disposition = match result {
                Ok(value) => LocalCompositionNestedDisposition::Resolved(value.clone()),
                Err(error) if local_composition_recovery_authoritative(error) => {
                    LocalCompositionNestedDisposition::RecoveryRequired
                }
                Err(error) => {
                    LocalCompositionNestedDisposition::Rejected(error.message().to_owned())
                }
            };
        }
        self.next_nested_commit_sequence = self
            .next_nested_commit_sequence
            .checked_add(1)
            .ok_or_else(|| {
                local_composition_started_error("nested commit sequence space was exhausted")
            })?;
        Ok(())
    }

    pub(in super::super) fn nested_sequence(&self) -> u32 {
        self.nested_sequence
    }
    pub(in super::super) fn pending_front(&self) -> Option<&PendingLocalCompositionNested> {
        self.pending_nested.front()
    }
    pub(in super::super) fn take_pending(&mut self) -> Option<PendingLocalCompositionNested> {
        self.pending_nested.pop_front()
    }
    pub(in super::super) fn pending_is_empty(&self) -> bool {
        self.pending_nested.is_empty()
    }
    pub(in super::super) fn completed_is_empty(&self) -> bool {
        self.completed_nested.is_empty()
    }
    pub(in super::super) fn has_active_nested(&self) -> bool {
        self.active_nested != 0
    }
    pub(in super::super) fn execution_started(&mut self) {
        self.active_nested = self.active_nested.saturating_add(1);
    }
    pub(in super::super) fn deliveries_are_empty(&self) -> bool {
        self.pending_deliveries.is_empty()
    }
    pub(in super::super) async fn next_delivery(
        &mut self,
    ) -> Option<Result<NestedDeliveryReceipt, AgentError>> {
        self.pending_deliveries.next().await
    }

    pub(in super::super) async fn settle_deliveries(&mut self) -> Vec<AgentError> {
        super::super::settle_local_composition_deliveries(&mut self.pending_deliveries).await
    }
    pub(in super::super) fn reset_nested_for_cell(&mut self) {
        self.nested_sequence = 0;
        self.nested_replays.clear();
        self.pending_nested.clear();
        self.completed_nested.clear();
        self.next_nested_commit_sequence = 1;
    }
    pub(in super::super) fn reset_nested_after_cleanup(&mut self) {
        self.nested_sequence = 0;
        self.next_nested_commit_sequence = 1;
        self.nested_replays.clear();
        self.completed_nested.clear();
    }
    pub(in super::super) fn take_next_ready(
        &mut self,
    ) -> Result<Option<LocalCompositionNestedReady>, AgentError> {
        take_next_local_composition_nested_ready(
            &mut self.completed_nested,
            &mut self.next_nested_commit_sequence,
        )
    }
    pub(in super::super) fn record_nested_disposition(
        &mut self,
        runtime_call_id: &str,
        disposition: LocalCompositionNestedDisposition,
    ) {
        if let Some(replay) = self.nested_replays.get_mut(runtime_call_id) {
            replay.disposition = disposition;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::{NestedDeliveryReceipt, deliver_local_composition_nested_result};
    use crate::AgentErrorKind;
    use crate::local_composition::{
        LocalCompositionFuture, LocalCompositionNestedDelivery, LocalCompositionNestedResponder,
    };

    struct FixedDelivery(LocalCompositionNestedDelivery);

    impl LocalCompositionNestedResponder for FixedDelivery {
        fn resolve(
            self: Box<Self>,
            _value: serde_json::Value,
        ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
            Box::pin(async move { Ok(self.0) })
        }

        fn reject(
            self: Box<Self>,
            _reason: String,
        ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
            Box::pin(async move { Ok(self.0) })
        }
    }

    #[tokio::test]
    async fn refused_acknowledgement_is_a_not_owned_receipt() {
        let receipt = deliver_local_composition_nested_result(
            Box::new(FixedDelivery(LocalCompositionNestedDelivery::Refused(
                "unknown tool call 7".to_owned(),
            ))),
            Ok(serde_json::json!("committed")),
        )
        .await
        .expect("a refusal must not fail the composition");
        assert!(
            matches!(receipt, NestedDeliveryReceipt::NotOwned),
            "a refused delivery must read as a request the cell no longer owns"
        );
    }

    #[tokio::test]
    async fn proved_non_delivery_is_a_receipt_and_never_requires_recovery() {
        for delivery in [
            LocalCompositionNestedDelivery::NotObserved("the cell had already ended".to_owned()),
            LocalCompositionNestedDelivery::GenerationLost,
        ] {
            let receipt = deliver_local_composition_nested_result(
                Box::new(FixedDelivery(delivery.clone())),
                Ok(serde_json::json!("committed")),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("{delivery:?} is a knowable outcome, not a recovery boundary: {error:?}")
            });
            assert!(
                matches!(receipt, NestedDeliveryReceipt::NotObserved),
                "a result the cell provably never saw must read as a non-observation"
            );
        }
    }

    #[tokio::test]
    async fn lost_acknowledgement_still_fails_the_started_cell() {
        let error = deliver_local_composition_nested_result(
            Box::new(FixedDelivery(LocalCompositionNestedDelivery::HostLost)),
            Ok(serde_json::json!("committed")),
        )
        .await
        .expect_err("a lost acknowledgement must require recovery");
        assert_eq!(error.kind(), AgentErrorKind::OutcomeUnknown);
    }
}
