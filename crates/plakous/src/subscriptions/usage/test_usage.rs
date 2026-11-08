//! Deterministic native allowance I/O at the feature-owned test boundary.

use super::{NativeFuture, NativeProvider, NativeSubscriptions, SubscriptionUsageRead};
use std::sync::Arc;

type Read =
    dyn Fn(NativeProvider, bool) -> NativeFuture<'static, SubscriptionUsageRead> + Send + Sync;

#[derive(Clone)]
pub(in crate::subscriptions) struct Fixture(Option<Arc<Read>>);
pub(in crate::subscriptions) const EMPTY: Fixture = Fixture(None);

impl Fixture {
    pub fn read(
        &self,
        provider: NativeProvider,
        refreshing: bool,
    ) -> Option<NativeFuture<'static, SubscriptionUsageRead>> {
        self.0.as_ref().map(|read| read(provider, refreshing))
    }
}

impl NativeSubscriptions {
    /// Replaces native allowance I/O for deterministic host fixtures. The bool
    /// is true only for the refreshing legacy entry point.
    #[doc(hidden)]
    #[must_use]
    pub fn with_usage_fixture_for_tests(
        mut self,
        fixture: impl Fn(
            NativeProvider,
            bool,
        )
            -> std::pin::Pin<Box<dyn Future<Output = SubscriptionUsageRead> + Send>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.usage_fixture = Fixture(Some(Arc::new(fixture)));
        self
    }
}
