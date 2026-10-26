pub(crate) type Observer = Option<std::convert::Infallible>;

pub(crate) fn observe(
    subscriptions: plakous::subscriptions::NativeSubscriptions,
    _: Observer,
) -> plakous::subscriptions::NativeSubscriptions {
    subscriptions
}
