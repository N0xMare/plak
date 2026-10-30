//! Family-owned grammar over the shared admitted attempt host.

use crate::{
    ChatFinishReason, ChatStreamError, ChatStreamProjector, ChatTerminalProjection,
    PreparedChatStreamBinding,
};
use plakous_exchange::family::FamilyWire;
use plakous_model::{Clock, IdGenerator, ModelEvent, ModelProviderRequestId, ModelResponse};
use std::sync::Arc;

/// The family grammar supplied to the shared exchange host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatWire;

pub use plakous_attempt::MAX_ADMITTED_CLEANUP_POLLS as MAX_CHAT_ADMITTED_CLEANUP_POLLS;
pub use plakous_exchange::family::{
    FamilyAttemptBinding as ChatAttemptBinding, FamilyOperationBinding as ChatOperationBinding,
    FamilyPhysicalRequestBinding as ChatPhysicalRequestBinding,
    call_family_operation as call_chat_operation,
};
/// ChatAttempt over the shared family host.
pub type ChatAttempt = plakous_exchange::family::FamilyAttempt<ChatWire>;
/// ChatAttemptError over the shared family host.
pub type ChatAttemptError = plakous_exchange::family::FamilyAttemptError<ChatWire>;
/// ChatAttemptProtocolError over the shared family host.
pub type ChatAttemptProtocolError = plakous_exchange::family::FamilyAttemptProtocolError<ChatWire>;
/// ChatAttemptEventReceiver over the shared family host.
pub type ChatAttemptEventReceiver = plakous_exchange::family::FamilyAttemptEventReceiver<ChatWire>;
/// ChatAttemptEventSink over the shared family host.
pub type ChatAttemptEventSink = plakous_exchange::family::FamilyAttemptEventSink<ChatWire>;
/// ChatAttemptResponse over the shared family host.
pub type ChatAttemptResponse = plakous_exchange::family::FamilyAttemptResponse<ChatWire>;

/// Chat forbids hosted effects and requires a complete sealed aggregate.
pub const CHAT_FAMILY_LEDGER_POLICY: plakous_attempt::LedgerPolicy =
    plakous_attempt::LedgerPolicy {
        require_service_admitted_for_dispatch: true,
        require_active_request_always: false,
        dual_auth_slots: false,
        clear_service_on_terminal_success: true,
    };

impl FamilyWire for ChatWire {
    const NAME: &'static str = "Chat";
    const LEDGER_POLICY: plakous_attempt::LedgerPolicy = CHAT_FAMILY_LEDGER_POLICY;
    const HOSTED_EFFECTS: bool = false;
    const PROJECTOR_TERMINAL: bool = false;
    const PROVIDER_DEADLINE_REPLAY_ONCE: bool = true;
    const BUILD_REVALIDATES_RETRY: bool = true;
    const STATUS_RETRY_DECLINE_NOTICE: bool = false;
    type StreamBinding = PreparedChatStreamBinding;
    type Projector = (ChatStreamProjector, Option<ChatTerminalProjection>);
    type StreamError = ChatStreamError;
    type Projection = ChatTerminalProjection;
    type Stop = ChatFinishReason;
    fn projector(
        binding: Self::StreamBinding,
        request_id: Option<ModelProviderRequestId>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self::Projector, Self::StreamError> {
        Ok((
            ChatStreamProjector::new(binding, request_id, clock, ids),
            None,
        ))
    }
    fn push(
        projector: &mut Self::Projector,
        bytes: &[u8],
    ) -> Result<Vec<ModelEvent>, Self::StreamError> {
        projector.0.push(bytes)
    }
    fn finish(projector: &mut Self::Projector) -> Result<Vec<ModelEvent>, Self::StreamError> {
        projector.1 = Some(projector.0.finish()?);
        Ok(Vec::new())
    }
    fn projection(mut projector: Self::Projector) -> Result<Self::Projection, Self::StreamError> {
        match projector.1.take() {
            Some(projection) => Ok(projection),
            None => projector.0.finish(),
        }
    }
    fn request_id(projector: &Self::Projector) -> Option<ModelProviderRequestId> {
        projector
            .1
            .as_ref()
            .and_then(|projection| projection.response().provider_request_id())
            .and_then(|id| ModelProviderRequestId::new(id).ok())
    }
    fn completed_items(_projector: &Self::Projector) -> Option<u64> {
        None
    }
    fn usage_limit(error: &Self::StreamError) -> bool {
        error.usage_limit()
    }
    fn into_parts(projection: Self::Projection) -> (ModelResponse, Self::Stop) {
        projection.into_parts()
    }
}
