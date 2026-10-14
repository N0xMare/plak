//! The provider outcome record (spec 01 §5.5): what one round's provider
//! attempt returned and what the driver decided on it, written as that
//! attempt's terminal evidence.

use crate::runtime::continuation::runtime_component;
use crate::runtime::terminal::DurableAgentError;
use crate::runtime::{ContextOverflowRemedy, OperationId};
use crate::{
    AgentError, AgentErrorKind, DurableOperationKind, DurableOperationRecord,
    DurableOperationState, ReconstructiveComponent, RuntimeComponentKind, SessionStoreError,
};
use plakous_model::{ModelOperationRoot, ModelResponse, Usage};

const PROVIDER_OUTCOME_TAG: &str = "provider-outcome-v2";

/// Every evidence tag other than `provider-outcome-v2` that a live driver or
/// a recovery closure of this build writes onto an operation record (spec 01
/// §5.5). A round provider record's evidence carrying any other tag is
/// corrupt.
pub(in crate::runtime) const CLOSURE_TAGS: [&str; 20] = [
    // Live operation and tool dispositions.
    "completed",
    "failed",
    "completed-tool-outcome-v1",
    "completed-tool-outcome-v2",
    "failed-tool-outcome-v1",
    "proven-not-dispatched-tool-v1",
    "joined-process-session-v1",
    // Local-composition child evidence.
    "plakous-local-composition-child-completed-v1",
    "plakous-local-composition-child-completed-v2",
    "plakous-local-composition-child-failure-v1",
    "plakous-local-composition-child-rejected-v1",
    "plakous-local-composition-child-unsupported-v1",
    "plakous-local-composition-child-admission-failure-v1",
    // Recovery closures: abandonment evidence, Code Cell abandonment and the
    // not-started terminal `finalize_not_started` records.
    "plakous-agent/abandoned-provider-attempt-v1",
    "plakous-agent/abandoned-compaction-v1",
    "plakous-agent/abandoned-tool-effect-v1",
    "plakous-agent/abandoned-process-session-v1",
    "plakous-code-cell-abandonment-evidence-v1",
    "plakous-terminal-result-pending",
    "plakous-terminal-result",
];

/// Per-root-turn guard state; the only authority for each guard, live and
/// restored. Frames carry the same value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each per-turn guard is spent independently and persists separately"
)]
pub(in crate::runtime) struct TurnBudgets {
    pub(in crate::runtime) output_limit_retry_taken: bool,
    pub(in crate::runtime) content_filter_retry_spent: bool,
    pub(in crate::runtime) overflow_recovery_taken: bool,
    pub(in crate::runtime) paid_fold_spent: bool,
    pub(in crate::runtime) pressure_fold_spent: bool,
    /// Rounds admitted so far, the wrap-up round included.
    pub(in crate::runtime) model_rounds: u64,
}

impl TurnBudgets {
    pub(in crate::runtime) fn over_round_cap(&self, max_model_rounds: usize) -> bool {
        u64::try_from(max_model_rounds).is_ok_and(|max| self.model_rounds > max)
    }
}

/// What the attempt itself returned.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "the canonical record keeps the complete typed response without an indirection"
)]
pub(in crate::runtime) enum ProviderAttemptResult {
    Response(ModelResponse),
    Failure(DurableAgentError),
}

/// The closed set of decisions a round's provider attempt can carry.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum RoundDecision {
    Terminal,
    FailTurn(FailTurnShape),
    Continue,
    ToolBatch,
    RetryOutputLimit,
    RetryContentFilter,
    OverflowFold,
}

/// The terminal a `FailTurn` decision committed the turn to, so the
/// consequence needs no measurement or classification of its own.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct FailTurnShape {
    pub(in crate::runtime) failure: DurableAgentError,
    /// The turn's own prompt is replaced by the withheld-prompt note.
    pub(in crate::runtime) prompt_withheld: bool,
    pub(in crate::runtime) remedy: Option<DecidedRemedy>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(in crate::runtime) enum DecidedRemedy {
    ResendShorter,
    Compact,
    ClearOrFork,
}

impl From<ContextOverflowRemedy> for DecidedRemedy {
    fn from(value: ContextOverflowRemedy) -> Self {
        match value {
            ContextOverflowRemedy::ResendShorter => Self::ResendShorter,
            ContextOverflowRemedy::Compact => Self::Compact,
            ContextOverflowRemedy::ClearOrFork => Self::ClearOrFork,
        }
    }
}

impl From<DecidedRemedy> for ContextOverflowRemedy {
    fn from(value: DecidedRemedy) -> Self {
        match value {
            DecidedRemedy::ResendShorter => Self::ResendShorter,
            DecidedRemedy::Compact => Self::Compact,
            DecidedRemedy::ClearOrFork => Self::ClearOrFork,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ProviderOutcome {
    pub(in crate::runtime) result: ProviderAttemptResult,
    /// Exactly what the attempt observed: a failed attempt keeps the partial
    /// usage its stream reported; `None` is never filled in.
    pub(in crate::runtime) usage: Option<Usage>,
    pub(in crate::runtime) decision: RoundDecision,
    /// The budgets in force after `decision`.
    pub(in crate::runtime) budgets: TurnBudgets,
    /// Accepted steers pending when the decision was taken, in acceptance order.
    pub(in crate::runtime) steering: Vec<OperationId>,
}

impl ProviderOutcome {
    pub(in crate::runtime) fn component(&self) -> Result<ReconstructiveComponent, AgentError> {
        let bytes = serde_json::to_vec(&(PROVIDER_OUTCOME_TAG, self)).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "provider outcome could not be canonicalized",
            )
        })?;
        runtime_component(RuntimeComponentKind::Operation, bytes)
    }

    /// Decodes evidence carrying this record. Evidence tagged with one of
    /// [`CLOSURE_TAGS`] is `None`; untagged bytes, any other tag, or a record
    /// that does not decode to its exact canonical bytes, are corrupt.
    pub(in crate::runtime) fn decode(
        evidence: &ReconstructiveComponent,
    ) -> Result<Option<Self>, AgentError> {
        let corrupt = || AgentError::from_store(SessionStoreError::corrupt());
        let (tag, value) = serde_json::from_slice::<(String, serde_json::Value)>(evidence.bytes())
            .map_err(|_| corrupt())?;
        if tag != PROVIDER_OUTCOME_TAG {
            return if CLOSURE_TAGS.contains(&tag.as_str()) {
                Ok(None)
            } else {
                Err(corrupt())
            };
        }
        let outcome: Self = serde_json::from_value(value).map_err(|_| corrupt())?;
        if serde_json::to_vec(&(PROVIDER_OUTCOME_TAG, &outcome)).map_err(|_| corrupt())?
            != evidence.bytes()
            || !outcome.coherent()
        {
            return Err(corrupt());
        }
        Ok(Some(outcome))
    }

    /// Whether the decision is one the driver can take on this result: a
    /// terminal answer, continuation or tool batch on a response, and a retry
    /// or fold on a failure. `Terminal` requires no pending steering; a
    /// `FailTurn` (a round cap or wrap-up failure) may settle steers unapplied.
    fn coherent(&self) -> bool {
        let response = matches!(self.result, ProviderAttemptResult::Response(_));
        match self.decision {
            RoundDecision::Terminal => response && self.steering.is_empty(),
            RoundDecision::Continue | RoundDecision::ToolBatch => response,
            RoundDecision::RetryOutputLimit
            | RoundDecision::RetryContentFilter
            | RoundDecision::OverflowFold => !response,
            RoundDecision::FailTurn(_) => true,
        }
    }

    pub(in crate::runtime) const fn response(&self) -> Option<&ModelResponse> {
        match &self.result {
            ProviderAttemptResult::Response(response) => Some(response),
            ProviderAttemptResult::Failure(_) => None,
        }
    }

    #[cfg(test)]
    pub(in crate::runtime) const fn failure(&self) -> Option<&DurableAgentError> {
        match &self.result {
            ProviderAttemptResult::Response(_) => None,
            ProviderAttemptResult::Failure(error) => Some(error),
        }
    }
}

/// The outcome record a round provider operation's current state carries,
/// if any. Compaction attempts are compaction records and carry none.
///
/// `Completed`, and a `Finalized` that followed `Completed`, are only ever
/// written with the record itself, so either without it is corrupt.
pub(in crate::runtime) fn provider_outcome(
    record: &DurableOperationRecord,
) -> Result<Option<ProviderOutcome>, AgentError> {
    if !matches!(
        record.kind(),
        DurableOperationKind::Provider {
            root: ModelOperationRoot::Turn(_),
            ..
        }
    ) {
        return Ok(None);
    }
    let after_completed = record
        .transitions()
        .iter()
        .rev()
        .nth(1)
        .is_some_and(|previous| matches!(previous.state(), DurableOperationState::Completed(_)));
    match record.state() {
        DurableOperationState::Completed(evidence) => required(evidence),
        DurableOperationState::Finalized(evidence) if after_completed => required(evidence),
        DurableOperationState::Finalized(evidence)
        | DurableOperationState::ProvenNotDispatched(evidence)
        | DurableOperationState::OutcomeUnknown(evidence) => ProviderOutcome::decode(evidence),
        _ => Ok(None),
    }
}

fn required(evidence: &ReconstructiveComponent) -> Result<Option<ProviderOutcome>, AgentError> {
    ProviderOutcome::decode(evidence)?
        .map(Some)
        .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))
}

/// The commit-valid response a round provider operation's current state
/// records, for readers that only ask whether there is one.
///
/// # Errors
///
/// A record that carries a corrupt outcome is an error, never "no response".
pub(in crate::runtime) fn recorded_provider_response(
    record: &DurableOperationRecord,
) -> Result<Option<ModelResponse>, AgentError> {
    Ok(
        provider_outcome(record)?.and_then(|outcome| match outcome.result {
            ProviderAttemptResult::Response(response) => Some(response),
            ProviderAttemptResult::Failure(_) => None,
        }),
    )
}

/// Synthetic outcome records for ledgers built by tests.
#[cfg(test)]
pub(in crate::runtime) mod test_support {
    use super::{
        AgentError, FailTurnShape, ProviderAttemptResult, ProviderOutcome, RoundDecision,
        SessionStoreError, TurnBudgets,
    };
    use crate::ReconstructiveComponent;
    use crate::runtime::terminal::DurableAgentError;
    use plakous_model::ModelResponse;

    /// The record the live driver writes for a final answer or a failure
    /// that ends the turn, with no usage, budgets or steering.
    pub(in crate::runtime) fn outcome_component(
        attempt: Result<&ModelResponse, &AgentError>,
    ) -> Result<ReconstructiveComponent, AgentError> {
        // What the live driver records for such a fixture round: a round
        // that asks the harness for tools is a tool batch, any other answer
        // is terminal.
        let asks_for_tools = |response: &ModelResponse| {
            response
                .output()
                .iter()
                .flat_map(plakous_model::ConversationItem::blocks)
                .any(|block| {
                    matches!(block, plakous_model::ContentBlock::ToolCall(call)
                        if matches!(call.owner(), plakous_model::ToolExecutionOwner::PlakLocal
                            | plakous_model::ToolExecutionOwner::PlakMcp { .. })
                        && matches!(call.state(), plakous_model::ToolCallState::Requested
                            | plakous_model::ToolCallState::Pending))
                })
        };
        let usage = attempt.as_ref().ok().and_then(|response| response.usage());
        let (result, decision) = match attempt {
            Ok(response) => (
                ProviderAttemptResult::Response(response.clone()),
                if asks_for_tools(response) {
                    RoundDecision::ToolBatch
                } else {
                    RoundDecision::Terminal
                },
            ),
            Err(error) => (
                ProviderAttemptResult::Failure(DurableAgentError::from_error(error)?),
                RoundDecision::FailTurn(FailTurnShape {
                    failure: DurableAgentError::from_error(error)?,
                    prompt_withheld: false,
                    remedy: None,
                }),
            ),
        };
        ProviderOutcome {
            result,
            usage,
            decision,
            budgets: TurnBudgets::default(),
            steering: Vec::new(),
        }
        .component()
    }

    /// A recorded response with the rest of its record, so a test can
    /// rewrite the response and keep the decision.
    pub(in crate::runtime) fn recorded_response_parts(
        evidence: &ReconstructiveComponent,
    ) -> Result<(ProviderOutcome, ModelResponse), AgentError> {
        let outcome = ProviderOutcome::decode(evidence)?
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        let response = outcome
            .response()
            .cloned()
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        Ok((outcome, response))
    }

    /// The canonical bytes of the record in `evidence` with `decision` in
    /// place of the one it recorded, so a test fixture records the decision
    /// the live driver takes on its rewritten response.
    pub(in crate::runtime) fn recorded_decision_bytes(
        evidence: &ReconstructiveComponent,
        decision: RoundDecision,
    ) -> Result<Vec<u8>, AgentError> {
        let mut outcome = ProviderOutcome::decode(evidence)?
            .ok_or_else(|| AgentError::from_store(SessionStoreError::corrupt()))?;
        outcome.decision = decision;
        Ok(outcome.component()?.bytes().to_vec())
    }

    /// The canonical bytes of `outcome` recording `response` instead.
    pub(in crate::runtime) fn recorded_response_bytes(
        mut outcome: ProviderOutcome,
        response: ModelResponse,
    ) -> Result<Vec<u8>, AgentError> {
        outcome.usage = response.usage();
        outcome.result = ProviderAttemptResult::Response(response);
        Ok(outcome.component()?.bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous_model::{
        Actor, ContentBlock, ConversationItem, Digest, DigestDomain, EntryId, FinishDetails,
        FinishReason, ModelError, ModelErrorKind, Provenance, TextBlock,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn response() -> Result<ModelResponse, Box<dyn std::error::Error>> {
        Ok(ModelResponse::new(
            vec![ConversationItem::new(
                EntryId::try_from(uuid::Uuid::from_u128(0xA2_01))?,
                Actor::Assistant,
                vec![ContentBlock::Text(TextBlock::new("done", None)?)],
                Provenance::ModelOutput,
            )?],
            FinishDetails::new(FinishReason::Stop, None),
            Some(Usage::new(11, 3, 0, 0, 0)),
            Some("resp-a2".into()),
            Digest::sha256(DigestDomain::Blob, 1, &[b"a2-plan"])?,
        ))
    }

    fn failure(kind: ModelErrorKind) -> Result<DurableAgentError, AgentError> {
        DurableAgentError::from_error(&AgentError::from_model(ModelError::new(
            kind,
            "attempt failed",
        )))
    }

    fn budgets() -> TurnBudgets {
        TurnBudgets {
            output_limit_retry_taken: true,
            content_filter_retry_spent: true,
            overflow_recovery_taken: false,
            paid_fold_spent: true,
            pressure_fold_spent: false,
            model_rounds: 3,
        }
    }

    #[test]
    fn every_decision_round_trips_through_its_exact_canonical_bytes() -> TestResult {
        let fail = RoundDecision::FailTurn(FailTurnShape {
            failure: failure(ModelErrorKind::ContextLimit)?,
            prompt_withheld: true,
            remedy: Some(DecidedRemedy::ResendShorter),
        });
        for (result, decision) in [
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::Terminal,
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::Continue,
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::ToolBatch,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
                fail,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::OutputLimit)?),
                RoundDecision::RetryOutputLimit,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::ContentFilteredIncomplete)?),
                RoundDecision::RetryContentFilter,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
                RoundDecision::OverflowFold,
            ),
        ] {
            let steering = if decision == RoundDecision::Terminal {
                Vec::new()
            } else {
                vec![OperationId(4), OperationId(9)]
            };
            let outcome = ProviderOutcome {
                result,
                usage: Some(Usage::new(7, 2, 0, 0, 0)),
                decision,
                budgets: budgets(),
                steering,
            };
            let component = outcome.component()?;
            assert!(component.bytes().starts_with(b"[\"provider-outcome-v2\","));
            assert_eq!(ProviderOutcome::decode(&component)?, Some(outcome));
        }
        Ok(())
    }

    #[test]
    fn a_non_canonical_or_malformed_record_is_corrupt_and_other_evidence_is_none() -> TestResult {
        let outcome = ProviderOutcome {
            result: ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
            usage: None,
            decision: RoundDecision::FailTurn(FailTurnShape {
                failure: failure(ModelErrorKind::Protocol)?,
                prompt_withheld: false,
                remedy: None,
            }),
            budgets: TurnBudgets::default(),
            steering: Vec::new(),
        };
        let mut padded = outcome.component()?.bytes().to_vec();
        padded.push(b' ');
        for bytes in [
            padded,
            b"[\"provider-outcome-v2\",{}]".to_vec(),
            b"[\"provider-outcome-v2\",{\"result\":1}]".to_vec(),
            b"corrupt".to_vec(),
        ] {
            let evidence = runtime_component(RuntimeComponentKind::Operation, bytes)?;
            assert!(
                ProviderOutcome::decode(&evidence).is_err(),
                "malformed record accepted"
            );
        }
        let other = runtime_component(
            RuntimeComponentKind::Operation,
            b"[\"plakous-code-cell-abandonment-evidence-v1\",{}]".to_vec(),
        )?;
        assert_eq!(ProviderOutcome::decode(&other)?, None);
        Ok(())
    }

    #[test]
    fn an_incoherent_decision_is_corrupt() -> TestResult {
        for (result, decision, steering) in [
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
                RoundDecision::Terminal,
                Vec::new(),
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
                RoundDecision::ToolBatch,
                Vec::new(),
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::Protocol)?),
                RoundDecision::Continue,
                Vec::new(),
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::RetryOutputLimit,
                Vec::new(),
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::RetryContentFilter,
                Vec::new(),
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::OverflowFold,
                Vec::new(),
            ),
        ] {
            let outcome = ProviderOutcome {
                result,
                usage: None,
                decision,
                budgets: TurnBudgets::default(),
                steering,
            };
            assert!(
                ProviderOutcome::decode(&outcome.component()?).is_err(),
                "{:?}",
                outcome.decision
            );
        }
        Ok(())
    }

    /// The frozen bytes of one record per decision. A change here is an epoch
    /// bump, never a silent re-pin.
    #[test]
    fn one_record_per_decision_has_frozen_bytes() -> TestResult {
        const GOLDEN: [&str; 7] = [
            "b0b683b4ae1e0bd9499b9f19ce68589175ec8bbcd1edab4262743d105b9a04bd",
            "6976c4d2283d4c87f09ee9703039402d78c4b9a488e9fa4db8925e9c142c4ab1",
            "6f20caa88e11bd4ae0a937c1b42a716cbbb34fe0edc6aca35942ea2324754e86",
            "149487ced9e8d22acd070eef9458bc37403d3024387f228447fb015c3a30b0db",
            "8bb87b1ff4247f2936df201468d5a66fc76d89ce1b237a5abddb898d260d2aa7",
            "df056b6fdfcf3cf7565519103cd238a154bbd36499252dd3fae4cbb65b69a901",
            "57aadb4f022c0b70d202cce8ba3aba080e878461ec18d0e7e436ba1d97d8e50a",
        ];
        let fail = RoundDecision::FailTurn(FailTurnShape {
            failure: failure(ModelErrorKind::ContextLimit)?,
            prompt_withheld: true,
            remedy: Some(DecidedRemedy::ResendShorter),
        });
        let mut actual = Vec::new();
        for (result, decision) in [
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::Terminal,
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::Continue,
            ),
            (
                ProviderAttemptResult::Response(response()?),
                RoundDecision::ToolBatch,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
                fail,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::OutputLimit)?),
                RoundDecision::RetryOutputLimit,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::ContentFilteredIncomplete)?),
                RoundDecision::RetryContentFilter,
            ),
            (
                ProviderAttemptResult::Failure(failure(ModelErrorKind::ContextLimit)?),
                RoundDecision::OverflowFold,
            ),
        ] {
            let steering = if decision == RoundDecision::Terminal {
                Vec::new()
            } else {
                vec![OperationId(4)]
            };
            let bytes = ProviderOutcome {
                result,
                usage: Some(Usage::new(7, 2, 0, 0, 0)),
                decision,
                budgets: budgets(),
                steering,
            }
            .component()?
            .bytes()
            .to_vec();
            let digest = Digest::sha256(DigestDomain::Blob, 1, &[&bytes])?;
            actual.push(digest.bytes().iter().fold(String::new(), |mut text, byte| {
                let _ = std::fmt::Write::write_fmt(&mut text, format_args!("{byte:02x}"));
                text
            }));
        }
        assert_eq!(actual, GOLDEN, "provider-outcome-v2 golden bytes changed");
        Ok(())
    }

    #[test]
    fn budgets_compare_rounds_against_the_admitted_cap() {
        let budgets = TurnBudgets {
            model_rounds: 4,
            ..TurnBudgets::default()
        };
        assert!(budgets.over_round_cap(3));
        assert!(!budgets.over_round_cap(4));
    }

    fn round_provider(
        states: Vec<DurableOperationState>,
    ) -> Result<DurableOperationRecord, Box<dyn std::error::Error>> {
        let request = runtime_component(RuntimeComponentKind::Operation, b"request".to_vec())?;
        Ok(DurableOperationRecord::new(
            7,
            DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(plakous_model::TurnId::try_from(
                    uuid::Uuid::from_u128(0xA4_E0),
                )?),
                attempt_id: plakous_model::AttemptId::try_from(uuid::Uuid::from_u128(0xA4_E1))?,
            },
            request,
            std::iter::once(DurableOperationState::Prepared)
                .chain(states)
                .enumerate()
                .map(|(index, state)| {
                    Ok(crate::DurableOperationTransition::new(
                        crate::CheckpointId::try_from(uuid::Uuid::from_u128(
                            0xA4_F0 + index as u128,
                        ))?,
                        state,
                    ))
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?,
        )?)
    }

    fn tagged(tag: &str) -> Result<ReconstructiveComponent, AgentError> {
        runtime_component(
            RuntimeComponentKind::Operation,
            serde_json::to_vec(&(tag, "closure detail")).map_err(|_| {
                AgentError::new(AgentErrorKind::Internal, "evidence could not be encoded")
            })?,
        )
    }

    #[test]
    fn each_enumerated_closure_tag_reads_as_no_outcome() -> TestResult {
        for tag in CLOSURE_TAGS {
            let evidence = tagged(tag)?;
            assert_eq!(ProviderOutcome::decode(&evidence)?, None, "{tag}");
            for states in [
                vec![
                    DurableOperationState::Dispatched,
                    DurableOperationState::OutcomeUnknown(evidence.clone()),
                    DurableOperationState::Finalized(evidence.clone()),
                ],
                vec![
                    DurableOperationState::Dispatched,
                    DurableOperationState::OutcomeUnknown(evidence.clone()),
                ],
                vec![
                    DurableOperationState::ProvenNotDispatched(evidence.clone()),
                    DurableOperationState::Finalized(evidence.clone()),
                ],
            ] {
                assert_eq!(provider_outcome(&round_provider(states)?)?, None, "{tag}");
            }
        }
        Ok(())
    }

    #[test]
    fn an_unknown_tag_is_corrupt_in_every_state() -> TestResult {
        let evidence = tagged("plakous-agent/unknown-closure-v1")?;
        assert!(ProviderOutcome::decode(&evidence).is_err());
        for states in [
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::OutcomeUnknown(evidence.clone()),
            ],
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::OutcomeUnknown(evidence.clone()),
                DurableOperationState::Finalized(evidence.clone()),
            ],
            vec![DurableOperationState::ProvenNotDispatched(evidence.clone())],
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::Completed(evidence.clone()),
            ],
        ] {
            assert!(provider_outcome(&round_provider(states)?).is_err());
        }
        Ok(())
    }

    #[test]
    fn completed_and_its_finalization_require_the_outcome_record() -> TestResult {
        let closure = tagged("plakous-agent/abandoned-provider-attempt-v1")?;
        for states in [
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::Completed(closure.clone()),
            ],
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::Completed(closure.clone()),
                DurableOperationState::Finalized(closure.clone()),
            ],
        ] {
            assert!(
                provider_outcome(&round_provider(states)?).is_err(),
                "a closure tag stood in for provider-outcome-v2"
            );
        }
        let outcome = ProviderOutcome {
            result: ProviderAttemptResult::Response(response()?),
            usage: None,
            decision: RoundDecision::Terminal,
            budgets: TurnBudgets::default(),
            steering: Vec::new(),
        };
        let record = outcome.component()?;
        for states in [
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::Completed(record.clone()),
            ],
            vec![
                DurableOperationState::Dispatched,
                DurableOperationState::Completed(record.clone()),
                DurableOperationState::Finalized(record.clone()),
            ],
        ] {
            assert_eq!(
                provider_outcome(&round_provider(states)?)?,
                Some(outcome.clone())
            );
        }
        Ok(())
    }
}
