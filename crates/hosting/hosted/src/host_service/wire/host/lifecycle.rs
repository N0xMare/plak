//! Advisory product notices share the sole connection writer, not admission.

use super::{
    ClientFrame, HostConnection, HostFrame, PoisonError, ProtocolExtension, SessionHost,
    write_frame,
};
use crate::error::CliResult;
use plakous_host_protocol::wire::HostLifecycle;
use std::time::Instant;

/// Process-local lifecycle facts. Starting and its readiness edge were not
/// published on the protocol 12 connection.
#[derive(Clone, Debug)]
pub enum LifecycleState<P: ProtocolExtension> {
    /// An existing product lifecycle transition.
    Transition(HostLifecycle<P>),
    /// A host that can answer Welcome but has not yet reopened admission.
    Starting {
        /// The same generation returned in Welcome.
        host_generation: u64,
    },
    /// That successor completed startup and reopened admission.
    Started {
        /// The same generation returned in Welcome.
        host_generation: u64,
    },
}

/// Draft projection called by the connection writer with a current countdown.
pub type LifecycleOutput<P> = fn(&mut dyn std::io::Write, LifecycleState<P>, u64) -> CliResult<()>;

#[derive(Clone)]
pub(super) struct Notice<P: ProtocolExtension> {
    state: LifecycleState<P>,
    deadline: Option<Instant>,
}

impl<P: ProtocolExtension> Notice<P> {
    fn remaining(&self, now: Instant) -> u64 {
        self.deadline.map_or(0, |deadline| {
            let remaining = deadline.saturating_duration_since(now);
            remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() != 0))
        })
    }
}

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
        let latest = self
            .lifecycle_latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &latest.as_ref()?.state {
            LifecycleState::Transition(notice) => Some(notice.clone()),
            _ => None,
        }
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
        self.publish_lifecycle_until(notice, None);
    }

    /// Publishes existing advisory state with its active monotonic deadline.
    pub fn publish_lifecycle_until(&self, notice: HostLifecycle<P>, deadline: Option<Instant>) {
        self.publish(Notice {
            state: LifecycleState::Transition(notice),
            deadline,
        });
    }

    /// Publishes successor startup independently of the bridge handshake.
    pub fn publish_starting(&self, deadline: Instant) {
        self.publish(Notice {
            state: LifecycleState::Starting {
                host_generation: self.generation,
            },
            deadline: Some(deadline),
        });
    }

    /// Publishes readiness only if startup is still the active phase.
    pub fn publish_started(&self) {
        let mut latest = self
            .lifecycle_latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(
            latest.as_ref().map(|notice| &notice.state),
            Some(LifecycleState::Starting { .. })
        ) {
            self.publish_locked(
                &mut latest,
                Notice {
                    state: LifecycleState::Started {
                        host_generation: self.generation,
                    },
                    deadline: None,
                },
            );
            drop(latest);
            self.changed();
        }
    }

    fn publish(&self, notice: Notice<P>) {
        let mut latest = self
            .lifecycle_latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.publish_locked(&mut latest, notice);
        drop(latest);
        self.changed();
    }

    fn publish_locked(&self, latest: &mut Option<Notice<P>>, notice: Notice<P>) {
        let mut deliveries = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for delivery in deliveries.values_mut() {
            let queue = &mut delivery.pending;
            // A slow client must not grow host memory across repeated aborted upgrades.
            if queue.len() == 16 {
                queue.pop_front();
            }
            queue.push_back(notice.clone());
        }
        *latest = Some(notice);
    }
}

impl<P: ProtocolExtension, W: std::io::Write + Clone> HostConnection<P, W>
where
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: serde::Serialize,
    ClientFrame<P>: serde::de::DeserializeOwned,
{
    /// Supplies the draft lifecycle projection after Welcome.
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
        self.flush_lifecycle_at(Instant::now)
    }

    pub(in super::super) fn flush_lifecycle_at(
        &mut self,
        mut now: impl FnMut() -> Instant,
    ) -> CliResult<()> {
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
                let remaining = notice.remaining(now());
                output(&mut self.out, notice.state, remaining)?;
            } else if let LifecycleState::Transition(legacy) = notice.state {
                write_frame(&mut self.out, &HostFrame::<P>::from(legacy))?;
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
