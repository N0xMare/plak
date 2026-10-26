pub(super) const COMPILED: bool = true;
pub(super) fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::openrouter::OPENROUTER_HOSTED_RETRIEVAL)
}
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::openrouter::OPENROUTER_CHAT_REVIEW_DUE_AT)
}
