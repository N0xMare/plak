pub(super) const COMPILED: bool = true;
#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry function pointers share an optional signature with feature-omitted implementations"
)]
pub(super) fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::openrouter::OPENROUTER_HOSTED_RETRIEVAL)
}
#[allow(
    clippy::unnecessary_wraps,
    reason = "route registry function pointers share an optional signature with feature-omitted implementations"
)]
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::openrouter::OPENROUTER_CHAT_REVIEW_DUE_AT)
}
