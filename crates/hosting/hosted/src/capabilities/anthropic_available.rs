pub(super) const COMPILED: bool = true;
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::anthropic::ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_UNIX_SECONDS)
}
