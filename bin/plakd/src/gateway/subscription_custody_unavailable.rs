//! A build without a subscription route has no login for a gateway to hold.

use plakous::gateway::GatewayCustody;
use plakous::subscriptions::NativeSubscriptions;
use plakous_hosted::agent::ProductStateRoots;

pub(crate) const fn subscriptions_under(
    subscriptions: NativeSubscriptions,
    _: &GatewayCustody,
) -> NativeSubscriptions {
    subscriptions
}

pub(crate) const fn roots_under(roots: ProductStateRoots, _: &GatewayCustody) -> ProductStateRoots {
    roots
}
