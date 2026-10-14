//! Family-agnostic attempt lifecycle, observation ledger, and await core.
//!
//! Hosted at the `plakous-attempt` crate root; see crate-level docs.

mod admission;
mod admission_first;
mod admitted_await;
mod binding;
mod call_operation;
mod core;
mod delivery;
mod emit;
mod facts;
mod ledger;
mod lifecycle;
mod map;
mod retry;
mod summary;
mod terminal;
pub use summary::{AttemptPhase, AttemptRetryDecline, AttemptSummary};

pub use admission::{LedgerOnlyAdmission, LifecycleLedgerAdmission};
pub use admission_first::AdmissionFirstFuture;
pub use admitted_await::{
    AdmittedAwaitHooks, AdmittedAwaitPolicy, ServiceAdmissionSeal, await_admitted_operation,
};
pub use binding::{AttemptBinding, PhysicalRequestBinding};
pub use call_operation::{AdmittedCallDriver, call_admitted_operation};
pub use core::{FamilyAttemptCore, FamilyCoreError};
pub use delivery::{ContainedSinkDelivery, PanicAsError};
pub use emit::{SinkDeliveryError, race_sink_delivery};
pub use facts::{PhysicalRequestOutcome, ProviderAcceptance};
pub use ledger::{
    AttemptObservationError, AuthReservationError, AuthReservationMask, LedgerPolicy,
    ObservationSnapshot, PhysicalRequestLedger,
};
pub use lifecycle::{
    ADMITTED, AttemptLifecycleError, DECODE_FAILED, DECODED, DECODING, FAILED,
    MAX_ADMITTED_CLEANUP_POLLS, PREPARED, TERMINAL, begin_decoding, claim_protocol_failure,
    is_active, is_decoding, mark_decode_failed, mark_decoded, seal_service_attempt,
};
pub use map::{
    FamilyAuthError, FamilyObservationError, map_auth_reservation, map_core, map_observation,
};
pub use retry::{
    FailurePoint, Jitter, LongStreamReplayBudget, ReplaySafety, RetryPolicy, UnreachableRideOut,
    body_reports_usage_limit, retry_after_seconds, terminal_usage_error, value_reports_usage_limit,
};
pub use terminal::{
    Cancellation, ExchangeBreak, FailureRisk, LogicalOutcome, ProviderFailure, ProviderRefusal,
    RetryClass, TerminalFacts,
};
