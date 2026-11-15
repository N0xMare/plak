//! The concrete product vocabulary the TUI reads through its explicit module
//! boundary. Hosted sessions carry these opaque extension payloads without
//! owning settings, themes, or product intents.

pub(crate) use crate::capabilities::usage_verticals;
pub(crate) use crate::tui_preferences::TuiPreferences;
mod usage_decoration;
pub(crate) use crate::surface_protocol::tui_channel::{
    TuiActionSender as FaceActionSender, TuiEventReceiver as HostEventReceiver,
};
pub(crate) use crate::surface_protocol::{
    AgentAcknowledgement, HostEvent, McpOverview, ProfileApply, ProfileDifference, ProfileOutcome,
    ProfileRole, ProfileRow, ProfilesView, SettingsTheme, SettingsView, SurfaceIntent,
    SurfaceObservation, UserAction, UserActionSendError, WorkspaceChrome,
};
pub(crate) use plakd_wire_types::McpSessionBinding;

/// A test's TUI mailbox: the receiver, and a face sender no service serves,
/// whose intents reach it as sent.
#[cfg(all(test, feature = "tui"))]
pub(crate) fn user_action_channel() -> (
    FaceActionSender,
    crate::surface_protocol::UserActionReceiver,
) {
    let (actions, intents) = crate::surface_protocol::user_action_channel();
    (
        FaceActionSender::InProcess(crate::surface_protocol::FaceActionSender::unattached(
            actions,
        )),
        intents,
    )
}

/// A test's TUI observation mailbox: the sender, and the TUI's receiver of
/// it in process.
#[cfg(all(test, feature = "tui"))]
pub(crate) fn host_event_channel() -> (crate::surface_protocol::HostEventSender, HostEventReceiver)
{
    let (sender, receiver) = crate::surface_protocol::host_event_channel();
    (sender, HostEventReceiver::InProcess(receiver))
}
/// Reading, sniffing and chips of the image files a draft attaches.
pub(crate) use plakous_hosted::attachments as image_attachments;
pub(crate) use usage_decoration::UsageDecorationReader;
