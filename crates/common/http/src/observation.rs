//! Explicit, caller-owned physical exchange observation without header access.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::{ConnectionUse, ErrorKind, Failure, Phase, TransmissionFact};

/// Opt-in diagnostic observer. Receives body bytes only, never HTTP headers.
///
/// Implementations must be fast, bounded, and must not panic,
/// including in destructors. Synchronous callback time contributes to exchange
/// deadlines; instrumented exchanges do not establish timing or performance
/// equivalence. Body data can contain private conversation data;
/// storage, redaction and access control remain the caller's responsibility.
/// Callbacks cannot authorize retries or change transport policy. Unwinding
/// callback panics disable passive observation; required observation instead
/// fails the exchange. The process panic hook still runs.
pub trait ExchangeObserver: Send + Sync {
    /// Refuses unobserved dispatch and aborts delivery if the sink fails.
    /// Passive observers leave this false. This never authorizes a retry.
    fn required(&self) -> bool {
        false
    }

    /// Starts one observation before request validation or physical dispatch.
    /// Return `None` to decline this exchange (or refuse required capture).
    /// No bytes imply transmission.
    fn start(&self, request_body: &[u8]) -> Option<Box<dyn ExchangeObservation>>;
}

/// One exchange's borrowed-byte diagnostic sink, owned through body consumption.
pub trait ExchangeObservation: Send {
    /// Reports a sticky sink failure after any callback, including terminal.
    fn failed(&self) -> bool {
        false
    }

    /// Records whether the exchange ran on a fresh, reused or stale-replaced
    /// connection, just before its response status.
    fn connection(&mut self, _connection: ConnectionUse) {}

    /// Records a validated response status, without headers.
    fn response_head(&mut self, status: u16);
    /// Records bytes yielded to the consumer, in order.
    fn response_chunk(&mut self, bytes: &[u8]);
    /// Records the single observation terminal. A callback panic disables it.
    fn terminal(&mut self, terminal: &ExchangeTerminal);
}

/// Physical observation completion, never a provider/task success verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExchangeTerminal {
    /// Consumer reached validated EOF; connection work was joined, or a
    /// pooled client kept the connection idle.
    Complete,
    /// Consumer validated its protocol terminal, then closed and joined work
    /// (or, on a pooled client that saw EOF, kept the connection idle).
    /// This is a caller assertion, not transport proof of task success or EOF.
    ProtocolComplete,
    /// Consumer explicitly closed without reaching EOF.
    ClosedEarly,
    /// Exchange future or unconsumed body was dropped.
    Abandoned,
    /// Transport returned a redacted physical failure.
    Failed(Failure),
}

#[derive(Default)]
pub(crate) struct Observation {
    sink: Option<Box<dyn ExchangeObservation>>,
    required: bool,
    failed: bool,
}

impl Observation {
    pub(crate) fn start(
        observer: Option<&Arc<dyn ExchangeObserver>>,
        body: &[u8],
    ) -> Result<Self, Failure> {
        let Some(observer) = observer else {
            return Ok(Self::default());
        };
        let required = catch_unwind(AssertUnwindSafe(|| observer.required())).map_err(|_| {
            Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                ErrorKind::Internal,
            )
        })?;
        let sink = catch_unwind(AssertUnwindSafe(|| observer.start(body)))
            .ok()
            .flatten();
        if required && sink.is_none() {
            return Err(Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                ErrorKind::Internal,
            ));
        }
        if sink.is_none() {
            return Ok(Self::default());
        }
        let mut observation = Self {
            sink,
            required,
            failed: false,
        };
        if !observation.notify(|_| {}) {
            return Err(Failure::new(
                Phase::RequestWrite,
                TransmissionFact::NotStarted,
                ErrorKind::Internal,
            ));
        }
        Ok(observation)
    }

    fn notify(&mut self, callback: impl FnOnce(&mut dyn ExchangeObservation)) -> bool {
        if let Some(mut observer) = self.sink.take() {
            match catch_unwind(AssertUnwindSafe(|| {
                callback(observer.as_mut());
                observer.failed()
            })) {
                Ok(failed) => {
                    self.failed |= failed;
                    self.sink = Some(observer);
                }
                Err(_) => self.failed = true,
            }
        }
        !self.required || !self.failed
    }

    pub(crate) fn head(&mut self, status: u16, connection: Option<ConnectionUse>) -> bool {
        self.notify(|observer| {
            if let Some(connection) = connection {
                observer.connection(connection);
            }
            observer.response_head(status);
        })
    }

    pub(crate) fn chunk(&mut self, bytes: &[u8]) -> bool {
        self.notify(|observer| observer.response_chunk(bytes))
    }

    pub(crate) fn finish(&mut self, terminal: ExchangeTerminal) -> bool {
        let healthy = self.notify(|observer| observer.terminal(&terminal));
        self.sink = None;
        healthy
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.finish(ExchangeTerminal::Abandoned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Panics {
        calls: Arc<AtomicUsize>,
        at: usize,
    }

    impl ExchangeObservation for Panics {
        fn response_head(&mut self, _: u16) {
            self.called(0);
        }
        fn response_chunk(&mut self, _: &[u8]) {
            self.called(1);
        }
        fn terminal(&mut self, _: &ExchangeTerminal) {
            self.called(2);
        }
    }

    impl Panics {
        fn called(&self, phase: usize) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_ne!(phase, self.at, "synthetic observer callback panic");
        }
    }

    #[test]
    fn callback_panics_disable_remaining_observation() {
        for at in 0..3 {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut observation = Observation {
                sink: Some(Box::new(Panics {
                    calls: Arc::clone(&calls),
                    at,
                })),
                required: false,
                failed: false,
            };
            observation.head(200, Some(ConnectionUse::Fresh));
            observation.chunk(b"body");
            observation.finish(ExchangeTerminal::Complete);
            drop(observation);
            assert_eq!(calls.load(Ordering::SeqCst), at + 1);
        }
    }
}
