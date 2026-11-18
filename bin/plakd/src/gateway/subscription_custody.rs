//! Subscription logins held by a configured gateway.

use plakous::gateway::GatewayCustody;
use plakous::subscriptions::NativeSubscriptions;
use plakous_hosted::agent::ProductStateRoots;

/// `subscriptions` with `gateway` holding their logins.
pub(crate) fn subscriptions_under(
    subscriptions: NativeSubscriptions,
    gateway: &GatewayCustody,
) -> NativeSubscriptions {
    subscriptions.with_custody(&gateway.custody())
}

/// `roots` whose sessions' subscription logins `gateway` holds.
pub(crate) fn roots_under(roots: ProductStateRoots, gateway: &GatewayCustody) -> ProductStateRoots {
    roots.with_subscription_custody(gateway.custody())
}
