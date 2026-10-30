//! Dialect hooks for the shared family HTTP/SSE host.
use super::attempt::ChatWire;
/// PreparedChatHttpOperation over the shared family host.
pub type PreparedChatHttpOperation =
    plakous_exchange::family::PreparedFamilyHttpOperation<ChatWire>;
/// ChatHttpAttemptService over the shared family host.
pub type ChatHttpAttemptService<H> =
    plakous_exchange::family::FamilyHttpAttemptService<ChatWire, H>;
pub use plakous_exchange::family::FamilyHttpHooks as ChatHttpHooks;
pub use plakous_exchange::family::{
    FamilyHttpErrorHead as ChatHttpErrorHead, FamilyHttpResponseHead as ChatHttpResponseHead,
    FamilyHttpSafety as ChatHttpSafety,
};

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "focused loopback fixtures use infallible constants and assertion extraction"
)]
mod tests;
