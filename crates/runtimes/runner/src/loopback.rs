//! A runner and a host joined in process: the runner end over a local
//! execution environment, the host end as a [`RunnerBackend`], and a link
//! between them that can be severed, reconnected or restarted.

use std::fmt;
use std::sync::Arc;

use plakous_runner_protocol::{Bind, Hello};
use plakous_tools::ExecutionEnvironmentError;
use plakous_tools::internal::runner::RunnerBackend;
use tokio::task::AbortHandle;

use crate::client::{ClientWindows, ConnectError, RunnerClient};
use crate::service::RunnerService;

const LINK_BUFFER: usize = 64 * 1024;

/// Why a loopback could not be joined.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoopbackError {
    /// The host end refused the runner.
    Connect(ConnectError),
    /// The runner's descriptor is not one a runner backend presents.
    Backend(ExecutionEnvironmentError),
}

impl fmt::Display for LoopbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(formatter, "loopback connect failed: {error}"),
            Self::Backend(error) => write!(formatter, "loopback backend refused: {error}"),
        }
    }
}

impl std::error::Error for LoopbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(error) => Some(error),
            Self::Backend(error) => Some(error),
        }
    }
}

/// One runner and one host over an in-memory link.
pub struct Loopback {
    service: RunnerService,
    client: RunnerClient,
    backend: Arc<RunnerBackend>,
    link: AbortHandle,
}

impl Loopback {
    /// Starts serving `service` and binds a host to it as `bind`.
    ///
    /// # Errors
    ///
    /// Refuses a runner the host cannot bind or present.
    pub async fn join(
        service: RunnerService,
        bind: Bind,
        windows: ClientWindows,
    ) -> Result<Self, LoopbackError> {
        let (host, link) = connect(&service);
        let (client, hello) = RunnerClient::connect(host, bind, windows)
            .await
            .map_err(LoopbackError::Connect)?;
        let link_client: Arc<RunnerClient> = Arc::new(client.clone());
        let backend =
            RunnerBackend::new(hello.descriptor, link_client).map_err(LoopbackError::Backend)?;
        Ok(Self {
            service,
            client,
            backend: Arc::new(backend),
            link,
        })
    }

    /// The host's backend for this runner.
    #[must_use]
    pub fn backend(&self) -> Arc<RunnerBackend> {
        Arc::clone(&self.backend)
    }

    /// The runner instance currently linked.
    #[must_use]
    pub const fn service(&self) -> &RunnerService {
        &self.service
    }

    /// Drops the link: both ends see the stream end. Calls in flight wait for
    /// a reconnection or their window.
    pub fn sever(&self) {
        self.link.abort();
    }

    /// Severs the link and binds a new one to the same runner instance.
    ///
    /// # Errors
    ///
    /// As [`RunnerClient::reconnect`].
    pub async fn reconnect(&mut self) -> Result<Hello, ConnectError> {
        self.sever();
        let (host, link) = connect(&self.service);
        self.link = link;
        self.client.reconnect(host).await
    }

    /// Severs the link and binds the host to `service`, a new runner
    /// instance: a runner restart.
    ///
    /// # Errors
    ///
    /// As [`RunnerClient::reconnect`].
    pub async fn restart(&mut self, service: RunnerService) -> Result<Hello, ConnectError> {
        self.sever();
        self.service = service;
        let (host, link) = connect(&self.service);
        self.link = link;
        self.client.reconnect(host).await
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.link.abort();
    }
}

/// A host stream served by `service` through a relay whose abort severs it.
fn connect(service: &RunnerService) -> (tokio::io::DuplexStream, AbortHandle) {
    let (host, mut host_side) = tokio::io::duplex(LINK_BUFFER);
    let (runner, mut runner_side) = tokio::io::duplex(LINK_BUFFER);
    let service = service.clone();
    tokio::spawn(async move { service.serve_connection(runner).await });
    let relay = tokio::spawn(async move {
        let _ = tokio::io::copy_bidirectional(&mut host_side, &mut runner_side).await;
    });
    (host, relay.abort_handle())
}
