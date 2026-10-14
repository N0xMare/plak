//! Family-agnostic attempt lifecycle, observation ledger, and await core.
//!
//! ## Ownership
//!
//! | Concern | This crate | Protocol-family / dialect crates |
//! | --- | --- | --- |
//! | `AtomicU8` phase constants / transitions | yes | map errors to dialect enums |
//! | Pure acceptance / physical outcomes | yes | re-export under dialect names |
//! | Physical-request observation ledger | yes | event classifiers, public errors |
//! | Contained sink delivery | yes | `PanicAsError` for dialect sink errors |
//! | Admitted await cleanup polls | yes | seals, hooks, call_operation |
//! | Lifecycle+ledger / ledger-only admission | yes | — |
//! | Shared `call_admitted_operation` | yes | dialect `AdmittedCallDriver` hooks |
//! | Family attempt core / terminal facts | yes | type aliases, dialect-only fields |
//! | Wire grammar / SSE / request encode | — | Responses-family (`plakous-responses`) or Messages |
//! | Dialect-erasing mega-`Service` type | — | never in this crate |
//!
//! **Protocol-family kernels** (`plakous-responses`, Messages residual in
//! `plakous-anthropic`) host wire grammar and dialect-shaped projection.
//! This crate owns only the family-agnostic physical-attempt authority plane
//! shared by Responses dialects and adoptable by Messages.
//!
//! ## Not in this crate
//!
//! - Provider HTTP clients, credentials, or route identities
//! - Agent loop / session / history stacks
//! - SSE framing or Responses/Messages event machines
//! - Canonical model types beyond cancellation tokens used by await hooks

#![forbid(unsafe_code)]
// Product names (OpenAI, OpenRouter, SuperGrok) appear in ownership docs.
// Ledger/policy structs intentionally use small bool policy tables.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::struct_excessive_bools,
    clippy::must_use_candidate,
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::type_complexity,
    reason = "product names; ledger helpers; policy bool tables; trait await hooks; panic-containment match arms; boxed futures"
)]

mod attempt;

pub use attempt::{
    ADMITTED, AdmissionFirstFuture, AdmittedAwaitHooks, AdmittedAwaitPolicy, AdmittedCallDriver,
    AttemptBinding, AttemptLifecycleError, AttemptObservationError, AttemptPhase,
    AttemptRetryDecline, AttemptSummary, AuthReservationError, AuthReservationMask, Cancellation,
    ContainedSinkDelivery, DECODE_FAILED, DECODED, DECODING, ExchangeBreak, FAILED, FailurePoint,
    FailureRisk, FamilyAttemptCore, FamilyAuthError, FamilyCoreError, FamilyObservationError,
    Jitter, LedgerOnlyAdmission, LedgerPolicy, LifecycleLedgerAdmission, LogicalOutcome,
    LongStreamReplayBudget, MAX_ADMITTED_CLEANUP_POLLS, ObservationSnapshot, PREPARED,
    PanicAsError, PhysicalRequestBinding, PhysicalRequestLedger, PhysicalRequestOutcome,
    ProviderAcceptance, ProviderFailure, ProviderRefusal, ReplaySafety, RetryClass, RetryPolicy,
    ServiceAdmissionSeal, SinkDeliveryError, TERMINAL, TerminalFacts, UnreachableRideOut,
    await_admitted_operation, begin_decoding, body_reports_usage_limit, call_admitted_operation,
    claim_protocol_failure, is_active, is_decoding, map_auth_reservation, map_core,
    map_observation, mark_decode_failed, mark_decoded, race_sink_delivery, retry_after_seconds,
    seal_service_attempt, terminal_usage_error, value_reports_usage_limit,
};
