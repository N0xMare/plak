pub(super) const COMPILED: bool = true;
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::openai::OPENAI_PLATFORM_MODEL_MANIFEST_REVIEW_DUE_AT_UNIX_SECONDS)
}
