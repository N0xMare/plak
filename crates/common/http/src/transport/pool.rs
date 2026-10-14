//! Idle connections a pooled [`Client`](super::Client) keeps for reuse.
//!
//! The pool hands a connection to one exchange at a time: a checked-out
//! connection is owned by that exchange until it ends, and only a cleanly
//! finished exchange checks it back in. Nothing here runs in the background;
//! expired or closed connections are dropped, which aborts their tasks, when a
//! later checkout finds them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{ConnectionTask, Limits, ProgressSlot, TransportPolicy};
use crate::WireProtocol;

/// An idle connection this old is closed instead of reused. It assumes the
/// inference front ends keep an idle connection open for longer, about 60 s
/// or more, as common proxy defaults do. That is an assumption, not a timeout
/// any provider guarantees. Under it a connection is rarely offered after its
/// server already decided to close it.
pub(super) const IDLE_BOUND: Duration = Duration::from_secs(5);

/// Idle connections kept per key. One agent runs one exchange at a time per
/// route; a second covers a brief overlap such as a title request.
pub(super) const MAX_IDLE_PER_KEY: usize = 2;

/// Everything that decides whether one connection may carry another request.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct PoolKey {
    pub(super) host: String,
    pub(super) port: u16,
    /// Address of the clients' shared Rustls configuration. Every client that
    /// shares a pool holds that same `Arc`, so the address cannot be reused
    /// by another configuration while the pool exists.
    pub(super) tls_config: usize,
    pub(super) policy: TransportPolicy,
    pub(super) settings: ConnectionSettings,
}

/// Limits fixed into a connection when it is negotiated.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct ConnectionSettings {
    pub(super) max_response_headers: usize,
    pub(super) max_response_header_bytes: usize,
    pub(super) receive_window: usize,
}

impl ConnectionSettings {
    pub(super) fn new(limits: &Limits) -> Self {
        Self {
            max_response_headers: limits.max_response_headers,
            max_response_header_bytes: limits.max_response_header_bytes,
            receive_window: limits
                .max_response_body_bytes
                .min(limits.max_response_chunk_bytes)
                .clamp(1, 1024 * 1_024),
        }
    }
}

pub(super) struct IdleConnection {
    pub(super) connection: ConnectionTask,
    pub(super) slot: Arc<ProgressSlot>,
    pub(super) protocol: WireProtocol,
    pub(super) since: Instant,
}

impl IdleConnection {
    fn usable(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) < IDLE_BOUND && self.connection.is_ready()
    }
}

#[derive(Default)]
pub(super) struct Pool {
    idle: Mutex<HashMap<PoolKey, Vec<IdleConnection>>>,
    /// Hands out idle connections without checking them, so a test can
    /// reproduce the race with a server closing one after checkout.
    #[cfg(test)]
    offer_unchecked: std::sync::atomic::AtomicBool,
}

impl Pool {
    /// Takes the most recently used idle connection that is still open and
    /// inside the idle bound, and drops every other expired or closed one.
    pub(super) fn checkout(&self, key: &PoolKey) -> Option<(IdleConnection, Duration)> {
        let now = Instant::now();
        let mut evicted = Vec::new();
        let found = {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            let entries = idle.get_mut(key)?;
            let mut found = None;
            for entry in std::mem::take(entries) {
                if !self.offers(&entry, now) {
                    evicted.push(entry);
                } else if let Some(older) = found.replace(entry) {
                    entries.push(older);
                }
            }
            if entries.is_empty() {
                idle.remove(key);
            }
            found
        };
        drop(evicted);
        found.map(|entry| {
            let age = now.saturating_duration_since(entry.since);
            (entry, age)
        })
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self, reason = "tests can make the pool skip the check")]
    fn offers(&self, entry: &IdleConnection, now: Instant) -> bool {
        entry.usable(now)
    }

    #[cfg(test)]
    fn offers(&self, entry: &IdleConnection, now: Instant) -> bool {
        self.offer_unchecked
            .load(std::sync::atomic::Ordering::Acquire)
            || entry.usable(now)
    }

    /// Keeps a cleanly finished connection, dropping the oldest beyond the cap.
    pub(super) fn checkin(&self, key: PoolKey, connection: IdleConnection) {
        let evicted = {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            let entries = idle.entry(key).or_default();
            entries.push(connection);
            (entries.len() > MAX_IDLE_PER_KEY).then(|| entries.remove(0))
        };
        drop(evicted);
    }

    /// Idle connections that are still open and inside the idle bound.
    #[cfg(test)]
    pub(super) fn usable_count(&self) -> usize {
        let now = Instant::now();
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .flatten()
            .filter(|entry| entry.usable(now))
            .count()
    }

    #[cfg(test)]
    pub(super) fn offer_unchecked(&self) {
        self.offer_unchecked
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn idle_count(&self) -> usize {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(Vec::len)
            .sum()
    }

    /// Ages every idle connection as if it had sat idle for `by` longer.
    #[cfg(test)]
    pub(super) fn age(&self, by: Duration) {
        for entries in self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            for entry in entries {
                entry.since = entry.since.checked_sub(by).unwrap_or(entry.since);
            }
        }
    }
}

#[cfg(test)]
mod tests;
