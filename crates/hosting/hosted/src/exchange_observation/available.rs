pub(crate) type Observer =
    Option<std::sync::Arc<dyn plakous::exchange_observation::ExchangeObserver>>;

pub(crate) fn observe(
    subscriptions: plakous::subscriptions::NativeSubscriptions,
    observer: Observer,
) -> plakous::subscriptions::NativeSubscriptions {
    subscriptions.with_exchange_observer(observer)
}
