//! The TUI's wire face. Compiled only with the `tui` feature:
//! a headless product has no sender for it.

#[cfg(test)]
use super::{FaceActionSender, HostEventReceiver};
use super::{HostEvent, IntentSendError, PlakExtension, UserAction, UserActionSendError};

/// What the TUI sends with: its face on an embedded or separately hosted session, wire v2
/// frames to the session's host service ([`plakous_host_protocol::client`]),
/// or in unit tests an attachment of a service in the test's process.
#[derive(Clone)]
pub enum TuiActionSender {
    /// A face of this home's running host.
    Socket(std::sync::Arc<crate::tui_launch::socket::SocketFace>),
    /// Wire v2 frames.
    Wire(plakous_host_protocol::client::WireSender<PlakExtension>),
    /// A unit test's attachment of a host service in its process.
    #[cfg(test)]
    InProcess(FaceActionSender),
}

/// An action the session did not take, learned after its send returned.
pub type Unsent = plakous_host_protocol::client::SendError<PlakExtension>;

impl TuiActionSender {
    /// Asks the serving host to stop this session without detaching.
    pub fn stop_session(&self, session: &str) -> bool {
        match self {
            Self::Socket(face) => face.stop_session(session),
            Self::Wire(_) => false,
            #[cfg(test)]
            Self::InProcess(_) => false,
        }
    }
    /// Sends `action` without waiting for the session: a later refusal
    /// comes back through [`TuiEventReceiver::try_unsent`].
    ///
    /// # Errors
    ///
    /// The action back when the session is already gone.
    #[allow(
        clippy::result_large_err,
        reason = "the refusal hands the rejected action back so the caller can retry it verbatim"
    )]
    pub fn send(&self, action: UserAction) -> Result<(), UserActionSendError> {
        match self {
            Self::Socket(face) => face.send(action),
            Self::Wire(wire) => wire.send(action).map_err(|error| match error {
                plakous_host_protocol::client::SendError::Refused(refused) => refused,
                unsent => IntentSendError::Disconnected(unsent.into_intent()),
            }),
            #[cfg(test)]
            Self::InProcess(face) => face.send(action),
        }
    }

    /// Waits up to `timeout` for the session's verdict on every action sent
    /// so far; whether none is left waiting.
    #[must_use]
    pub fn settle(&self, timeout: std::time::Duration) -> bool {
        match self {
            Self::Socket(face) => face.settle(timeout),
            Self::Wire(wire) => wire.settle(timeout),
            #[cfg(test)]
            Self::InProcess(_) => true,
        }
    }
}

/// What the TUI receives, from the same face as its
/// [`TuiActionSender`].
pub enum TuiEventReceiver {
    /// A face of this home's running host.
    Socket(std::sync::Arc<crate::tui_launch::socket::SocketFace>),
    /// Wire v2 frames.
    Wire(plakous_host_protocol::client::WireReceiver<PlakExtension>),
    /// A unit test's attachment of a host service in its process.
    #[cfg(test)]
    InProcess(HostEventReceiver),
    /// A unit test's actions the session did not take, and nothing else.
    #[cfg(test)]
    Refusing(std::sync::Mutex<std::collections::VecDeque<Unsent>>),
}

impl TuiEventReceiver {
    /// Client-local build notice for this socket face, outside the wire.
    pub(crate) fn host_notice(&self) -> Option<String> {
        match self {
            Self::Socket(face) => face.host_notice(),
            Self::Wire(_) => None,
            #[cfg(test)]
            Self::InProcess(_) | Self::Refusing(_) => None,
        }
    }

    /// The next observation, as [`HostEventReceiver::try_recv`].
    ///
    /// # Errors
    ///
    /// `Empty` when none is ready, `Disconnected` once the session is gone.
    pub fn try_recv(&self) -> Result<HostEvent, std::sync::mpsc::TryRecvError> {
        match self {
            Self::Socket(face) => face.try_recv(),
            Self::Wire(wire) => wire.try_recv(),
            #[cfg(test)]
            Self::InProcess(face) => face.try_recv(),
            #[cfg(test)]
            Self::Refusing(_) => Err(std::sync::mpsc::TryRecvError::Empty),
        }
    }

    /// Why the underlying transport ended, when it has.
    #[must_use]
    pub fn ended(&self) -> Option<String> {
        match self {
            Self::Socket(face) => face.ended(),
            Self::Wire(wire) => wire.ended(),
            #[cfg(test)]
            _ => None,
        }
    }

    /// Every observation ready now.
    #[cfg(test)]
    pub fn try_iter(&self) -> impl Iterator<Item = HostEvent> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    /// The next action sent on this face that the session did not take.
    #[must_use]
    pub fn try_unsent(&self) -> Option<Unsent> {
        match self {
            Self::Socket(face) => face.try_unsent(),
            Self::Wire(wire) => wire.try_unsent(),
            #[cfg(test)]
            Self::InProcess(_) => None,
            #[cfg(test)]
            Self::Refusing(unsent) => unsent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front(),
        }
    }

    /// Installs the hook that wakes the TUI when something arrives.
    pub fn set_notify(&self, notify: impl Fn() + Send + Sync + 'static) {
        match self {
            Self::Socket(face) => face.set_notify(notify),
            Self::Wire(wire) => wire.set_notify(notify),
            #[cfg(test)]
            Self::InProcess(face) => face.set_notify(notify),
            #[cfg(test)]
            Self::Refusing(_) => drop(notify),
        }
    }

    /// Whether `scope`'s transcript lost content its next baseline repays.
    #[must_use]
    pub fn transcript_resnapshot_required_for(
        &self,
        scope: &plakous_host_protocol::views::transcript::TranscriptScope,
    ) -> bool {
        match self {
            Self::Socket(face) => face.transcript_resnapshot_required_for(scope),
            Self::Wire(wire) => wire.transcript_resnapshot_required_for(scope),
            #[cfg(test)]
            Self::InProcess(face) => face.transcript_resnapshot_required_for(scope),
            #[cfg(test)]
            Self::Refusing(_) => false,
        }
    }
}
