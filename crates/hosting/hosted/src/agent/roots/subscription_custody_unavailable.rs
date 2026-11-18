use plakous::subscriptions::NativeSubscriptions;

/// Without a subscription route there is no login to hold elsewhere.
pub(super) type SubscriptionCustody = ();

pub(super) const fn apply(
    (): &SubscriptionCustody,
    subscriptions: NativeSubscriptions,
) -> NativeSubscriptions {
    subscriptions
}
