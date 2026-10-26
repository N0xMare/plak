pub(super) const COMPILED: bool = true;
pub(super) fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    Some(plakous::xai::XAI_API_HOSTED_RETRIEVAL)
}
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::xai::XAI_PLATFORM_REVIEW_DUE_AT_UNIX_SECONDS)
}
