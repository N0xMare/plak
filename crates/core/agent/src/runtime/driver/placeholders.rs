//! The placeholder rung: offline, deterministic, unpaid compaction.
//!
//! Below the provider-native facet and below the semantic fold sits this
//! third strategy. When the next model round would overflow the admitted
//! ceiling, or the 85 % fence is crossed between rounds inside a turn, and
//! neither the native facet nor the fold can act because the whole session is
//! one protected intent group, the driver replaces the bodies of the oldest
//! completed tool results with bounded placeholders until the projection fits.
//!
//! No model call is made, so the operation is durably recorded as a single
//! atomic `Finalized` transition in one lease-fenced CAS: there is never an
//! ambiguous paid outcome to recover, and no checkpoint is named that the
//! branch does not publish. The successor carries the same provenance,
//! measurements as a summarised compaction, preserves cache affinity, and the
//! exact source checkpoint remains immutable and loadable.
//!
//! The pass is repeatable. A canonical history admits exactly one compaction
//! segment, so the notice merges into the segment the source already has
//! rather than adding one beside it; see `compaction::placeholder`.

use crate::runtime::PromptCommand;
use crate::runtime::compaction::placeholder::{
    PlaceholderCandidate, PlaceholderSuccessor, collect_placeholder_candidates,
    placeholder_successor, portable_placeholder_successor, replay_omission_successor,
};
use crate::runtime::compaction::record::compaction_attempt_id;
use crate::runtime::compaction::request::{
    compact_runtime_endpoint_error, compaction_fence_base_tokens, occupancy_gauge_percent,
    portable_compaction_history,
};
use crate::runtime::compaction::{
    DurableCompactionIdentity, DurablePlaceholderEvidence, DurablePlaceholderRequestRecord,
    ExactCompactionRuntime, PLACEHOLDER_LANDING_PERCENT,
};
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::driver::Driver;
use crate::runtime::driver::compaction::CompactionFitBasis;
use crate::runtime::durable::DurableSessionAddress;
use crate::runtime::observation::committed_compaction_summary;
use crate::{
    AgentCompactSessionOutcome, AgentError, AgentErrorKind, AgentEventKind,
    AgentSessionEndpointError, ContextEvaluationAudit, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableOperationTransition, EventContext,
    RuntimeComponentKind, SemanticCompactionPublication, SessionWorkflowId, StoreAdmission,
};
use plakous_model::{
    CanonicalHistory, ContextStatus, ContextStatusKind, ConversationView, Digest, DigestDomain,
    ModelError, ModelErrorKind, ModelErrorPhase, ModelRequest, RequestMetadata, WorkingProjection,
};

/// One accepted placeholders depth with the measurement that justified it.
struct AcceptedPlaceholders {
    count: usize,
    after: ContextEvaluationAudit,
    occupancy_after: Option<u64>,
    wire_digest: Option<Digest>,
    clears_fence: bool,
}

#[derive(Clone, Copy)]
enum SourceAudit {
    Fresh {
        audit: ContextEvaluationAudit,
        occupancy: Option<u64>,
    },
    UnboundRescue {
        audit: ContextEvaluationAudit,
        occupancy: Option<u64>,
    },
}

impl SourceAudit {
    fn measurements(self) -> (ContextEvaluationAudit, Option<u64>) {
        match self {
            Self::Fresh { audit, occupancy } | Self::UnboundRescue { audit, occupancy } => {
                (audit, occupancy)
            }
        }
    }

    fn accepts(self, candidate: &AcceptedPlaceholders) -> bool {
        match self {
            Self::Fresh { audit, .. } => candidate.after.input_tokens() < audit.input_tokens(),
            // No comparable source wire exists. Only successful fresh
            // preparation below the landing fence can rescue this history.
            Self::UnboundRescue { .. } => candidate.clears_fence && candidate.wire_digest.is_some(),
        }
    }
}

/// One offline measurement of a candidate depth.
#[derive(Clone, Copy)]
struct PlaceholdersProbe {
    fits: bool,
    clears_fence: bool,
    after: Option<ContextEvaluationAudit>,
    occupancy: Option<u64>,
    wire_digest: Option<Digest>,
}

struct PlaceholderPreference {
    before_input_tokens: u64,
    smallest: Option<(usize, u64)>,
}

impl PlaceholderPreference {
    fn new(before_input_tokens: u64) -> Self {
        Self {
            before_input_tokens,
            smallest: None,
        }
    }

    /// Returns true exactly when this is the shallowest qualifying depth.
    fn observe(
        &mut self,
        count: usize,
        fits: bool,
        clears_fence: bool,
        after_input_tokens: u64,
    ) -> bool {
        if !fits || after_input_tokens >= self.before_input_tokens {
            return false;
        }
        if clears_fence {
            return true;
        }
        if self
            .smallest
            .is_none_or(|(_, current)| after_input_tokens < current)
        {
            self.smallest = Some((count, after_input_tokens));
        }
        false
    }

    fn smallest(&self) -> Option<usize> {
        self.smallest.map(|(count, _)| count)
    }
}

impl Driver {
    /// Runs the placeholder rung against the live head.
    ///
    /// Returns `NoEligibleRange` when there is nothing to replace, or when even
    /// replacing every tool-result body in the turn leaves the projection over
    /// capacity, because the irreducible remainder — prefix, intent, calls,
    /// placeholders, notice — does not fit. The caller keeps the existing
    /// protected-overflow refusal for that case.
    #[allow(
        clippy::too_many_arguments,
        reason = "the offline placeholders publication carries the same closed authority tuple as the paid fold"
    )]
    pub(super) async fn replace_protected_tool_results(
        &mut self,
        active: Option<&PromptCommand>,
        origin: super::super::compaction::CompactOrigin,
        runtime: &ExactCompactionRuntime,
        identity: DurableCompactionIdentity,
        operation_id: &str,
        source: DurableSessionAddress,
        source_projection_identity: Digest,
        source_harness_plan: Digest,
        before: ContextEvaluationAudit,
        occupancy_before: Option<u64>,
        native_protected_history: bool,
    ) -> Result<AgentCompactSessionOutcome, AgentSessionEndpointError> {
        let source_history = self
            .authority
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .clone();
        // Both sides of the shrink proof use fresh lowering, without the live
        // artifact or its request anchor. An active artifact may authorize
        // historical replay that a fresh successor would not send.
        let source_audit = match self
            .measure_placeholder_successor_with_wire_digest(runtime, identity, 0, &source_history)
            .await
        {
            Ok((status, audit, _)) => SourceAudit::Fresh {
                audit,
                occupancy: status.unanchored_occupancy_tokens(),
            },
            Err(error) if error.model_kind() == Some(ModelErrorKind::ProtectedHistory) => {
                let portable = portable_compaction_history(&source_history)
                    .map_err(|error| compact_runtime_endpoint_error(&error))?;
                if portable.identity() == source_history.identity() {
                    return Err(compact_runtime_endpoint_error(&error));
                }
                // There is no fresh wire to compare when unbound replay cannot
                // lower at all. Retain the unanchored source audit solely for
                // this existing portable-rescue rung; a preparable source can
                // never use the artifact-bearing audit.
                SourceAudit::UnboundRescue {
                    audit: before,
                    occupancy: occupancy_before,
                }
            }
            Err(error) => return Err(compact_runtime_endpoint_error(&error)),
        };
        let (before, occupancy_before) = source_audit.measurements();
        let mut history = source_history.clone();
        let mut candidates = collect_placeholder_candidates(&history);
        let unprojected = self
            .search_placeholder_depth(runtime, identity, &history, &candidates, source_audit)
            .await;
        if !native_protected_history
            && let Ok(Some(accepted)) = &unprojected
            && let Some((portable, accepted)) = self
                .portable_placeholder_with_identical_wire(
                    runtime,
                    identity,
                    &source_history,
                    accepted,
                )
                .await?
            && source_audit.accepts(&accepted)
        {
            let portable_candidates = collect_placeholder_candidates(&portable);
            return self
                .publish_placeholders(
                    active,
                    origin,
                    identity,
                    operation_id,
                    source,
                    &portable,
                    &portable_candidates,
                    accepted,
                    source_projection_identity,
                    source_harness_plan,
                    before,
                    occupancy_before,
                )
                .await
                .map_err(|error| compact_runtime_endpoint_error(&error));
        }
        // Route enrollment alone never authorizes dropping replay. Try the
        // unchanged history first; a native refusal or no fitting depth permits
        // portable projection even when unchanged lowering itself succeeds.
        let accepted = if native_protected_history || matches!(&unprojected, Err(_) | Ok(None)) {
            let portable_history = portable_compaction_history(&source_history)
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
            if portable_history.identity() == source_history.identity() {
                unprojected?
            } else {
                let portable_candidates = collect_placeholder_candidates(&portable_history);
                let portable = self
                    .search_placeholder_depth(
                        runtime,
                        identity,
                        &portable_history,
                        &portable_candidates,
                        source_audit,
                    )
                    .await;
                match (portable, unprojected) {
                    (Ok(Some(accepted)), _) => {
                        history = portable_history;
                        candidates = portable_candidates;
                        Some(accepted)
                    }
                    // Portable replay loss is optional: neither a refusal nor
                    // a larger notice invalidates an already accepted pass.
                    (_, Ok(Some(accepted))) => Some(accepted),
                    (Err(error), _) => return Err(error),
                    // Replay-only omission is the last rung, used only when
                    // neither placeholder search accepted a successor.
                    (Ok(None), _) => {
                        let probe = self
                            .probe_placeholder_depth(
                                runtime,
                                identity,
                                &portable_history,
                                &portable_candidates,
                                0,
                            )
                            .await?;
                        history = portable_history;
                        candidates = portable_candidates;
                        probe
                            .after
                            .map(|after| AcceptedPlaceholders {
                                count: 0,
                                after,
                                occupancy_after: probe.occupancy,
                                wire_digest: probe.wire_digest,
                                clears_fence: probe.clears_fence,
                            })
                            .filter(|accepted| source_audit.accepts(accepted))
                    }
                }
            }
        } else {
            unprojected?
        };
        let Some(accepted) = accepted else {
            return Ok(AgentCompactSessionOutcome::NoEligibleRange);
        };
        self.publish_placeholders(
            active,
            origin,
            identity,
            operation_id,
            source,
            &history,
            &candidates,
            accepted,
            source_projection_identity,
            source_harness_plan,
            before,
            occupancy_before,
        )
        .await
        .map_err(|error| compact_runtime_endpoint_error(&error))
    }

    // Fresh lowering can already omit replay while canonical history retains
    // it. Compare without the loss note, then independently measure the full
    // note-bearing successor before authorizing its publication.
    async fn portable_placeholder_with_identical_wire(
        &mut self,
        runtime: &ExactCompactionRuntime,
        identity: DurableCompactionIdentity,
        source: &CanonicalHistory,
        accepted: &AcceptedPlaceholders,
    ) -> Result<Option<(CanonicalHistory, AcceptedPlaceholders)>, AgentSessionEndpointError> {
        let Some(unchanged_wire) = accepted.wire_digest else {
            return Ok(None);
        };
        let portable = portable_compaction_history(source)
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
        if portable.identity() == source.identity() {
            return Ok(None);
        }
        let candidates = collect_placeholder_candidates(&portable);
        let note_free =
            placeholder_successor(&portable, &candidates, accepted.count, identity.workflow_id)
                .map_err(|error| compact_runtime_endpoint_error(&error))?;
        let measured = self
            .measure_placeholder_successor_with_wire_digest(
                runtime,
                identity,
                accepted.count,
                &note_free.history,
            )
            .await;
        if !matches!(measured, Ok((_, _, Some(wire))) if wire == unchanged_wire) {
            return Ok(None);
        }
        let probe = self
            .probe_placeholder_depth(runtime, identity, &portable, &candidates, accepted.count)
            .await;
        let Ok(PlaceholdersProbe {
            after: Some(after),
            occupancy,
            wire_digest,
            clears_fence,
            ..
        }) = probe
        else {
            return Ok(None);
        };
        // The unchanged pass already shrinks; the note must not consume that
        // saving. The caller checks this against its common source audit.
        Ok(Some((
            portable,
            AcceptedPlaceholders {
                count: accepted.count,
                after,
                occupancy_after: occupancy,
                wire_digest,
                clears_fence,
            },
        )))
    }

    /// Finds a measured net-shrinking successor, preferring the shallowest one
    /// that also lands below the fence.
    ///
    /// Placeholder text and the two notices have real size. Replacing another
    /// small result can therefore grow the projection, so candidate depths are
    /// not a monotone predicate and cannot be binary searched. Every accepted
    /// depth is measured independently against the source. The first
    /// net-shrinking depth that clears the fence is therefore the shallowest
    /// qualifying depth and ends the search. If none clears the fence, every
    /// depth is measured and the smallest fitting successor wins for a fresh
    /// source. An unbound rescue has no comparable source wire and must clear
    /// the landing fence; a merely fitting successor cannot win that search.
    async fn search_placeholder_depth(
        &mut self,
        runtime: &ExactCompactionRuntime,
        identity: DurableCompactionIdentity,
        history: &CanonicalHistory,
        candidates: &[PlaceholderCandidate],
        source_audit: SourceAudit,
    ) -> Result<Option<AcceptedPlaceholders>, AgentSessionEndpointError> {
        let mut preference =
            PlaceholderPreference::new(source_audit.measurements().0.input_tokens());
        let mut smallest = None;
        for count in 1..=candidates.len() {
            let probe = self
                .probe_placeholder_depth(runtime, identity, history, candidates, count)
                .await?;
            let Some(after) = probe.after else {
                continue;
            };
            let accepted = AcceptedPlaceholders {
                count,
                after,
                occupancy_after: probe.occupancy,
                wire_digest: probe.wire_digest,
                clears_fence: probe.clears_fence,
            };
            if !source_audit.accepts(&accepted) {
                continue;
            }
            if matches!(source_audit, SourceAudit::UnboundRescue { .. }) {
                return Ok(Some(accepted));
            }
            if preference.observe(
                count,
                probe.fits,
                probe.clears_fence,
                accepted.after.input_tokens(),
            ) {
                return Ok(Some(accepted));
            }
            if preference.smallest() == Some(count) {
                smallest = Some(accepted);
            }
        }
        Ok(smallest)
    }

    /// Measures one candidate depth offline.
    async fn probe_placeholder_depth(
        &mut self,
        runtime: &ExactCompactionRuntime,
        identity: DurableCompactionIdentity,
        history: &CanonicalHistory,
        candidates: &[PlaceholderCandidate],
        count: usize,
    ) -> Result<PlaceholdersProbe, AgentSessionEndpointError> {
        let successor = self
            .placeholder_pass_successor(history, candidates, count, identity.workflow_id)
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
        let (status, after, wire_digest) = self
            .measure_placeholder_successor_with_wire_digest(
                runtime,
                identity,
                count,
                &successor.history,
            )
            .await
            .map_err(|error| compact_runtime_endpoint_error(&error))?;
        let fits = matches!(status.kind(), ContextStatusKind::Fits { .. });
        // Compaction records keep before and after on one unanchored basis.
        let occupancy = status.unanchored_occupancy_tokens();
        Ok(PlaceholdersProbe {
            fits,
            clears_fence: fits
                && placeholders_clear_the_fence(&status, occupancy.unwrap_or(after.input_tokens())),
            after: fits.then_some(after),
            occupancy,
            wire_digest,
        })
    }

    async fn measure_placeholder_successor_with_wire_digest(
        &mut self,
        runtime: &ExactCompactionRuntime,
        identity: DurableCompactionIdentity,
        depth: usize,
        successor: &CanonicalHistory,
    ) -> Result<(ContextStatus, ContextEvaluationAudit, Option<Digest>), AgentError> {
        // Unchanged and portable probes at the same depth lower different inputs
        // and must never share an attempt identity.
        let mut measurement_key = format!("placeholder-measurement-{depth}").into_bytes();
        measurement_key.extend_from_slice(&successor.identity().identity_bytes());
        let attempt_id = compaction_attempt_id(identity.workflow_id, &measurement_key)?;
        let prepared_at = runtime.clock.now();
        let request = ModelRequest::from_parts(
            self.authority
                .checkpoint()
                .manifest()
                .canonical()
                .stable_prefix()
                .clone(),
            ConversationView::from_shared(successor.clone(), WorkingProjection::default()),
            RequestMetadata::for_session_workflow(
                self.run_id,
                identity.workflow_id,
                attempt_id,
                prepared_at,
            ),
        )
        .map_err(|error| ModelError::new(ModelErrorKind::InvalidRequest, error.to_string()))
        .map_err(|error| self.project_model_error(error, ModelErrorPhase::Preparation))?;
        // Do not attach the live turn's family artifact: the successor's
        // coverage differs, and measuring against the live binding reports
        // Fits at depth 1 even when occupancy is still over the envelope.
        // Preserve the source session affinity, however: Claude OAuth needs
        // its logical session identity even for an offline measurement.
        let affinity = self
            .authority
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .affinity()
            .clone();
        self.measure_compaction_projection_with_wire_digest(
            runtime,
            request,
            CompactionFitBasis::OfflineSuccessor,
            Some(affinity),
            prepared_at,
        )
        .await
    }

    fn placeholder_pass_successor(
        &self,
        history: &CanonicalHistory,
        candidates: &[PlaceholderCandidate],
        count: usize,
        workflow_id: SessionWorkflowId,
    ) -> Result<PlaceholderSuccessor, AgentError> {
        let checkpoint = self.authority.checkpoint();
        let source = checkpoint.manifest().canonical().history();
        if count == 0 {
            replay_omission_successor(source, history, workflow_id)
        } else if history.identity() != source.identity() {
            portable_placeholder_successor(source, history, candidates, count, workflow_id)
        } else {
            placeholder_successor(history, candidates, count, workflow_id)
        }
    }

    /// Publishes the placeholder pass in one lease-fenced CAS.
    #[allow(
        clippy::too_many_arguments,
        reason = "the offline placeholders publication carries the same closed authority tuple as the paid fold"
    )]
    async fn publish_placeholders(
        &mut self,
        active: Option<&PromptCommand>,
        origin: super::super::compaction::CompactOrigin,
        identity: DurableCompactionIdentity,
        operation_id: &str,
        source: DurableSessionAddress,
        history: &CanonicalHistory,
        candidates: &[PlaceholderCandidate],
        accepted: AcceptedPlaceholders,
        source_projection_identity: Digest,
        source_harness_plan: Digest,
        before: ContextEvaluationAudit,
        occupancy_before: Option<u64>,
    ) -> Result<AgentCompactSessionOutcome, AgentError> {
        let successor = self.placeholder_pass_successor(
            history,
            candidates,
            accepted.count,
            identity.workflow_id,
        )?;
        let after = accepted.after;
        let occupancy_after = accepted.occupancy_after;
        let operation = Digest::sha256(
            DigestDomain::ProviderOperation,
            3,
            &[
                b"plakous-agent/session-tool-result-placeholders-publication-v1",
                identity.workflow_id.as_bytes(),
                &history.identity().identity_bytes(),
                &successor.history.identity().identity_bytes(),
                &successor.notice.identity().identity_bytes(),
            ],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Identity,
                "tool-result placeholders publication identity failed",
            )
        })?;
        let (lifecycle, mut ledger, context) = {
            let authority = self.authority.checkpoint();
            let manifest = authority.manifest();
            (
                manifest.lifecycle().clone(),
                manifest.ledger().clone(),
                manifest.canonical().context().retained_for_exact_resume(),
            )
        };
        let route_binding = {
            let authority = self.authority.checkpoint();
            authority
                .manifest()
                .binding()
                .route_binding_identity()
                .map_err(runtime_manifest_error)?
        };
        let model_binding = self
            .authority
            .checkpoint()
            .manifest()
            .binding()
            .model_binding();
        let publication = SemanticCompactionPublication::new(
            source.checkpoint_id,
            operation,
            route_binding,
            model_binding,
            source_harness_plan,
            before,
            after,
            None,
        )
        .map_err(runtime_manifest_error)?;
        let context = context
            .successful_tool_result_placeholders(
                &publication,
                &successor.notice,
                &successor.history,
                self.authority
                    .checkpoint()
                    .manifest()
                    .canonical()
                    .history()
                    .identity(),
                source_projection_identity,
                successor.selected_start,
                successor.selected_end,
                successor.replaced_results,
                u32::try_from(
                    self.authority
                        .checkpoint()
                        .manifest()
                        .canonical()
                        .history()
                        .segments()
                        .len(),
                )
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "placeholders source segment count is out of range",
                    )
                })?,
            )
            .map_err(runtime_manifest_error)?;
        let request_component = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&DurablePlaceholderRequestRecord {
                version: if accepted.count == 0 { 2 } else { 1 },
                workflow_id: identity.workflow_id,
                turn_fence: origin.is_turn_fence(),
                operation_id: operation_id.to_owned(),
                source_session: source.session_id,
                source_branch: source.branch_id,
                source_checkpoint: source.checkpoint_id,
                source_history: self
                    .authority
                    .checkpoint()
                    .manifest()
                    .canonical()
                    .history()
                    .identity(),
                source_projection: source_projection_identity,
                source_harness_plan,
                selected_start: successor.selected_start,
                selected_end: successor.selected_end,
                replaced_results: successor.replaced_results,
                replaced_bytes: successor.replaced_bytes,
                before_measurement: before.measurement_identity(),
                before_status: before.status_identity(),
                before_input_tokens: before.input_tokens(),
                target_affinity_entropy: [0; 32],
            })
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "tool-result placeholders request could not be encoded",
                )
            })?,
        )?;
        let evidence = runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&DurablePlaceholderEvidence {
                version: if accepted.count == 0 { 2 } else { 1 },
                workflow_id: identity.workflow_id,
                source_checkpoint: source.checkpoint_id,
                operation,
                successor_history: successor.history.identity(),
                notice_segment: successor.notice.identity(),
                replaced_results: successor.replaced_results,
                replaced_bytes: successor.replaced_bytes,
                after_measurement: after.measurement_identity(),
                after_status: after.status_identity(),
                after_input_tokens: after.input_tokens(),
            })
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "tool-result placeholders evidence could not be encoded",
                )
            })?,
        )?;
        let operation_id = self.take_operation_id()?;
        ledger = ledger
            .append_operation(
                DurableOperationRecord::new(
                    operation_id.0,
                    DurableOperationKind::Compaction {
                        workflow_id: identity.workflow_id,
                    },
                    request_component,
                    // Decided and published in the same commit. Naming a
                    // prepared or completed checkpoint would cite ids this
                    // branch never publishes, and publishing one to carry
                    // them would leave a compaction obligation demanding
                    // provider evidence for an operation that never
                    // dispatched.
                    vec![DurableOperationTransition::new(
                        identity.success_checkpoint,
                        DurableOperationState::Finalized(evidence),
                    )],
                )
                .map_err(runtime_manifest_error)?,
            )
            .map_err(runtime_manifest_error)?;
        // Drop the family artifact. Placeholders change the next request's
        // binding; keeping SuperGrok's prior artifact fails the following
        // ordinary round with ArtifactBindingMismatch.
        self.context_anchor = None;
        self.request_anchor = None;
        self.publish_checkpoint(
            StoreAdmission::Ordinary,
            identity.success_checkpoint,
            lifecycle,
            successor.history,
            ledger,
            context,
            None,
        )
        .await?;
        let observation = AgentEventKind::ToolResultsReplaced {
            results: successor.replaced_results,
            bytes: successor.replaced_bytes,
            tokens_before: occupancy_before.unwrap_or_else(|| before.input_tokens()),
            tokens_after: occupancy_after.unwrap_or_else(|| after.input_tokens()),
        };
        // An idle `/compact` that lands on this rung has no turn to hang the
        // observation on, but the operator still needs to be told that tool
        // bodies stopped being quoted verbatim.
        match active {
            Some(command) => {
                let _ = command
                    .shared
                    .events
                    .publish(EventContext::turn(command.turn_id), observation);
            }
            None => {
                let _ = self.events.publish(EventContext::agent(), observation);
            }
        }
        Ok(AgentCompactSessionOutcome::Compacted {
            workflow_id: identity.workflow_id,
            source: source.checkpoint_id,
            target: identity.success_checkpoint,
            summary: committed_compaction_summary(&self.authority)
                .map(|summary| summary.with_occupancy(occupancy_before, occupancy_after)),
            feed_seq: self.authority.feed_seq_at(identity.success_checkpoint),
        })
    }
}

/// True when this successor has landed at or below 50 % of the window.
///
/// A window the descriptor does not report leaves only the admitted-ceiling
/// fit to go on, which is the hard requirement the caller already checked.
///
/// This compares the successor's own unanchored measure to the window. The
/// fence that scheduled the compact used the anchored measure plus the
/// nested-observation tokens (`tool_group_fence_occupied_tokens`); a committed
/// compaction clears both, so until the next round re-anchors, the successor's
/// unanchored estimate is what the meter reads and the fence cannot fire.
/// This predicate asks where that reading lands.
fn placeholders_clear_the_fence(status: &ContextStatus, occupied: u64) -> bool {
    let Some(base) = status
        .capacity()
        .and_then(|capacity| compaction_fence_base_tokens(capacity.rule()))
    else {
        return true;
    };
    occupancy_gauge_percent(occupied, base)
        .is_none_or(|percent| percent < PLACEHOLDER_LANDING_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::PlaceholderPreference;

    fn choose(
        before: u64,
        probes: impl IntoIterator<Item = (usize, bool, bool, u64)>,
    ) -> (Option<usize>, usize) {
        let mut preference = PlaceholderPreference::new(before);
        let mut prepared = 0;
        for (count, fits, clears, after) in probes {
            prepared += 1;
            if preference.observe(count, fits, clears, after) {
                return (Some(count), prepared);
            }
        }
        (preference.smallest(), prepared)
    }

    #[test]
    fn anchored_fence_does_not_make_expansion_useful_and_depths_are_non_monotone() {
        assert_eq!(
            choose(
                100,
                [
                    (1, true, false, 104),
                    (2, true, true, 90),
                    (3, true, false, 96),
                ],
            ),
            (Some(2), 2),
            "the shallowest fence-clearing successor stops later preparations"
        );
        assert_eq!(
            // `clears_fence` can be true because publication would discard the
            // source anchor. Admission pressure still compares the two
            // independently measured requests and rejects net expansion.
            choose(100, [(1, true, true, 104), (2, true, true, 101)]),
            (None, 2),
            "fitting or fence-clearing expansion is not useful shrink"
        );
        assert_eq!(
            choose(
                100,
                [
                    (1, true, false, 90),
                    (2, true, false, 80),
                    (3, true, false, 85)
                ],
            ),
            (Some(2), 3),
            "when no depth clears the fence, every non-monotone depth is prepared and the smallest fitting successor wins"
        );
    }
}
