//! The neutral hosted protocol and its typed extension mailboxes.

use plakous_host_protocol::{Core, ProtocolExtension, intent_channel, observation_channel};

/// An intent submitted for host admission, not evidence that execution began.
/// The extension parameter adds driver-owned intents without bypassing admission.
pub type UserAction<P = Core> = plakous_host_protocol::SessionIntent<P>;
/// A discardable observation of host state, not canonical history or turn authority.
pub type HostEvent<P = Core> = plakous_host_protocol::SessionObservation<P>;
/// Producer content that cannot forge keyed transcript publications; the host
/// projection owns transcript keys and publication order.
pub type ContentEvent<P = Core> = plakous_host_protocol::ContentObservation<P>;
/// A clonable, nonblocking intent producer. Bounded ordinary controls can be
/// refused; urgent controls use the mailbox's separate control path.
pub type UserActionSender<P = Core> = plakous_host_protocol::IntentSender<P>;
/// The single host consumer that delivers queued controls for admission.
pub type UserActionReceiver<P = Core> = plakous_host_protocol::IntentReceiver<P>;
/// One exact intent and its generation-bound acknowledgement. Dropping this
/// delivery, or its extracted acknowledgement guard, releases duplicate protection
/// for a non-repeatable intent.
pub type UserActionDelivery<P = Core> = plakous_host_protocol::IntentDelivery<P>;
/// A generation-bound guard that keeps a non-repeatable intent pending until
/// dropped, without acknowledging a later generation of the same control.
pub type UserActionAcknowledgement<P = Core> = plakous_host_protocol::IntentAcknowledgement<P>;
/// A rejected submission retaining the unaccepted intent for the caller.
/// Saturation, disconnection, duplication and attachment-role refusal are distinct.
pub type UserActionSendError<P = Core> = plakous_host_protocol::IntentSendError<P>;
/// A bounded, nonblocking observation producer. Saturation can drop replaceable
/// deltas or evict older observations in favor of latest state.
pub type HostEventSender<P = Core> = plakous_host_protocol::ObservationSender<P>;
/// A bounded observation consumer; received views do not substitute for canonical
/// history or proof that an admitted turn settled.
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
