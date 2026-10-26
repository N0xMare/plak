pub(super) const COMPILED: bool = true;
pub(super) fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::anthropic::ANTHROPIC_API_HOSTED_RETRIEVAL)
}
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::anthropic::ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_UNIX_SECONDS)
}
