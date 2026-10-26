pub(super) const COMPILED: bool = true;
use plakous::subscriptions::{NativeProvider, NativeSubscriptions};

pub(super) fn review_deadline() -> Option<u64> {
    NativeSubscriptions::notice(NativeProvider::Claude)
        .ok()
        .map(plakous::subscriptions::CompatibilityNotice::expires_at)
}
