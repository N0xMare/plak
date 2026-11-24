//! Ordinary turn preparation while its exact gateway socket is between holders.

use super::*;
use crate::gateway::GatewayEndpoint;
use crate::subscriptions::NativeAuthStatus;
use crate::subscriptions::state::tests::child_catalog::{claude_store, restore_claude_fixture};
use plakous_agent::{
    Agent, AgentBuilder, AgentEventKind, AgentRequestPolicy, RequestSlots, Turn, TurnResultOutcome,
};
use plakous_auth::{CredentialStatus, SecretBytes};
use plakous_http::relay_server::{Peer, RelayServer};
use plakous_model::{
    FixedClock, GenerationLimits, ModelErrorKind, ModelErrorPhase, ReasoningPolicy,
};
use plakous_tools::{ToolRegistryBuilder, WorkspacePolicy};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout};

struct Socket {
    tasks: JoinSet<()>,
    connections: Arc<AtomicUsize>,
}

impl Socket {
    fn bind(
        path: &Path,
        server: RelayServer<InProcessGateway>,
        same_user: bool,
    ) -> TestResult<Self> {
        Self::bind_with_gap(path, server, same_user, None)
    }

    fn bind_with_gap(
        path: &Path,
        server: RelayServer<InProcessGateway>,
        same_user: bool,
        unlink_on_connection: Option<usize>,
    ) -> TestResult<Self> {
        let listener = UnixListener::bind(path)?;
        let path = path.to_path_buf();
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&connections);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut calls = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let Ok((io, _)) = connection else { break };
                        let connection_number = accepted.fetch_add(1, Ordering::Relaxed) + 1;
                        if unlink_on_connection == Some(connection_number)
                            && std::fs::remove_file(&path).is_err()
                        {
                            break;
                        }
                        let server = server.clone();
                        calls.spawn(async move {
                            let _ = server.serve(io, Peer::Unix { same_user }).await;
                        });
                    }
                    _ = calls.join_next(), if !calls.is_empty() => {}
                }
            }
        });
        Ok(Self { tasks, connections })
    }

    async fn stop(mut self) {
        self.tasks.shutdown().await;
    }
}

struct Swap {
    agent: Agent,
    upstream: Upstream,
    socket_path: PathBuf,
    destination: RelayServer<InProcessGateway>,
    slots: RequestSlots,
    paths: SubscriptionPaths,
    _workspace: tempfile::TempDir,
}

impl Swap {
    fn assert_no_upstream_requests(&self) {
        let count = self.upstream.take().len();
        assert_eq!(count, 0);
    }

    async fn new() -> TestResult<Self> {
        let workspace = tempfile::tempdir()?;
        let socket_path = workspace.path().join("gateway.sock");
        let paths = SubscriptionPaths::from_xdg_state_home(workspace.path())?;
        seed_claude(&paths, "fixture-access", "fixture-refresh")?;
        let upstream = upstream_with(Arc::new(|request: &[u8]| {
            if request.starts_with(b"POST /v1/messages?beta=true ") {
                return (
                    "200 OK",
                    "text/event-stream\r\nrequest-id: req_gateway_fixture",
                    SSE.as_bytes().to_vec(),
                );
            }
            let (status, body) = fixture_reply(request);
            (status, "application/json", body)
        }))
        .await?;
        let incumbent = Socket::bind(&socket_path, gateway_server(&paths, &upstream)?, true)?;
        let gateway =
            GatewayCustody::connect(GatewayEndpoint::Unix(socket_path.clone()), None, |kind| {
                (kind == "claude").then_some(CLAUDE_CREDENTIAL_HEADERS)
            })?;
        let provider = subscriptions(&paths)?
            .with_custody(&gateway.custody())
            .resolve_turn_scoped_provider_for_attach(claude_selection(MODEL)?, limits()?)
            .await?;
        let clock = Arc::new(FixedClock::new(provider.initial_authority().observed_at()));
        let slots = RequestSlots::new("fixture", NonZeroUsize::MIN);
        let agent = AgentBuilder::new(
            provider.configured_model(),
            ToolRegistryBuilder::default().freeze(),
            Arc::new(WorkspacePolicy::host(workspace.path(), ".")?),
        )
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(
                NonZeroU32::new(plakous_anthropic::claude_max_output_tokens(MODEL)),
                Vec::new(),
            )?,
            ReasoningPolicy::Disabled,
        ))
        .clock(clock)
        .request_slots(slots.clone())
        .build()?;
        incumbent.stop().await;
        let destination = gateway_server(&paths, &upstream)?;
        Ok(Self {
            agent,
            upstream,
            socket_path,
            destination,
            slots,
            paths,
            _workspace: workspace,
        })
    }

    async fn start_round(&self) -> TestResult<Turn> {
        let mut turn = self.agent.prompt("fixture prompt").await?;
        wait_for_event(&mut turn, |event| {
            matches!(event, AgentEventKind::ModelStarted)
        })
        .await?;
        Ok(turn)
    }

    fn assert_completed_once(&self, result: &plakous_agent::TurnResult) -> TestResult {
        let response = result.response().ok_or_else(|| {
            result
                .error()
                .map_or("missing response", plakous_agent::AgentError::message)
        })?;
        assert_eq!(response.output_text(), "ok");
        let requests = self.upstream.take();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(b"POST /v1/messages?beta=true "));
        Ok(())
    }
}

async fn wait_for_event(turn: &mut Turn, wanted: impl Fn(&AgentEventKind) -> bool) -> TestResult {
    let mut events = turn.take_events()?;
    loop {
        if wanted(events.recv().await?.kind()) {
            return Ok(());
        }
    }
}

#[tokio::test]
async fn a_turn_crosses_a_gateway_swap_with_one_dispatch() -> TestResult {
    let swap = Swap::new().await?;
    tokio::time::pause();
    // The closed incumbent leaves a stale socket (connection refused).
    let turn = swap.start_round().await?;
    let mut result = Box::pin(turn.result());
    tokio::select! {
        ended = &mut result => return Err(format!("turn ended during gateway swap: {:?}", ended?.error()).into()),
        () = sleep(Duration::from_secs(15)) => {}
    }
    swap.assert_no_upstream_requests();
    std::fs::remove_file(&swap.socket_path)?;
    let successor = Socket::bind(&swap.socket_path, swap.destination.clone(), true)?;
    tokio::time::resume();

    let result = result.await?;
    swap.assert_completed_once(&result)?;
    swap.agent.shutdown().await?;
    successor.stop().await;
    Ok(())
}

#[tokio::test]
async fn final_authority_releases_the_slot_then_dispatches_once() -> TestResult {
    let swap = Swap::new().await?;
    std::fs::remove_file(&swap.socket_path)?;
    // Preparation uses Facts then Refresh; the second connection can still
    // answer, but the final check's new dial has no listener.
    let incumbent =
        Socket::bind_with_gap(&swap.socket_path, swap.destination.clone(), true, Some(2))?;
    let mut turn = swap.agent.prompt("fixture prompt").await?;
    timeout(
        Duration::from_secs(5),
        wait_for_event(&mut turn, |event| {
            matches!(event, AgentEventKind::RequestPrepared { .. })
        }),
    )
    .await??;
    assert_eq!(incumbent.connections.load(Ordering::Relaxed), 2);
    tokio::time::pause();
    let mut result = Box::pin(turn.result());
    tokio::select! {
        ended = &mut result => return Err(format!("final check ended during the gap: {:?}", ended?.error()).into()),
        () = sleep(Duration::from_secs(1)) => {}
    }
    assert_eq!(swap.slots.in_flight(), 0);
    swap.assert_no_upstream_requests();
    let successor = Socket::bind(&swap.socket_path, swap.destination.clone(), true)?;
    tokio::time::resume();
    swap.assert_completed_once(&result.await?)?;
    swap.agent.shutdown().await?;
    successor.stop().await;
    incumbent.stop().await;
    Ok(())
}

#[tokio::test]
async fn a_revoked_gateway_credential_parks_until_the_same_login_is_ready() -> TestResult {
    let swap = Swap::new().await?;
    std::fs::remove_file(&swap.socket_path)?;
    let holder = Socket::bind(&swap.socket_path, swap.destination.clone(), true)?;
    let store = claude_store(&swap.paths)?;
    let payload = store
        .inspect_payload(|payload| SecretBytes::new(payload.expose_secret().to_vec()))?
        .ok_or("missing fixture payload")?;
    let local = subscriptions(&swap.paths)?;
    assert_eq!(
        local.logout_local(PROVIDER).await?,
        NativeAuthStatus::Revoked
    );
    assert_eq!(
        local.status(PROVIDER).await?.credential(),
        NativeAuthStatus::Revoked
    );
    let revoked = store.load()?.ok_or("missing fixture tombstone")?;
    assert_eq!(revoked.status(), CredentialStatus::Revoked);

    let mut turn = swap.agent.prompt("fixture prompt").await?;
    timeout(
        Duration::from_secs(5),
        wait_for_event(&mut turn, |event| {
            matches!(event, AgentEventKind::ModelCredentialWait { waiting: true })
        }),
    )
    .await??;
    assert_eq!(swap.slots.in_flight(), 0);
    swap.assert_no_upstream_requests();
    {
        let lock = local.acquire_compatibility_lock(PROVIDER).await?;
        restore_claude_fixture(&swap.paths, &revoked, payload)?;
        lock.verify()?;
    }
    assert_eq!(
        local.status(PROVIDER).await?.credential(),
        NativeAuthStatus::Ready
    );

    swap.assert_completed_once(&timeout(Duration::from_secs(10), turn.result()).await??)?;
    swap.agent.shutdown().await?;
    holder.stop().await;
    Ok(())
}

#[tokio::test]
async fn an_absent_gateway_exhausts_the_preparation_wait() -> TestResult {
    let swap = Swap::new().await?;
    std::fs::remove_file(&swap.socket_path)?;
    tokio::time::pause();
    let started = Instant::now();
    let result = swap.start_round().await?.result().await?;
    let error = result.error().ok_or("missing preparation failure")?;
    assert_eq!(
        error.model_kind(),
        Some(ModelErrorKind::Unavailable),
        "{}",
        error.message()
    );
    assert_eq!(
        error
            .model_context()
            .ok_or("missing model context")?
            .phase(),
        ModelErrorPhase::Preparation
    );
    assert!((Duration::from_secs(60)..Duration::from_secs(61)).contains(&started.elapsed()));
    swap.assert_no_upstream_requests();
    swap.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn an_authorization_refusal_does_not_retry() -> TestResult {
    let swap = Swap::new().await?;
    std::fs::remove_file(&swap.socket_path)?;
    let denying = Socket::bind(&swap.socket_path, swap.destination.clone(), false)?;
    let result = swap.start_round().await?.result().await?;
    let error = result.error().ok_or("missing authorization failure")?;
    assert_eq!(error.model_kind(), Some(ModelErrorKind::Authorization));
    assert_eq!(
        error
            .model_context()
            .ok_or("missing model context")?
            .phase(),
        ModelErrorPhase::Preparation
    );
    assert_eq!(denying.connections.load(Ordering::Relaxed), 1);
    swap.assert_no_upstream_requests();
    swap.agent.shutdown().await?;
    denying.stop().await;
    Ok(())
}

#[tokio::test]
async fn cancelling_during_the_gateway_wait_never_dispatches() -> TestResult {
    let swap = Swap::new().await?;
    tokio::time::pause();
    let turn = swap.start_round().await?;
    let control = turn.control();
    let mut result = Box::pin(turn.result());
    tokio::select! {
        ended = &mut result => return Err(format!("turn ended before cancellation: {:?}", ended?.error()).into()),
        () = sleep(Duration::from_secs(1)) => {}
    }
    let cancelled = Instant::now();
    control.cancel().await?;
    assert!(matches!(
        result.await?.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    assert!(cancelled.elapsed() < Duration::from_millis(250));
    swap.assert_no_upstream_requests();
    swap.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shutting_down_during_the_gateway_wait_never_dispatches() -> TestResult {
    let swap = Swap::new().await?;
    tokio::time::pause();
    let mut result = Box::pin(swap.start_round().await?.result());
    tokio::select! {
        ended = &mut result => return Err(format!("turn ended before shutdown: {:?}", ended?.error()).into()),
        () = sleep(Duration::from_secs(1)) => {}
    }
    assert_eq!(swap.slots.in_flight(), 0);
    timeout(Duration::from_secs(1), swap.agent.shutdown()).await??;
    assert!(matches!(
        result.await?.outcome(),
        TurnResultOutcome::Cancelled(_)
    ));
    swap.assert_no_upstream_requests();
    Ok(())
}
