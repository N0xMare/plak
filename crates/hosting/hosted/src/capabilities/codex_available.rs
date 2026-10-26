pub(super) const COMPILED: bool = true;
pub(super) fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    NativeProvider::Codex.hosted_retrieval()
}
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

pub(super) fn review_deadline() -> Option<u64> {
    NativeSubscriptions::notice(NativeProvider::Codex)
        .ok()
        .map(plakous::subscriptions::CompatibilityNotice::expires_at)
}
