//! The neutral hosted protocol and its typed extension mailboxes.

use plakous_host_protocol::{Core, ProtocolExtension, intent_channel, observation_channel};

/// Hosted UserAction with an optional driver-owned extension.
pub type UserAction<P = Core> = plakous_host_protocol::SessionIntent<P>;
/// Hosted HostEvent with an optional driver-owned extension.
pub type HostEvent<P = Core> = plakous_host_protocol::SessionObservation<P>;
/// Hosted UserActionSender with an optional driver-owned extension.
pub type UserActionSender<P = Core> = plakous_host_protocol::IntentSender<P>;
/// Hosted UserActionReceiver with an optional driver-owned extension.
pub type UserActionReceiver<P = Core> = plakous_host_protocol::IntentReceiver<P>;
/// Hosted UserActionDelivery with an optional driver-owned extension.
pub type UserActionDelivery<P = Core> = plakous_host_protocol::IntentDelivery<P>;
/// Hosted UserActionAcknowledgement with an optional driver-owned extension.
pub type UserActionAcknowledgement<P = Core> = plakous_host_protocol::IntentAcknowledgement<P>;
/// Hosted UserActionSendError with an optional driver-owned extension.
pub type UserActionSendError<P = Core> = plakous_host_protocol::IntentSendError<P>;
/// Hosted HostEventSender with an optional driver-owned extension.
pub type HostEventSender<P = Core> = plakous_host_protocol::ObservationSender<P>;
/// Hosted HostEventReceiver with an optional driver-owned extension.
pub type HostEventReceiver<P = Core> = plakous_host_protocol::ObservationReceiver<P>;

/// Construct a bounded intent mailbox.
#[must_use]
pub fn user_action_channel<P: ProtocolExtension>() -> (UserActionSender<P>, UserActionReceiver<P>) {
    intent_channel()
}
/// Construct a bounded observation mailbox.
#[must_use]
pub fn host_event_channel<P: ProtocolExtension>() -> (HostEventSender<P>, HostEventReceiver<P>) {
    observation_channel()
}
