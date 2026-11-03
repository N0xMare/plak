//! The shutdown of the host that attached a tree last, latched once raised:
//! a tree writer waiting on a storage outage selects on it rather than
//! polling, and the hub reads it when a root's release decides the tree's end.

use super::TreeDriver;

/// A host's shutdown, latched once signalled: a tree writer waiting on a
/// storage outage selects on it rather than polling.
#[derive(Clone, Debug, Default)]
pub struct HostShutdown(tokio_util::sync::CancellationToken);

impl HostShutdown {
    /// A signal not yet raised.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the signal; it stays raised.
    pub fn signal(&self) {
        self.0.cancel();
    }

    /// Whether the signal was raised.
    #[must_use]
    pub fn is_signalled(&self) -> bool {
        self.0.is_cancelled()
    }
}

impl TreeDriver {
    /// Binds the shutdown of the host that attached this tree last. Each
    /// writer takes the binding current when it asks for its turn, and stops
    /// waiting for a refused publication once that host's signal is latched.
    pub fn bind_host_shutdown(&self, signal: HostShutdown) {
        *self
            .host_shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(signal);
    }

    /// Whether the host that attached this tree last is shutting down.
    pub(crate) fn host_shutdown_latched(&self) -> bool {
        self.host_shutdown_signal()
            .is_some_and(|signal| signal.is_cancelled())
    }

    pub(super) fn host_shutdown_signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.host_shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|signal| signal.0.clone())
    }
}
