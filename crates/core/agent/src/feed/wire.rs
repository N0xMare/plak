//! The feed codec's own spellings for the public enums a fact names.
//!
//! The feed encoding is frozen per codec version, so it never borrows another
//! type's serde: each embedded public enum maps through a private mirror with
//! explicit `snake_case` spellings. A public variant this codec does not know
//! fails the encode (and so refuses the commit) instead of inventing bytes.

macro_rules! mirror {
    ($module:ident, $public:ty, { $($variant:ident => $spelling:literal),+ $(,)? }) => {
        #[allow(
            dead_code,
            reason = "each mirror generates both the required and the optional field codec"
        )]
        pub(super) mod $module {
            use serde::{Deserialize, Deserializer, Serialize, Serializer};

            type Public = $public;

            #[derive(Clone, Copy, Deserialize, Serialize)]
            pub(in crate::feed) enum Wire {
                $(#[serde(rename = $spelling)] $variant,)+
            }

            #[allow(unreachable_patterns, reason = "the public enum may be non-exhaustive")]
            pub(in crate::feed) fn to_wire<E: serde::ser::Error>(value: Public) -> Result<Wire, E> {
                match value {
                    $(Public::$variant => Ok(Wire::$variant),)+
                    _ => Err(E::custom(concat!(
                        "feed codec has no spelling for this ",
                        stringify!($module),
                    ))),
                }
            }

            pub(in crate::feed) const fn from_wire(wire: Wire) -> Public {
                match wire {
                    $(Wire::$variant => Public::$variant,)+
                }
            }

            #[allow(
                clippy::trivially_copy_pass_by_ref,
                reason = "serde's `with` contract passes the field by reference"
            )]
            pub(in crate::feed) fn serialize<S: Serializer>(
                value: &Public,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                to_wire::<S::Error>(*value)?.serialize(serializer)
            }

            pub(in crate::feed) fn deserialize<'de, D: Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Public, D::Error> {
                Wire::deserialize(deserializer).map(from_wire)
            }

            pub(in crate::feed) mod option {
                use serde::{Deserialize, Deserializer, Serialize, Serializer};

                #[allow(
                    clippy::ref_option,
                    reason = "serde's `with` contract passes the field by reference"
                )]
                pub(in crate::feed) fn serialize<S: Serializer>(
                    value: &Option<super::Public>,
                    serializer: S,
                ) -> Result<S::Ok, S::Error> {
                    value
                        .map(super::to_wire::<S::Error>)
                        .transpose()?
                        .serialize(serializer)
                }

                pub(in crate::feed) fn deserialize<'de, D: Deserializer<'de>>(
                    deserializer: D,
                ) -> Result<Option<super::Public>, D::Error> {
                    Option::<super::Wire>::deserialize(deserializer)
                        .map(|wire| wire.map(super::from_wire))
                }
            }
        }
    };
}

mirror!(turn_origin, plakous_model::TurnOrigin, {
    UserPrompt => "user_prompt",
    HostWake => "host_wake",
});

mirror!(finish_reason, plakous_model::FinishReason, {
    Stop => "stop",
    ToolCalls => "tool_calls",
    AssistantContinuation => "assistant_continuation",
    ProviderContinuation => "provider_continuation",
    Length => "length",
    Refusal => "refusal",
    Policy => "policy",
});

mirror!(model_error_kind, plakous_model::ModelErrorKind, {
    InvalidConfiguration => "invalid_configuration",
    InvalidRequest => "invalid_request",
    UnsupportedCapability => "unsupported_capability",
    ExpectedRequestMismatch => "expected_request_mismatch",
    ScriptExhausted => "script_exhausted",
    InvalidEvent => "invalid_event",
    IncompleteResponse => "incomplete_response",
    ContentFilteredIncomplete => "content_filtered_incomplete",
    Policy => "policy",
    OutcomeUnknown => "outcome_unknown",
    Cancelled => "cancelled",
    Timeout => "timeout",
    Authentication => "authentication",
    ReauthenticationRequired => "reauthentication_required",
    Authorization => "authorization",
    RateLimited => "rate_limited",
    QuotaExceeded => "quota_exceeded",
    ContextLimit => "context_limit",
    ProtectedHistory => "protected_history",
    OutputLimit => "output_limit",
    Transport => "transport",
    Unavailable => "unavailable",
    ResourceLimit => "resource_limit",
    Protocol => "protocol",
});

mirror!(segment_kind, plakous_model::HistorySegmentKind, {
    Conversation => "conversation",
    RecentIntent => "recent_intent",
    Steering => "steering",
    ToolExchange => "tool_exchange",
    ReasoningReplay => "reasoning_replay",
    Compaction => "compaction",
});

mirror!(agent_error_kind, crate::AgentErrorKind, {
    InvalidInput => "invalid_input",
    Backpressure => "backpressure",
    Shutdown => "shutdown",
    Cancelled => "cancelled",
    Model => "model",
    Tool => "tool",
    ExternalBindingUnavailable => "external_binding_unavailable",
    OutcomeUnknown => "outcome_unknown",
    Session => "session",
    IncompatibleSession => "incompatible_session",
    RoundLimit => "round_limit",
    Identity => "identity",
    Internal => "internal",
});

mirror!(control_error_kind, crate::ControlErrorKind, {
    Backpressure => "backpressure",
    LedgerFull => "ledger_full",
    Shutdown => "shutdown",
    ForeignTurn => "foreign_turn",
    Stale => "stale",
    WrongEffectDecision => "wrong_effect_decision",
    Expired => "expired",
    Cancelled => "cancelled",
    WrongPhase => "wrong_phase",
    QueueFull => "queue_full",
    InvalidInput => "invalid_input",
    IdentityConflict => "identity_conflict",
    Internal => "internal",
    StoragePressure => "storage_pressure",
    AdmissionRejected => "admission_rejected",
    OutcomeUnknown => "outcome_unknown",
    Reclaimed => "reclaimed",
});

mirror!(abandoned_claim, crate::AbandonedTurnClaim, {
    RunningTool => "running_tool",
    ModelRequest => "model_request",
    CompactionNotApplied => "compaction_not_applied",
    NothingStarted => "nothing_started",
    ToolRoundCompleted => "tool_round_completed",
});

mirror!(abandonment_cause, crate::AbandonmentCause, {
    ProcessLost => "process_lost",
    Interrupted => "interrupted",
});

mirror!(overflow_remedy, crate::ContextOverflowRemedy, {
    ResendShorter => "resend_shorter",
    Compact => "compact",
    ClearOrFork => "clear_or_fork",
});

/// A workflow identity as 64 lowercase hex digits.
pub(super) mod workflow_id {
    use std::fmt::Write as _;

    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    use crate::session::SessionWorkflowId;

    pub(in crate::feed) fn serialize<S: Serializer>(
        value: &SessionWorkflowId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(64);
        for byte in value.as_bytes() {
            let _ = write!(text, "{byte:02x}");
        }
        serializer.serialize_str(&text)
    }

    pub(in crate::feed) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SessionWorkflowId, D::Error> {
        let text = String::deserialize(deserializer)?;
        let invalid = || D::Error::custom("feed workflow identity is not 64 lowercase hex digits");
        if text.len() != 64
            || !text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid());
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(|_| invalid())?;
        }
        SessionWorkflowId::new(bytes).map_err(|_| invalid())
    }
}
