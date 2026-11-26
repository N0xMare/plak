//! Advisory product notices share the sole connection writer, not admission.

use super::{ClientFrame, HostConnection, HostFrame, PoisonError, ProtocolExtension, SessionHost};
use crate::error::CliResult;
use plakous_host_protocol::wire::HostLifecycle;
/// Product projection called by the connection writer.
pub type LifecycleOutput<P> = fn(&mut dyn std::io::Write, HostLifecycle<P>) -> CliResult<()>;

pub(super) struct Delivery<T> {
    pending: std::collections::VecDeque<T>,
    sending: bool,
}

impl<T> Delivery<T> {
    pub(super) fn new(latest: Option<T>) -> Self {
        Self {
            pending: latest.into_iter().collect(),
            sending: false,
        }
    }
}

impl<P: ProtocolExtension> SessionHost<P> {
    /// This process's generation, also carried by welcome.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Latest advisory state, independent of admission.
    #[must_use]
    pub fn lifecycle(&self) -> Option<HostLifecycle<P>> {
        self.lifecycle_latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Joins transport send attempts, not peer receipt or any request verdict.
    /// A missing/slow peer cannot hold planned transition progress indefinitely.
    pub async fn wait_lifecycle_delivery(&self, bound: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + bound;
        loop {
            let pending = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .any(|delivery| !delivery.pending.is_empty() || delivery.sending);
            if !pending || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

impl<P: ProtocolExtension> SessionHost<P>
where
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: serde::Serialize,
    ClientFrame<P>: serde::de::DeserializeOwned,
{
    /// Publishes advisory product state only to handshaken connections.
    /// It does not touch admission, sessions, drafts or their outcomes.
    pub fn publish_lifecycle(&self, notice: HostLifecycle<P>) {
        let mut latest = self
            .lifecycle_latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut deliveries = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for delivery in deliveries.values_mut() {
            let queue = &mut delivery.pending;
            if queue.len() == 16 {
                queue.pop_front();
            }
            queue.push_back(notice.clone());
        }
        *latest = Some(notice);
        drop(deliveries);
        drop(latest);
        self.changed();
    }
}

impl<P: ProtocolExtension, W: std::io::Write + Clone> HostConnection<P, W>
where
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: serde::Serialize,
    ClientFrame<P>: serde::de::DeserializeOwned,
{
    /// Supplies the product lifecycle projection after Welcome.
    #[must_use]
    pub const fn with_lifecycle_output(mut self, output: LifecycleOutput<P>) -> Self {
        self.lifecycle_output = Some(output);
        self
    }

    /// Writes pending advisory lifecycle notices before ordinary output.
    ///
    /// # Errors
    /// The transport outbox cannot accept a frame.
    pub fn flush_lifecycle(&mut self) -> CliResult<()> {
        let notices = self
            .host
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&self.connection_id)
            .map_or_default(|delivery| {
                delivery.sending |= !delivery.pending.is_empty();
                std::mem::take(&mut delivery.pending)
            });
        for notice in notices {
            if let Some(output) = self.lifecycle_output {
                output(&mut self.out, notice)?;
            } else {
                self.write(&HostFrame::<P>::from(notice))?;
            }
        }
        if !self.buffered_output {
            self.lifecycle_output_sent();
        }
        Ok(())
    }
}

impl<P: ProtocolExtension, W: std::io::Write> HostConnection<P, W> {
    pub(super) fn lifecycle_output_sent(&self) {
        if let Some(delivery) = self
            .host
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&self.connection_id)
        {
            delivery.sending = false;
        }
    }
}
