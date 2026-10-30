//! One admitted attempt and HTTP/SSE host; wire grammars remain family-owned.

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "closed protocol vocabulary and explicit attempt authority"
)]

use std::fmt::Debug;
use std::sync::Arc;

use plakous_attempt::LedgerPolicy;
use plakous_model::{Clock, IdGenerator, ModelEvent, ModelProviderRequestId, ModelResponse};

mod attempt;
mod http;

#[cfg(test)]
mod test_wire;

pub use attempt::{
    FamilyAttempt, FamilyAttemptBinding, FamilyAttemptError, FamilyAttemptEventReceiver,
    FamilyAttemptEventSink, FamilyAttemptProtocolError, FamilyAttemptResponse,
    FamilyOperationBinding, FamilyPhysicalRequestBinding, call_family_operation,
};
pub use http::{
    FamilyCredentialRejectedFn, FamilyHttpAttemptService, FamilyHttpErrorHead, FamilyHttpHooks,
    FamilyHttpResponseHead, FamilyHttpSafety, FamilyRedispatchFn, PreparedFamilyHttpOperation,
};

/// Grammar and projection facts, never credentials or provider policy.
pub trait FamilyWire: Clone + Copy + Debug + Eq + Send + Sync + 'static {
    /// Human protocol-family label used in existing safe diagnostics.
    const NAME: &'static str;
    /// The ledger's real family semantics.
    const LEDGER_POLICY: LedgerPolicy;
    /// Whether the family admits provider-hosted effects.
    const HOSTED_EFFECTS: bool;
    /// Whether the grammar emits a terminal observation before EOF sealing.
    const PROJECTOR_TERMINAL: bool;
    /// Whether an in-band provider deadline has a one-replay cap.
    const PROVIDER_DEADLINE_REPLAY_ONCE: bool;
    /// Whether request construction itself revalidates every retried grant.
    const BUILD_REVALIDATES_RETRY: bool;
    /// Whether retry-disabled status advice emits a declined retry observation.
    const STATUS_RETRY_DECLINE_NOTICE: bool = true;
    /// Exact stream authority.
    type StreamBinding: Clone + Debug + Send + Sync;
    /// Family-owned stream machine.
    type Projector: Send;
    /// Grammar failure classified by the dialect.
    type StreamError: Send;
    /// Sealed family projection.
    type Projection: Send;
    /// Typed stop/finish facts.
    type Stop: Debug + PartialEq + Send + Sync;

    /// Creates the real family projector with exact attribution.
    fn projector(
        binding: Self::StreamBinding,
        request_id: Option<ModelProviderRequestId>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self::Projector, Self::StreamError>;
    /// Feeds one bounded transport fragment to the grammar.
    fn push(
        projector: &mut Self::Projector,
        bytes: &[u8],
    ) -> Result<Vec<ModelEvent>, Self::StreamError>;
    /// Finishes the grammar through EOF, retaining its final observations.
    fn finish(projector: &mut Self::Projector) -> Result<Vec<ModelEvent>, Self::StreamError>;
    /// Takes the completed family projection.
    fn projection(projector: Self::Projector) -> Result<Self::Projection, Self::StreamError>;
    /// Exact in-band or HTTP attribution after completion.
    fn request_id(projector: &Self::Projector) -> Option<ModelProviderRequestId>;
    /// Family accounting that includes opaque items; otherwise retain event accounting.
    fn completed_items(projector: &Self::Projector) -> Option<u64>;
    /// Whether the grammar failure reports exhausted quota.
    fn usage_limit(error: &Self::StreamError) -> bool;
    /// Removes the family wrapper without changing the response or stop facts.
    fn into_parts(projection: Self::Projection) -> (ModelResponse, Self::Stop);
}
