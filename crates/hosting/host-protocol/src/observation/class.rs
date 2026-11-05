//! Who receives an observation once several attachments share a node
//! (wire v2, spec 02 §4.4).
//!
//! Every [`SessionObservation`] variant is exactly one class:
//!
//! * [`ObservationClass::State`]: broadcast to every attachment as the
//!   latest value of its kind, and part of the attach snapshot, so a client
//!   that reattaches sees it without any replay;
//! * [`ObservationClass::Event`]: broadcast as it happens. A
//!   [`Delivery::Required`] event is never dropped: an attachment that cannot
//!   queue one is detached and must reattach. A [`Delivery::Droppable`] event
//!   may be coalesced or evicted, and the gap is repaired by a resnapshot;
//! * [`ObservationClass::Addressed`]: delivered only to one attachment,
//!   either the one whose request it answers (a draft verdict, a queue edit,
//!   call detail, the navigator) or the one whose presentation baseline it
//!   advances (presentation snapshots and keyed patches, which each
//!   attachment holds for itself).
//!
//! The match below is exhaustive, so a new variant is a classification
//! decision. A scoped observation (an agent transcript stream) also goes
//! only to attachments that hold its scope; scope is routing, not class.

use super::SessionObservation;
use crate::intent::{ObservationExtension, ProtocolExtension};
use crate::views::transcript::TranscriptRow;

/// Whether a broadcast event may be lost.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Never dropped: an attachment that cannot take it is detached.
    Required,
    /// May be coalesced or evicted; a resnapshot repairs the gap.
    Droppable,
}

/// Who receives an observation.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ObservationClass {
    /// Broadcast latest value, part of the attach snapshot.
    State,
    /// Broadcast as it happens.
    Event(Delivery),
    /// For one attachment only.
    Addressed,
}

impl<P: ProtocolExtension> SessionObservation<P> {
    /// This observation's class (module docs).
    #[must_use]
    pub fn class(&self) -> ObservationClass {
        use ObservationClass::{Addressed, Event, State};
        let required = Event(Delivery::Required);
        let droppable = Event(Delivery::Droppable);
        match self {
            Self::ProviderSnapshot { .. }
            | Self::Activity { .. }
            | Self::AgentAttached(_)
            | Self::RestoredPause(_)
            | Self::Status { .. }
            | Self::QueueSnapshot { .. }
            | Self::ContextCapacityRecovery(_)
            | Self::SetBusy(_)
            | Self::PromptHistorySnapshot(_)
            | Self::Binding { .. }
            | Self::Usage(_)
            | Self::TurnUsage { .. }
            | Self::SubscriptionUsage(_)
            | Self::UsageLimits(_)
            | Self::SessionInfo(_)
            | Self::TemplateReadiness(_)
            | Self::ReattachReady
            | Self::SessionOpening
            | Self::SessionOpened { .. }
            | Self::PendingContinuation { .. }
            | Self::LastUserPromptAt(_)
            | Self::Processes { .. } => State,
            Self::PresentationSnapshot(_)
            | Self::TranscriptPatch(_)
            | Self::ResumeSessions(_)
            | Self::ToolInvocationOrigin { .. }
            | Self::PromptCorrelationReset
            | Self::IntentStarted(_)
            | Self::DraftAccepted(_)
            | Self::DraftRejected(_)
            | Self::DraftRestored(_)
            | Self::QueueEditApplied { .. }
            | Self::QueueEditRefused { .. }
            | Self::SteerAck { .. }
            | Self::TopologyNavigatorSnapshot(_)
            | Self::TopologyNavigatorUnavailable { .. }
            | Self::CallDetail(_)
            // The verdict on one operator action.
            | Self::StorageAdmissionRefused(_) => Addressed,
            Self::JobStarted { .. }
            | Self::JobPromoted { .. }
            | Self::JobTerminal { .. }
            | Self::PromptAdmitted(_)
            | Self::PromptStarted(_)
            | Self::ModelRefusal { .. }
            | Self::OutputDiscarded { .. }
            | Self::ToolLine { .. }
            | Self::ModelRetry(_)
            | Self::ClearView
            | Self::TurnLifecycle(_)
            | Self::ApprovalRequired { .. }
            | Self::ApprovalResolved { .. }
            // A park and its clear both land; the attach snapshot carries
            // the wait that holds now.
            | Self::CredentialWait { .. }
            | Self::Recovered { .. } => required,
            Self::Row(row) if row_is_required(row) => required,
            Self::Row(_)
            | Self::TranscriptStream { .. }
            | Self::UserMessage { .. }
            | Self::AssistantDelta(_)
            | Self::AssistantDone
            | Self::ReasoningDelta { .. }
            | Self::ReasoningSealed { .. }
            | Self::ModelRequestStarted
            | Self::ModelRequestQueued { .. } => droppable,
            Self::Extension(extension) => extension.wire_class(),
        }
    }

    /// Whether only a local face may receive this observation
    /// ([`crate::ObservationExtension::local_only`]).
    #[must_use]
    pub fn local_only(&self) -> bool {
        matches!(self, Self::Extension(extension) if extension.local_only())
    }
}

/// An error row is the record of a failure; any other row is repaired by a
/// resnapshot.
fn row_is_required(row: &TranscriptRow) -> bool {
    row.is_error()
}

/// The class an extension observation takes when the extension does not
/// say: latest-state values are [`ObservationClass::State`], anything else a
/// droppable event.
#[must_use]
pub fn default_extension_class<O: ObservationExtension>(observation: &O) -> ObservationClass {
    if observation.latest_state() {
        ObservationClass::State
    } else {
        ObservationClass::Event(Delivery::Droppable)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
