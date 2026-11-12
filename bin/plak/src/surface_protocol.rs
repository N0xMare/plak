//! Product-local mailbox and appearance adapters over the public wire DTOs.

pub use crate::tui_preferences::SettingsTheme;
#[cfg(any(feature = "mcp", all(test, feature = "tui")))]
pub use plakd_wire_types::McpServerView;
pub use plakd_wire_types::{
    AgentAcknowledgement, McpOverview, PlakExtension, ProfileApply, ProfileDifference,
    ProfileOutcome, ProfileRole, ProfileRow, ProfilesView, SettingsCompositionOption,
    SettingsSnapshot, SurfaceIntent, SurfaceObservation, WorkspaceChrome,
};
#[cfg(any(feature = "mcp", feature = "tui"))]
pub use plakd_wire_types::{McpServerConfiguration, McpSessionBinding};
use plakous_host_protocol::{
    IntentReceiver, IntentSendError, IntentSender, SessionIntent, SessionObservation,
    intent_channel,
};
mod settings;
pub use settings::{SettingsUpdate, SettingsView};

#[cfg(any(feature = "mcp", feature = "tui"))]
pub(crate) mod mcp_lines;
#[cfg(feature = "tui")]
pub(crate) mod tui_channel;

/// The intent enum Plak's surfaces send: the protocol plus [`SurfaceIntent`].
pub type UserAction = SessionIntent<PlakExtension>;
/// The observation enum Plak's surfaces receive: the protocol plus
/// [`SurfaceObservation`].
pub type HostEvent = SessionObservation<PlakExtension>;
/// Producer content that cannot carry keyed publication frames.
pub type ContentEvent = plakous_host_protocol::ContentObservation<PlakExtension>;
/// Producer half of Plak's intent mailbox.
pub type UserActionSender = IntentSender<PlakExtension>;
/// Consumer half of Plak's intent mailbox.
pub type UserActionReceiver = IntentReceiver<PlakExtension>;
/// Typed refusal of a Plak intent by the bounded mailbox.
pub type UserActionSendError = IntentSendError<PlakExtension>;
/// Producer half of Plak's observation mailbox.
#[cfg(test)]
pub type HostEventSender = plakous_host_protocol::ObservationSender<PlakExtension>;
/// What a Plak face receives: its observation mailbox, taken as an
/// attachment of the host service.
pub type HostEventReceiver = plakous_hosted::host_service::FaceReceiver<PlakExtension>;

/// What a Plak face sends with: its intents pass through its session's host
/// service as its attachment's.
pub type FaceActionSender = plakous_hosted::host_service::FaceSender<PlakExtension>;

/// Construct Plak's bounded intent mailbox.
#[must_use]
pub fn user_action_channel() -> (UserActionSender, UserActionReceiver) {
    intent_channel()
}

/// Construct Plak's bounded observation mailbox, received by a face.
#[cfg(test)]
#[must_use]
pub fn host_event_channel() -> (HostEventSender, HostEventReceiver) {
    let (sender, receiver) = plakous_host_protocol::observation_channel();
    (sender, HostEventReceiver::new(receiver))
}

/// The mailbox ends a Plak face holds on a host worker: the face is an
/// attachment of the worker's session's host service, its intents passing
/// through the service and its observations taken from it
/// ([`plakous_hosted::host_service::face`]).
#[must_use]
pub fn face_of(
    (actions, events, worker): plakous_hosted::runtime::HostWorker<PlakExtension>,
) -> (
    FaceActionSender,
    HostEventReceiver,
    plakous_hosted::runtime::HostWorkerHandle,
) {
    let (actions, events) = plakous_hosted::host_service::face(actions, events);
    (actions, events, worker)
}
