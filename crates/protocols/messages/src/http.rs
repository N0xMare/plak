//! Dialect hooks for the shared family HTTP/SSE host.
use super::attempt::MessagesWire;
/// PreparedMessagesHttpOperation over the shared family host.
pub type PreparedMessagesHttpOperation =
    plakous_exchange::family::PreparedFamilyHttpOperation<MessagesWire>;
/// MessagesHttpAttemptService over the shared family host.
pub type MessagesHttpAttemptService<H> =
    plakous_exchange::family::FamilyHttpAttemptService<MessagesWire, H>;
pub use plakous_exchange::family::FamilyHttpHooks as MessagesHttpHooks;
pub use plakous_exchange::family::{
    FamilyHttpErrorHead as MessagesHttpErrorHead,
    FamilyHttpResponseHead as MessagesHttpResponseHead, FamilyHttpSafety as MessagesHttpSafety,
};
/// MessagesCredentialRejectedFn over the shared family host.
pub type MessagesCredentialRejectedFn<H> =
    plakous_exchange::family::FamilyCredentialRejectedFn<MessagesWire, H>;
/// MessagesRedispatchFn over the shared family host.
pub type MessagesRedispatchFn<H> = plakous_exchange::family::FamilyRedispatchFn<MessagesWire, H>;
