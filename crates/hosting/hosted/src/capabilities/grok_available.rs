pub(super) const COMPILED: bool = true;
pub(super) fn hosted_retrieval() -> Option<plakous::model::HostedRetrievalTable> {
    NativeProvider::Xai.hosted_retrieval()
}
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

pub(super) fn review_deadline() -> Option<u64> {
    NativeSubscriptions::notice(NativeProvider::Xai)
        .ok()
        .map(plakous::subscriptions::CompatibilityNotice::expires_at)
}
