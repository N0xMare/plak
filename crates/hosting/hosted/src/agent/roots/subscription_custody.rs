//! Who holds a hosted session's subscription logins: this process's store, or
//! a credential gateway the product names.

use plakous::subscriptions::NativeSubscriptions;

use super::ProductStateRoots;

/// `None` is this process's store.
pub(super) type SubscriptionCustody = Option<plakous::custody::Custody>;

pub(super) fn apply(
    custody: &SubscriptionCustody,
    subscriptions: NativeSubscriptions,
) -> NativeSubscriptions {
    match custody {
        Some(custody) => subscriptions.with_custody(custody),
        None => subscriptions,
    }
}

impl ProductStateRoots {
    /// Puts every native-subscription service these roots open under
    /// `custody`: a credential gateway holds the logins, and nothing here
    /// opens the subscription store.
    #[must_use]
    pub fn with_subscription_custody(mut self, custody: plakous::custody::Custody) -> Self {
        self.subscription_custody = Some(custody);
        self
    }
}
