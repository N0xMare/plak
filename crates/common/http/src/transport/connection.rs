//! Connection establishment, direct-connection handles and task guards.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use hyper::client::conn::{http1, http2};
use hyper::rt::Executor;
use hyper_util::rt::TokioIo;
use tokio::task::{JoinError, JoinHandle};

use crate::{ErrorKind, ExchangeControl, Failure, Phase, TransmissionFact, WireProtocol};

use super::{
    ConnectionSettings, ConnectionState, ConnectionTask, DirectConnection, Limits, ProgressSlot,
    RequestIo, Sender, TaskGuard, TaskRegistry, TrackedExecutor, WireStream,
    abort_direct_connection, phase_deadline,
};

impl Sender {
    pub(super) fn is_ready(&self) -> bool {
        match self {
            Self::Http1(sender) => sender.is_ready(),
            Self::Http2(sender) => sender.is_ready(),
        }
    }

    pub(super) async fn ready(&mut self) -> bool {
        match self {
            Self::Http1(sender) => sender.ready().await.is_ok(),
            Self::Http2(sender) => sender.ready().await.is_ok(),
        }
    }
}

pub(super) async fn establish(
    stream: WireStream,
    protocol: WireProtocol,
    limits: &Limits,
    slot: Arc<ProgressSlot>,
) -> Result<ConnectionTask, Failure> {
    let io = TokioIo::new(RequestIo { stream, slot });
    let settings = ConnectionSettings::new(limits);
    let internal = || {
        Failure::new(
            Phase::Tls,
            TransmissionFact::NotStarted,
            ErrorKind::Internal,
        )
    };
    let handshake_failure = |error: hyper::Error| {
        Failure::new(
            Phase::Tls,
            TransmissionFact::NotStarted,
            ErrorKind::Protocol,
        )
        .with_protocol_cause(&error)
    };
    if protocol == WireProtocol::Http2 {
        let registry = TaskRegistry::new();
        let mut builder = http2::Builder::new(TrackedExecutor {
            registry: Arc::clone(&registry),
        });
        let receive_window = u32::try_from(settings.receive_window).map_err(|_| internal())?;
        builder
            .max_header_list_size(
                u32::try_from(settings.max_response_header_bytes).map_err(|_| internal())?,
            )
            .initial_stream_window_size(receive_window)
            .initial_connection_window_size(receive_window);
        let (sender, connection) = builder.handshake(io).await.map_err(handshake_failure)?;
        let mut connection = ConnectionTask::new(tokio::spawn(connection), Some(registry));
        connection.sender = Some(Sender::Http2(sender));
        Ok(connection)
    } else {
        let mut builder = http1::Builder::new();
        builder
            .max_headers(settings.max_response_headers)
            .max_buf_size(settings.max_response_header_bytes);
        let (sender, connection) = builder.handshake(io).await.map_err(handshake_failure)?;
        let mut connection = ConnectionTask::new(tokio::spawn(connection), None);
        connection.sender = Some(Sender::Http1(sender));
        Ok(connection)
    }
}

impl DirectConnection {
    pub(super) fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    pub(super) async fn abort_and_join(mut self) -> Result<(), Failure> {
        abort_direct_connection(self.handle.take(), self.registry.take()).await
    }
}

impl Drop for DirectConnection {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if let Some(registry) = self.registry.take() {
            registry.abort_all();
        }
    }
}

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        // A direct driver aborts itself as it drops.
        if let Some(ConnectionState::Supervised { stop, .. }) = &self.state {
            stop.cancel();
        }
    }
}

pub(super) const fn cleanup_failure() -> Failure {
    Failure::new(
        Phase::Cleanup,
        TransmissionFact::MayHaveTransmitted,
        ErrorKind::Internal,
    )
}

impl<F> Executor<F> for TrackedExecutor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, future: F) {
        self.registry.spawn(future);
    }
}

impl<T> TaskGuard<T> {
    pub(super) const fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

pub(super) async fn wait_task<T>(
    mut task: TaskGuard<Result<T, Failure>>,
    control: &ExchangeControl,
    timeout: Duration,
    phase: Phase,
    transmission: TransmissionFact,
) -> Result<T, Failure> {
    let deadline = phase_deadline(control.deadline(), timeout);
    let cancellation = control.cancellation().clone();
    let handle = task
        .handle
        .as_mut()
        .ok_or_else(|| Failure::new(phase, transmission, ErrorKind::Internal))?;
    let result = tokio::select! {
        () = cancellation.cancelled() => {
            handle.abort();
            let _ = handle.await;
            return Err(Failure::new(phase, transmission, ErrorKind::Cancelled));
        }
        () = tokio::time::sleep_until(deadline) => {
            handle.abort();
            let _ = handle.await;
            return Err(Failure::new(
                phase,
                transmission,
                ErrorKind::DeadlineExceeded,
            ));
        }
        result = &mut *handle => result,
    };
    task.handle.take();
    result.map_err(|error| join_failure(&error, phase, transmission))?
}

fn join_failure(error: &JoinError, phase: Phase, transmission: TransmissionFact) -> Failure {
    let kind = if error.is_cancelled() {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Internal
    };
    Failure::new(phase, transmission, kind)
}
