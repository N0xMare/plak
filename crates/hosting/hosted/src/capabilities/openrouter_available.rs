pub(super) const COMPILED: bool = true;
pub(super) fn review_deadline() -> Option<u64> {
    Some(plakous::openrouter::OPENROUTER_CHAT_REVIEW_DUE_AT)
}
