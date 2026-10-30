//! Family-owned grammar over the shared admitted attempt host.

use std::sync::Arc;

use plakous_exchange::family::FamilyWire;
use plakous_model::{Clock, IdGenerator, ModelEvent, ModelProviderRequestId, ModelResponse};

use crate::{
    MessagesStopOutcome, MessagesStreamError, MessagesStreamProjector, MessagesTerminalProjection,
    PreparedMessagesStreamBinding,
};

/// The family grammar supplied to the shared exchange host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessagesWire;

pub use plakous_attempt::MAX_ADMITTED_CLEANUP_POLLS as MAX_MESSAGES_ADMITTED_CLEANUP_POLLS;
pub use plakous_exchange::family::{
    FamilyAttemptBinding as MessagesAttemptBinding,
    FamilyOperationBinding as MessagesOperationBinding,
    FamilyPhysicalRequestBinding as MessagesPhysicalRequestBinding,
    call_family_operation as call_messages_operation,
};
/// MessagesAttempt over the shared family host.
pub type MessagesAttempt = plakous_exchange::family::FamilyAttempt<MessagesWire>;
/// MessagesAttemptError over the shared family host.
pub type MessagesAttemptError = plakous_exchange::family::FamilyAttemptError<MessagesWire>;
/// MessagesAttemptProtocolError over the shared family host.
pub type MessagesAttemptProtocolError =
    plakous_exchange::family::FamilyAttemptProtocolError<MessagesWire>;
/// MessagesAttemptEventReceiver over the shared family host.
pub type MessagesAttemptEventReceiver =
    plakous_exchange::family::FamilyAttemptEventReceiver<MessagesWire>;
/// MessagesAttemptEventSink over the shared family host.
pub type MessagesAttemptEventSink = plakous_exchange::family::FamilyAttemptEventSink<MessagesWire>;
/// MessagesAttemptResponse over the shared family host.
pub type MessagesAttemptResponse = plakous_exchange::family::FamilyAttemptResponse<MessagesWire>;

pub use plakous_attempt::ProviderRefusal as MessagesProviderRefusal;
impl FamilyWire for MessagesWire {
    const NAME: &'static str = "Messages";
    const LEDGER_POLICY: plakous_attempt::LedgerPolicy =
        plakous_attempt::LedgerPolicy::MESSAGES_FAMILY;
    const HOSTED_EFFECTS: bool = true;
    const PROJECTOR_TERMINAL: bool = true;
    const PROVIDER_DEADLINE_REPLAY_ONCE: bool = false;
    const BUILD_REVALIDATES_RETRY: bool = false;
    type StreamBinding = PreparedMessagesStreamBinding;
    type Projector = MessagesStreamProjector;
    type StreamError = MessagesStreamError;
    type Projection = MessagesTerminalProjection;
    type Stop = MessagesStopOutcome;
    fn projector(
        binding: Self::StreamBinding,
        request_id: Option<ModelProviderRequestId>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self::Projector, Self::StreamError> {
        let projector = MessagesStreamProjector::new(binding, clock, ids)?;
        Ok(match request_id {
            Some(id) => projector.with_provider_request_id(id),
            None => projector,
        })
    }
    fn push(
        projector: &mut Self::Projector,
        bytes: &[u8],
    ) -> Result<Vec<ModelEvent>, Self::StreamError> {
        projector.push(bytes)
    }
    fn finish(projector: &mut Self::Projector) -> Result<Vec<ModelEvent>, Self::StreamError> {
        projector.finish_eof()
    }
    fn projection(projector: Self::Projector) -> Result<Self::Projection, Self::StreamError> {
        projector.into_terminal_projection()
    }
    fn request_id(projector: &Self::Projector) -> Option<ModelProviderRequestId> {
        projector.provider_request_id().cloned()
    }
    fn completed_items(projector: &Self::Projector) -> Option<u64> {
        Some(projector.completed_items())
    }
    fn usage_limit(error: &Self::StreamError) -> bool {
        error
            .provider_payload()
            .is_some_and(plakous_attempt::body_reports_usage_limit)
    }
    fn into_parts(projection: Self::Projection) -> (ModelResponse, Self::Stop) {
        let (response, _, stop) = projection.into_parts();
        (response, stop)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert typed failure paths")]
mod tests;
