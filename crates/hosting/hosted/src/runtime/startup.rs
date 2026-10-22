//! Host startup: worker spawn, lifecycle phases, and session opening.
use std::future::Future;
use std::thread;
use std::time::{Duration, Instant};

use plakous::application::ApplicationExecutor;
use plakous::model::{
    Clock, Digest, EffectiveProviderConfigurationId, RuntimeModelAuthorityError,
    SelectedProviderConfigurationId, SessionLineage, SystemClock,
};
use plakous::sessions::SessionRuntimeBinding;
#[cfg(any(test, feature = "test-support"))]
use plakous::sessions::{SessionOperationId, SessionTargetBinding};
use plakous::subscriptions::{NativeProvider, ResolvedProvider};

#[cfg(any(test, feature = "test-support"))]
use crate::agent::build_or_open_injected_session;
use crate::agent::{
    AttachedCodingSession, ProductAgentSpec, SessionToolFence, api_session_runtime_binding,
    api_topology_parent, coding_session_runtime_binding, open_api_coding_session_with_fence,
    open_coding_session,
};
use crate::binding::TargetSelection;
use crate::sessions::SessionRecord;
use crate::{CliFailure, CliResult};

use super::host_loop::run_host_loop;
use super::info::plan_state_authority;
use super::models::effort_from_record;
use super::rebind::durable_tree_id;
use super::{
    HostConfig, HostEvent, HostLifecyclePhase, HostTargetAuthority, OpenRouterHostEvidence, Sender,
    ShellHostConfig, TestRuntimeRole, TuiHostMode, TuiHostPreparation,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostInjection, TestHostWorkflow, UserAction};
use crate::sessions::{NoticeScope, TranscriptRow};

impl HostLifecyclePhase {
    pub(super) const fn status(self) -> &'static str {
        match self {
            Self::ClearResolveAuthority
            | Self::ClearBuildCandidate
            | Self::ClearPublishCandidate
            | Self::ClearActivate => "clear",
            Self::RebindResolveTarget
            | Self::RebindPrepareRuntime
            | Self::RebindPublishIntent
            | Self::RebindCanonicalTransition
            | Self::RebindOpenTarget
            | Self::RebindActivate => "rebind",
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::ClearResolveAuthority => "fresh authority resolved",
            Self::ClearBuildCandidate => "candidate runtime built",
            Self::ClearPublishCandidate => "candidate product record published",
            Self::ClearActivate => "candidate activated",
            Self::RebindResolveTarget => "exact target resolved",
            Self::RebindPrepareRuntime => "immutable runtime prepared",
            Self::RebindPublishIntent => "durable transition prepared",
            Self::RebindCanonicalTransition => "canonical transition completed",
            Self::RebindOpenTarget => "target runtime opened",
            Self::RebindActivate => "target activated",
        }
    }
}

pub(super) fn observe_phase<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    phase: HostLifecyclePhase,
    started: Instant,
    queue_len: usize,
) {
    let _ = events.send(HostEvent::Status {
        phase: phase.status().into(),
        queue_len,
        detail: Some(format!(
            "{} · cumulative elapsed {}ms",
            phase.label(),
            started.elapsed().as_millis()
        )),
    });
}

pub(super) async fn bounded_transition_stage<T>(
    timeout: Duration,
    stage: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::time::timeout(timeout, stage).await.map_err(|_| ())
}

#[cfg(any(test, feature = "test-support"))]
pub fn run_injected_host<P: plakous_host_protocol::ProtocolExtension>(
    mut config: HostConfig<P>,
    injection: std::sync::Arc<TestHostInjection>,
    actions: Vec<UserAction<P>>,
) -> CliResult<Vec<HostEvent<P>>> {
    config.test_injection = Some(injection);
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    for action in actions {
        action_tx
            .send(action)
            .map_err(|_| CliFailure::new("injected host action channel closed"))?;
    }
    let mut one_shot = config
        .attached_one_shot
        .then_some(super::oneshot::OneShotState {
            attempted: false,
            resume_tree: false,
            result: None,
            run: None,
        });
    let result = super::host_loop::run(
        config,
        &action_rx,
        Sender::over(event_tx),
        None,
        one_shot.as_mut(),
    );
    drop(action_tx);
    result?;
    if let Some(state) = one_shot {
        state
            .result
            .ok_or_else(|| CliFailure::new("injected one-shot had no terminal"))??;
    }
    let mut observations = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        observations.push(event);
    }
    Ok(observations)
}

/// Installs the injected child-binding factory for the life of one host loop.
///
/// The guard is process-global, like the topology hub the children attach to,
/// so it is returned rather than dropped here: the caller holds it for exactly
/// as long as the loop it belongs to.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub(super) fn install_injected_child_runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> Option<crate::agent::injected_child_binding::Guard> {
    let factory = config
        .test_injection
        .as_ref()
        .and_then(|injection| injection.child_runtime.clone())?;
    Some(crate::agent::injected_child_binding::install(factory))
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn injected_runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    role: TestRuntimeRole,
) -> Option<CliResult<SessionRuntimeBinding>> {
    config.test_injection.as_ref().map(|injection| {
        (injection.runtime)(role)
            .map(|runtime| {
                runtime.with_job_host(config.state_roots.job_registry().for_agent("root", "root"))
            })
            .map_err(CliFailure::new)
    })
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn injected_rebind_target<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> Option<SessionTargetBinding> {
    config
        .test_injection
        .as_ref()
        .map(|injection| injection.rebind_target.clone())
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn observe_test_boundary(
    injection: Option<&TestHostInjection>,
    workflow: TestHostWorkflow,
    boundary: TestHostBoundary,
) {
    if let Some(injection) = injection {
        (injection.observe)(workflow, boundary);
    }
}

/// Control mailbox, bounded observations, and the worker owned by one driver.
pub type HostWorker<P = plakous_host_protocol::Core> = (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    thread::JoinHandle<CliResult<()>>,
);

/// Spawn host worker thread and return channels for the TUI.
pub fn spawn_host_worker<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
) -> CliResult<HostWorker<P>> {
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
    let event_tx = Sender::over(event_tx);
    let handle = thread::spawn(move || {
        let result = run_host_loop(
            config,
            &action_rx,
            event_tx.clone(),
            Some(startup_tx.clone()),
        );
        if let Err(err) = &result {
            let _ = startup_tx.send(Err(err.message().to_owned()));
            // A notice, not an error row: the failure itself is the worker's
            // result, which every surface reports once as its terminal error.
            let _ = event_tx.notice(TranscriptRow::info(
                NoticeScope::Host,
                format!("host error: {}", err.message()),
            ));
            let _ = event_tx.send(HostEvent::Status {
                phase: "error".into(),
                queue_len: 0,
                detail: Some(err.message().to_owned()),
            });
        }
        result
    });
    await_host_startup(startup_rx, action_tx, event_rx, handle)
}

pub(super) fn await_host_startup<P: plakous_host_protocol::ProtocolExtension>(
    startup_rx: std::sync::mpsc::Receiver<Result<(), String>>,
    action_tx: crate::surface_protocol::UserActionSender<P>,
    event_rx: crate::surface_protocol::HostEventReceiver<P>,
    handle: thread::JoinHandle<CliResult<()>>,
) -> CliResult<HostWorker<P>> {
    match startup_rx.recv() {
        Ok(Ok(())) => Ok((action_tx, event_rx, handle)),
        Ok(Err(error)) => {
            drop(action_tx);
            match handle
                .join()
                .map_err(|_| CliFailure::new("plak product host failed while joining startup"))?
            {
                Ok(()) => Err(CliFailure::new(error)),
                Err(termination) => Err(termination),
            }
        }
        Err(_) => {
            drop(action_tx);
            match handle
                .join()
                .map_err(|_| CliFailure::new("plak product host panicked during startup"))?
            {
                Ok(()) => Err(CliFailure::new(
                    "plak product host ended before completing startup",
                )),
                Err(termination) => Err(termination),
            }
        }
    }
}

/// Spawn the single Plak product controller before expensive provider work.
///
/// The returned channels are immediately usable by the TUI. The controller
/// publishes activity while discovery/attachment runs, then transfers the same
/// action receiver to either the one canonical runtime host or the shell host.
pub fn spawn_deferred_tui_host<P: plakous_host_protocol::ProtocolExtension>(
    prepare: impl FnOnce() -> TuiHostPreparation<P> + Send + 'static,
) -> (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    thread::JoinHandle<CliResult<()>>,
) {
    spawn_deferred_tui_host_with_clear_publication(prepare, None)
}

pub fn spawn_deferred_tui_host_with_clear_publication<
    P: plakous_host_protocol::ProtocolExtension,
>(
    prepare: impl FnOnce() -> TuiHostPreparation<P> + Send + 'static,
    clear_publication: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) -> (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    thread::JoinHandle<CliResult<()>>,
) {
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    let event_tx = Sender::over(event_tx);
    let handle = thread::spawn(move || {
        let _ = event_tx.send(HostEvent::Activity {
            label: "discovering providers and preparing session".into(),
            active: true,
        });
        let prepared = prepare();
        let _ = event_tx.send(HostEvent::ProviderSnapshot {
            providers: prepared.providers.clone(),
        });
        if let Some(refresh) = prepared.catalog_refresh {
            refresh(event_tx.inner.clone());
        }
        let shell_host = prepared.shell_host;
        match prepared.mode {
            TuiHostMode::Attached(config) => {
                let facts = shell_host.fallback_facts();
                let mut fallback = ShellHostConfig {
                    surface: facts.surface,
                    binding: facts.binding,
                    subscriptions: facts.subscriptions,
                    openrouter_credentials: facts.openrouter_credentials,
                    product_home: config.product_home.clone(),
                    home: config.home.clone(),
                    store: std::sync::Arc::clone(&config.store),
                    session: config.session.clone(),
                    cwd: config.cwd.clone(),
                    reason: "canonical startup failed".into(),
                    ready_catalog: config.ready_catalog.clone(),
                };
                let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
                if let Err(error) =
                    run_host_loop(config, &action_rx, event_tx.clone(), Some(ready_tx))
                {
                    if matches!(ready_rx.try_recv(), Ok(Ok(()))) {
                        return Err(error);
                    }
                    fallback.reason = if error.is_recovery_required() {
                        format!("recovery_required: {}", error.message())
                    } else {
                        format!("resolve_failed: {}", error.message())
                    };
                    if let Ok(authoritative) = fallback.store.load(&fallback.session.id) {
                        fallback.session = authoritative;
                    }
                    let _ = event_tx.send(HostEvent::AgentAttached(false));
                    let _ = event_tx.notice(TranscriptRow::error(
                        NoticeScope::Host,
                        error.message().to_owned(),
                    ));
                    return shell_host.run(
                        fallback,
                        action_rx,
                        event_tx,
                        clear_publication.clone(),
                    );
                }
                Ok(())
            }
            TuiHostMode::Shell(config) => {
                let _ = event_tx.send(HostEvent::AgentAttached(false));
                let _ = event_tx.send(HostEvent::Activity {
                    label: "provider setup required".into(),
                    active: false,
                });
                shell_host.run(config, action_rx, event_tx, clear_publication)
            }
        }
    });
    (action_tx, event_rx, handle)
}

/// Creates the canonical runtime of a record that has none, seeded with the
/// carried conversation in `config.seed_history`, then releases it, binds
/// the record to the released head and saves the record. This is the whole of
/// `plak session import`: the session is then resumable and promptable like
/// any other.
///
/// # Errors
///
/// Refuses a config with no carried history or a record that already has a
/// runtime, and returns any runtime construction, release or persistence
/// failure. A failed save leaves the released runtime unreferenced. Returns
/// the saved record and the startup notes the host wrote while building it.
pub fn create_seeded_session<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
) -> CliResult<(SessionRecord, String)> {
    create_seeded_session_with_preflight(config, |_| Ok(()))
}

/// Creates a seed after a host-owned advisory capacity callback, reusing the
/// exact runtime binding that will be created. No inference is dispatched.
///
/// # Errors
/// Returns construction, callback, create, release or persistence failures.
pub fn create_seeded_session_with_preflight<P: plakous_host_protocol::ProtocolExtension>(
    mut config: HostConfig<P>,
    preflight: impl FnOnce(plakous::sessions::SessionInputWindow) -> CliResult<()> + Send + 'static,
) -> CliResult<(SessionRecord, String)> {
    if config.session.plakous_locator.is_some() {
        return Err(CliFailure::new(
            "a carried history seeds only a session that has no runtime yet",
        ));
    }
    let history = config
        .seed_history
        .take()
        .ok_or_else(|| CliFailure::new("no carried history to seed the session with"))?;
    let seed = crate::agent::SeededHistory::with_preflight(history, preflight);
    let executor = ApplicationExecutor::new().map_err(|e| CliFailure::new(e.to_string()))?;
    let mut stderr_buf = Vec::new();
    let opened = executor
        .block_on(create_session(&mut config, Some(seed), &mut stderr_buf))
        .map_err(|e| CliFailure::new(e.to_string()))??;
    let locator = executor
        .block_on(opened.release())
        .map_err(|e| CliFailure::new(e.to_string()))?
        .map_err(|e| CliFailure::new(e.to_string()))?;
    let notes = String::from_utf8_lossy(&stderr_buf).into_owned();
    config
        .session
        .bind_plakous_locator(locator)
        .map_err(CliFailure::new)?;
    config.session.touch();
    config
        .store
        .save(&mut config.session)
        .map_err(CliFailure::new)?;
    Ok((config.session, notes))
}

pub(super) async fn build_session_from_config<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<AttachedCodingSession> {
    stderr_buf.clear();
    let seed = config.seed_history.take();
    if seed.is_some() && config.session.plakous_locator.is_some() {
        return Err(CliFailure::new(
            "a carried history seeds only a session that has no runtime yet",
        ));
    }
    if config.session.plakous_locator.is_none() {
        return create_session(
            config,
            seed.map(crate::agent::SeededHistory::new),
            stderr_buf,
        )
        .await;
    }
    // Spec 02 §4.2: step 0 names the head, step 1 opens exactly it. A stale
    // refusal means another owner committed in between: step 0 runs once
    // more, and a second stale answer is lease contention.
    let (mut head, mut projection_error) = projected_head(config).await?;
    let mut retried = false;
    loop {
        match open_head(config, head, stderr_buf).await {
            Ok(session) => return Ok(session),
            Err(error) if error.current_locator.is_some() => {
                if let Some(reason) = projection_error {
                    let remedy = if reason.store_refusal().is_some() {
                        "the store temporarily refused projection; retry when it is available"
                    } else {
                        "repair the reported projector or store failure; reopening alone cannot repair deterministic projection errors"
                    };
                    return Err(CliFailure::new(format!(
                        "the recorded session head is stale and projection could not catch up: {}; {remedy}; projection debt is not a recovery fence",
                        reason.message()
                    )));
                }
                if retried {
                    return Err(CliFailure::new(format!(
                        "{}; another process is using this session",
                        error.failure.message()
                    )));
                }
                retried = true;
                (head, projection_error) = projected_head(config).await?;
            }
            Err(error) => return Err(error.failure),
        }
    }
}

/// Step 0 of the reopen: the head the projector's lease-free pass names.
async fn projected_head<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<(plakous::sessions::SessionLocator, Option<CliFailure>)> {
    let recorded = config
        .session
        .plakous_locator
        .ok_or_else(|| CliFailure::new("the session has no runtime to open"))?;
    let sessions = config.state_roots.runtime_sessions()?;
    match super::projection::project_before_open(&sessions, &config.store, &config.session).await {
        Ok(head) => Ok((head.unwrap_or(recorded), None)),
        // Debt cannot fence a current head. Keep the pass's actual reason
        // only for an exact-open stale-head refusal.
        Err(error) => Ok((recorded, Some(CliFailure::from(error)))),
    }
}

#[cfg(any(test, feature = "test-support"))]
fn injected_role<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> TestRuntimeRole {
    if config.session.account.as_deref() == Some("offline-rebind") {
        TestRuntimeRole::Rebind
    } else {
        TestRuntimeRole::Primary
    }
}

/// Step 1 of the reopen: the exact open of `head` under a fresh lease.
async fn open_head<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    head: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
) -> Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(runtime) = injected_runtime(config, injected_role(config)) {
        let operation = SessionOperationId::new(format!("plak:test-open:{}", uuid::Uuid::new_v4()))
            .map_err(|_| CliFailure::new("injected host operation identity is invalid"))?;
        return build_or_open_injected_session(
            config.state_roots.runtime_sessions()?,
            Some(head),
            None,
            operation,
            runtime?,
        )
        .await;
    }
    if config.target.is_api() {
        return Box::pin(open_api_coding_session_with_fence(
            api_parent_from_config(config, &config.session)?,
            head,
            crate::topology_runtime::TopologyHubAttach::AttachRoot,
            openrouter_cached_evidence(config),
            SessionToolFence::Full,
        ))
        .await;
    }
    refresh_native_target(config).await?;
    Box::pin(open_coding_session(native_spec(config)?, head, stderr_buf)).await
}

async fn create_session<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    seed: Option<crate::agent::SeededHistory>,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<AttachedCodingSession> {
    freeze_root_tools(config)?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(runtime) = injected_runtime(config, injected_role(config)) {
        let mut seed = seed;
        let operation = SessionOperationId::new(config.session.create_operation_id.clone())
            .map_err(|_| CliFailure::new("injected host operation identity is invalid"))?;
        let runtime = runtime?;
        if let Some(seed) = seed.as_mut() {
            seed.inspect(&runtime)?;
        }
        return build_or_open_injected_session(
            config.state_roots.runtime_sessions()?,
            None,
            seed.map(|s| s.history),
            operation,
            runtime,
        )
        .await
        .map_err(|error| error.failure);
    }
    if config.target.is_api() {
        return Box::pin(crate::agent::build_api_coding_session_seed(
            api_parent_from_config(config, &config.session)?,
            &config.session.create_operation_id,
            seed,
            crate::topology_runtime::TopologyHubAttach::AttachRoot,
            openrouter_cached_evidence(config),
        ))
        .await;
    }
    refresh_native_target(config).await?;
    let spec = native_spec(config)?;
    Box::pin(crate::agent::build_coding_session_seed(
        spec,
        &config.session.create_operation_id,
        seed,
        stderr_buf,
    ))
    .await
}

fn native_spec<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<ProductAgentSpec> {
    let resolved = config
        .target
        .native_resolved()
        .ok_or_else(|| CliFailure::new("resolved provider authority is missing"))?;
    Ok(ProductAgentSpec {
        prompt_host_facts: config.prompt_host_facts(),
        product_home: Some(config.product_home.clone()),
        tool_selection: root_tool_selection(&config.session),
        provider: config
            .target
            .native_provider()
            .ok_or_else(|| CliFailure::new("native provider authority is missing"))?,
        resolved: std::sync::Arc::clone(resolved),
        effort: config.effort,
        effort_explicit: config.effort_explicit,
        workspace_root: config.session.workspace_root.clone(),
        cwd: config.cwd.clone(),
        plan_state_authority: plan_state_authority(&*config.store, &config.session)?,
        composition: config.session.composition,
        topology_tree_id: Some(durable_tree_id(&config.topology_rows(), &config.session)),
        topology_caller_id: None,
        state_roots: config.state_roots.clone(),
        scheduled_compaction: config.compaction_enabled,
        max_model_rounds: config.max_model_rounds,
        topology_enabled: config.topology_enabled,
    })
}

/// Re-resolve only a stale Codex or xAI receipt, retaining the exact model and
/// selected login. Returns whether the attached host must reopen under the
/// fresh receipt.
pub(super) async fn refresh_native_target<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
) -> CliResult<bool> {
    let HostTargetAuthority::Native {
        provider: provider @ (NativeProvider::Codex | NativeProvider::Xai),
        resolved,
        subscriptions,
        ..
    } = &config.target
    else {
        return Ok(false);
    };
    let (label, account) = match provider {
        NativeProvider::Codex => ("Codex", "codex"),
        _ => ("xAI", "grok"),
    };
    match resolved
        .model()
        .revalidate_runtime_authority(SystemClock.now())
        .await
    {
        Ok(_) => return Ok(false),
        // The model renews its own receipt; it reports Stale past the admitted
        // catalog window and ForeignAuthority when the selection changed. Both
        // re-resolve so the binding comparison below names the cause.
        Err(RuntimeModelAuthorityError::Stale | RuntimeModelAuthorityError::ForeignAuthority) => {}
        Err(RuntimeModelAuthorityError::Unavailable) => {
            return Err(native_authority_unavailable(label, account));
        }
        Err(error) => {
            return Err(CliFailure::new(format!(
                "{label} runtime authority cannot be refreshed: {error}"
            )));
        }
    }
    let previous = resolved.as_ref();
    let selected = previous.configuration().selected().clone();
    let limits = crate::agent::product_subscription_request_limits()?;
    let fresh = subscriptions
        .resolve_turn_scoped_provider(selected, limits, SessionLineage::Root)
        .await
        .map_err(|error| {
            CliFailure::new(format!("exact {label} resolution failed: {}", error.kind))
        })?;
    if !FreshNativeBinding::from_resolved(previous)?
        .admits(&FreshNativeBinding::from_resolved(&fresh)?)
    {
        return Err(CliFailure::new(format!(
            "fresh {label} authority changed the selected login or model semantics; explicit rebind required"
        )));
    }
    let HostTargetAuthority::Native { resolved, .. } = &mut config.target else {
        unreachable!("the native target was checked above");
    };
    *resolved = std::sync::Arc::new(fresh);
    Ok(true)
}

/// A renewal that found no usable credential names the repair, as other
/// credential failures do.
pub(super) fn native_authority_unavailable(label: &str, account: &str) -> CliFailure {
    CliFailure::new(format!(
        "{label} credential is unavailable (signed out, quarantined, or a login made by an earlier build); run `plak login {account}`"
    ))
}

/// The exact semantic and account axes that may not change during freshness reopen.
#[derive(Clone, Copy)]
pub(super) struct FreshNativeBinding {
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    account: Digest,
    credential_authority: Digest,
    selection: Digest,
    generation: u64,
}

impl FreshNativeBinding {
    pub(super) fn from_resolved(provider: &ResolvedProvider) -> CliResult<Self> {
        Ok(Self {
            selected: provider.configuration().selected_identity(),
            effective: provider.configuration().effective_identity(),
            account: provider.initial_authority().account_binding(),
            credential_authority: provider.initial_authority().credential_authority_binding(),
            selection: provider
                .credential_selection_binding()
                .map_err(|_| CliFailure::new("native credential selection binding is invalid"))?,
            generation: provider.initial_authority().credential_generation(),
        })
    }

    pub(super) fn admits(&self, fresh: &Self) -> bool {
        self.selected == fresh.selected
            && self.effective == fresh.effective
            && self.account == fresh.account
            && self.credential_authority == fresh.credential_authority
            && self.selection == fresh.selection
            && self.generation <= fresh.generation
    }
}

/// Whether a reopen may adopt the head a stale-locator refusal named.
///
/// Two different things leave the recorded locator behind the durable head, and
/// only one of them is the record's fault. A process that ended between
/// releasing a turn and writing the locator forward, including one killed
/// mid-turn, left a head the record simply never caught up with, and adopting
/// it is what this exists for. A turn whose terminal publication to the record
/// failed left a head carrying a result the record never learned about, and
/// adopting that would skip the result silently; it keeps its refusal. The one
/// exception is a head that is exactly the terminal of the record's newest
/// committed root delivery: open adopts it and publishes that result before
/// anything else binds the head.
///
/// The unresolved delivery intent or session workflow is the first signal. The
/// second is the head itself: a head that a reopen would close by abandoning an
/// unfinished model request can only have got ahead the harmless way.
/// The failure a reopen reports when it could not adopt the head it was told
/// about.
///
pub(super) fn api_parent_from_config<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    session: &SessionRecord,
) -> CliResult<crate::agent::ApiTopologyParent> {
    let target = session
        .target_selection
        .api_target()
        .ok_or_else(|| CliFailure::new("API runtime target is missing"))?;
    let effort = effort_from_record(session.effort.as_deref())?;
    let mut parent = api_topology_parent(
        config.product_home.clone(),
        durable_tree_id(&config.topology_rows(), session),
        session,
        config.cwd.clone(),
        config.state_roots.clone(),
        session
            .model
            .clone()
            .ok_or_else(|| CliFailure::new("API record model binding is missing"))?,
        effort,
        target,
        plan_state_authority(&*config.store, session)?,
        config.compaction_enabled,
        config.max_model_rounds,
        config.topology_enabled,
    );
    parent.prompt_host_facts = config.prompt_host_facts();
    parent.tool_selection = root_tool_selection(session);
    Ok(parent)
}

/// The root's optional tools: the `[root] agent` template's list frozen when
/// the session was created on that template's tuple, whatever tuple it runs
/// now. A record not yet frozen has none.
pub(super) fn root_tool_selection(session: &SessionRecord) -> crate::tool_plan::NodeToolSelection {
    session
        .frozen_tools
        .as_ref()
        .map(crate::frozen_tools::FrozenRootTools::selection)
        .unwrap_or_default()
}

/// Freezes the root's optional tools into the record before its runtime is
/// first created: when the session runs the `[root] agent` template's tuple,
/// the template as configured now and the MCP tools its selection reaches,
/// listed once here; off that tuple, none for the session's life. A wildcard's omitted tool becomes a transcript notice. A
/// refusal fails the creation like any bind refusal.
fn freeze_root_tools<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
) -> CliResult<()> {
    if config.session.frozen_tools.is_some() {
        return Ok(());
    }
    let mut frozen = crate::frozen_tools::FrozenRootTools::at_creation(
        config.root_template.clone(),
        config.session.account.as_deref(),
        config.session.model.as_deref(),
    );
    let tools = frozen.selection();
    if tools.selection.mcp_servers().next().is_some() {
        let bound = crate::mcp_binding::bind_mcp_tools(
            &tools.selection,
            &tools.tool_sections,
            &config.session.workspace_root,
            config.state_roots.named_secrets(),
            None,
        )
        .map_err(CliFailure::new)?;
        for notice in bound.notices {
            config
                .session
                .record_notice(TranscriptRow::warning(NoticeScope::Settings, notice));
        }
        frozen = frozen.with_mcp_tools(bound.records);
    }
    config.session.frozen_tools = Some(frozen);
    Ok(())
}

pub(super) fn openrouter_cached_evidence<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> Option<plakous::openrouter::OpenRouterChatModelEvidence> {
    let TargetSelection::OpenRouter { wire, .. } = &config.session.target_selection else {
        return None;
    };
    match &config.target {
        HostTargetAuthority::OpenRouter {
            evidence: Some(cached),
            ..
        } if cached.matches(&config.model_id, wire.resolved) => Some(cached.evidence.clone()),
        _ => None,
    }
}

pub(super) fn api_host_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<SessionRuntimeBinding> {
    let parent = api_parent_from_config(config, &config.session)?;
    ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("application executor unavailable"))?
        .block_on(async {
            api_session_runtime_binding(
                &parent,
                true,
                SessionToolFence::Full,
                openrouter_cached_evidence(config),
            )
            .await
            .map(|prepared| prepared.runtime)
        })
        .map_err(|_| CliFailure::new("application execution failed"))?
}

pub(super) async fn api_runtime_for_record<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    record: &SessionRecord,
    opening_durable_session: bool,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    tree_id: Option<&str>,
    attached_tree: &mut Option<String>,
) -> CliResult<SessionRuntimeBinding> {
    let mut parent = api_parent_from_config(config, record)?;
    if let Some(tree_id) = tree_id {
        parent.topology_tree_id = Some(tree_id.to_owned());
    }
    Ok(
        Box::pin(crate::agent::build::api_session_runtime_binding_for(
            &parent,
            opening_durable_session,
            topology_attach,
            attached_tree,
        ))
        .await?
        .runtime,
    )
}

pub(super) fn api_compaction_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<SessionRuntimeBinding> {
    let target = config
        .session
        .target_selection
        .api_target()
        .ok_or_else(|| CliFailure::new("API runtime target is missing"))?;
    let literal_model = config.model_id.clone();
    let effort = effort_from_record(config.session.effort.as_deref())?;
    let cached_evidence = match &config.target {
        HostTargetAuthority::OpenRouter {
            evidence: Some(cached),
            ..
        } if cached.matches(&literal_model, target.protocol()) => Some(cached.evidence.clone()),
        _ => None,
    };
    let mut parent = api_topology_parent(
        config.product_home.clone(),
        durable_tree_id(&config.topology_rows(), &config.session),
        &config.session,
        config.cwd.clone(),
        config.state_roots.clone(),
        literal_model,
        effort,
        target,
        plan_state_authority(&*config.store, &config.session)?,
        false,
        config.max_model_rounds,
        config.topology_enabled,
    );
    parent.tool_selection = root_tool_selection(&config.session);
    ApplicationExecutor::new()
        .map_err(|_| CliFailure::new("application executor unavailable"))?
        .block_on(async {
            api_session_runtime_binding(&parent, true, SessionToolFence::Full, cached_evidence)
                .await?
                .runtime
                .with_compaction_output_limit()
                .map_err(|_| CliFailure::new("compaction generation policy is invalid"))
        })
        .map_err(|error| CliFailure::new(format!("application execution failed: {error}")))?
}

pub(super) async fn cached_api_runtime_for_record<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    record: &SessionRecord,
    opening_durable_session: bool,
) -> CliResult<SessionRuntimeBinding> {
    let target = record
        .target_selection
        .api_target()
        .ok_or_else(|| CliFailure::new("API runtime target is missing"))?;
    let literal_model = record
        .model
        .clone()
        .ok_or_else(|| CliFailure::new("OpenRouter model binding is missing"))?;
    let cached_evidence = match &config.target {
        HostTargetAuthority::OpenRouter {
            evidence: Some(cached),
            ..
        } if cached.matches(&literal_model, target.protocol()) => Some(cached.evidence.clone()),
        _ => None,
    };
    let effort = effort_from_record(record.effort.as_deref())?;
    let mut parent = api_topology_parent(
        config.product_home.clone(),
        durable_tree_id(&config.topology_rows(), record),
        record,
        config.cwd.clone(),
        config.state_roots.clone(),
        literal_model.clone(),
        effort,
        target,
        plan_state_authority(&*config.store, record)?,
        config.compaction_enabled,
        config.max_model_rounds,
        config.topology_enabled,
    );
    parent.tool_selection = root_tool_selection(record);
    let prepared = Box::pin(api_session_runtime_binding(
        &parent,
        opening_durable_session,
        SessionToolFence::Full,
        cached_evidence,
    ))
    .await?;
    if let HostTargetAuthority::OpenRouter { evidence, .. } = &mut config.target {
        *evidence = Some(Box::new(OpenRouterHostEvidence {
            literal_model,
            wire: target.protocol(),
            evidence: prepared
                .evidence
                .ok_or_else(|| CliFailure::new("OpenRouter runtime omitted discovery evidence"))?,
        }));
    }
    Ok(prepared.runtime)
}

pub(super) fn host_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
    role: TestRuntimeRole,
) -> CliResult<SessionRuntimeBinding> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(runtime) = injected_runtime(config, role) {
        return runtime;
    }
    if role == TestRuntimeRole::Compact && config.target.is_api() {
        return api_compaction_runtime_binding(config);
    }
    if config.target.is_api() {
        return api_host_runtime_binding(config);
    }
    let resolved = config
        .target
        .native_resolved()
        .ok_or_else(|| CliFailure::new("resolved provider authority is missing"))?;
    let spec = ProductAgentSpec {
        prompt_host_facts: config.prompt_host_facts(),
        product_home: Some(config.product_home.clone()),
        tool_selection: root_tool_selection(&config.session),
        provider: config
            .target
            .native_provider()
            .ok_or_else(|| CliFailure::new("native provider authority is missing"))?,
        resolved: std::sync::Arc::clone(resolved),
        effort: config.effort,
        effort_explicit: config.effort_explicit,
        workspace_root: config.session.workspace_root.clone(),
        cwd: config.cwd.clone(),
        plan_state_authority: plan_state_authority(&*config.store, &config.session)?,
        composition: config.session.composition,
        topology_tree_id: Some(durable_tree_id(&config.topology_rows(), &config.session)),
        topology_caller_id: None,
        state_roots: config.state_roots.clone(),
        scheduled_compaction: config.compaction_enabled,
        max_model_rounds: config.max_model_rounds,
        topology_enabled: config.topology_enabled,
    };
    native_runtime_for_record(spec, role, stderr_buf)
}

pub(super) fn native_runtime_for_record(
    spec: ProductAgentSpec,
    role: TestRuntimeRole,
    stderr: &mut dyn std::io::Write,
) -> CliResult<SessionRuntimeBinding> {
    let binding = coding_session_runtime_binding(spec, stderr)?;
    if role == TestRuntimeRole::Compact {
        binding
            .with_compaction_output_limit()
            .map_err(|_| CliFailure::new("compaction generation policy is invalid"))
    } else {
        Ok(binding)
    }
}

/// Reopens `session` (the record that owns `locator`) on the host binding.
///
/// The record is passed explicitly because `/resume` reopens a session other
/// than the attached one: the plan-state authority, workspace root, topology
/// tree and composition must come from the target, or its durable
/// `update_plan` executor id has no fresh binding and exact reopen fails.
pub(super) async fn open_session_for_host<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    session: &SessionRecord,
    locator: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
    role: TestRuntimeRole,
) -> Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(runtime) = injected_runtime(config, role) {
        let operation = SessionOperationId::new(format!("plak:test-open:{}", uuid::Uuid::new_v4()))
            .map_err(|_| CliFailure::new("injected open operation identity is invalid"))?;
        return build_or_open_injected_session(
            config.state_roots.runtime_sessions()?,
            Some(locator),
            None,
            operation,
            runtime?,
        )
        .await;
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = role;
    if session.target_selection.api_target().is_some() {
        let parent = api_parent_from_config(config, session)
            .map_err(crate::agent::CodingSessionOpenFailure::from)?;
        return Box::pin(open_api_coding_session_with_fence(
            parent,
            locator,
            crate::topology_runtime::TopologyHubAttach::AttachRoot,
            openrouter_cached_evidence(config),
            SessionToolFence::Full,
        ))
        .await;
    }
    let resolved = config
        .target
        .native_resolved()
        .ok_or_else(|| CliFailure::new("resolved provider authority is missing"))?;
    let spec = ProductAgentSpec {
        prompt_host_facts: config.prompt_host_facts(),
        product_home: Some(config.product_home.clone()),
        tool_selection: root_tool_selection(session),
        provider: config
            .target
            .native_provider()
            .ok_or_else(|| CliFailure::new("native provider authority is missing"))?,
        resolved: std::sync::Arc::clone(resolved),
        effort: config.effort,
        effort_explicit: config.effort_explicit,
        workspace_root: session.workspace_root.clone(),
        cwd: config.cwd.clone(),
        plan_state_authority: plan_state_authority(&*config.store, session)?,
        composition: session.composition,
        topology_tree_id: Some(durable_tree_id(&config.topology_rows(), session)),
        topology_caller_id: None,
        state_roots: config.state_roots.clone(),
        scheduled_compaction: config.compaction_enabled,
        max_model_rounds: config.max_model_rounds,
        topology_enabled: config.topology_enabled,
    };

    Box::pin(open_coding_session(spec, locator, stderr_buf)).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostConfig = crate::runtime::HostConfig<TestExtension>;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::clear::*;

    use crate::runtime::shell::*;

    use crate::binding::{
        OpenRouterRoutingSelection, ProtocolSelection, TargetSelection, WireBinding,
    };
    use crate::session_log::{self};
    use crate::session_store::SessionRecordStore;
    use crate::sessions::SessionRecord;
    use plakous::agent::ReasoningEffort;
    use plakous::application::ApplicationExecutor;
    use plakous::subscriptions::NativeProvider;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    fn root_template(tools: &[&str]) -> crate::binding::RootTemplateFact {
        crate::binding::RootTemplateFact {
            name: "root-agent".to_owned(),
            account: "codex".to_owned(),
            model: "offline-loopback-v1".to_owned(),
            tools: crate::tool_plan::TemplateToolSelection::parse(tools).unwrap(),
            tool_sections: std::collections::BTreeMap::from([(
                "tools.fetch_url".to_owned(),
                format!("[tools.fetch_url]\nmax_bytes = {}\n", tools.len()),
            )]),
        }
    }

    /// A wildcard's unlowerable tool is omitted at creation with one
    /// Settings notice in the transcript, and the listed tools are frozen
    /// into the record for every later bind.
    #[test]
    fn creation_freezes_listed_mcp_tools_and_records_the_omission_notice() {
        let executable = std::env::current_exe().unwrap();
        let server = executable
            .parent()
            .and_then(std::path::Path::parent)
            .unwrap()
            .join("mcp-fake-server");
        assert!(
            server.is_file(),
            "run the workspace tests so the fixture is built"
        );
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
        let mut template = root_template(&["mcp:fake"]);
        template.tool_sections = std::collections::BTreeMap::from([(
            "mcp_servers.fake".to_owned(),
            format!(
                "[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {:?}\n\
                 args = [\"--tools\", \"unlowerable\"]\nenv = {{ }}\nsecret_env = {{ }}\n\
                 startup_seconds = 20\ncall_seconds = 120\n",
                server.to_str().unwrap()
            ),
        )]);
        config.root_template = Some(template);
        freeze_root_tools(&mut config).unwrap();

        let notices = config.session.transcript.pending_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let TranscriptRow::Notice(notice) = &notices[0] else {
            panic!("not a notice: {notices:?}");
        };
        assert_eq!(notice.scope, NoticeScope::Settings);
        assert!(notice.text.contains("fake/tangled"), "{}", notice.text);
        let frozen = root_tool_selection(&config.session).mcp_tools.unwrap();
        let names = frozen
            .iter()
            .map(|record| record.mcp_name().to_owned())
            .collect::<Vec<_>>();
        assert!(names.contains(&"echo".to_owned()), "{names:?}");
        assert!(!names.contains(&"tangled".to_owned()), "{names:?}");
    }

    /// The root's tools are frozen at creation: editing the `[root] agent`
    /// template afterwards changes neither the reopened record nor its
    /// rebinds, and a new session takes the edit.
    #[test]
    fn root_tools_are_frozen_at_creation_and_an_edit_changes_new_sessions_only() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
        config.root_template = Some(root_template(&["web_search"]));
        let opened = executor
            .block_on(build_session_from_config(&mut config, &mut Vec::new()))
            .unwrap()
            .unwrap();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        config.store.save(&mut config.session).unwrap();
        executor.block_on(opened.release()).unwrap().unwrap();

        config.root_template = Some(root_template(&["fetch_url", "image_gen"]));
        let reopened = config.store.load(&config.session.id).unwrap();
        let frozen = root_tool_selection(&reopened);
        assert_eq!(
            frozen.selection,
            crate::tool_plan::TemplateToolSelection::parse(["web_search"]).unwrap()
        );
        assert_eq!(
            frozen.tool_sections["tools.fetch_url"],
            "[tools.fetch_url]\nmax_bytes = 1\n"
        );
        assert_eq!(reopened.frozen_tools.as_ref().unwrap().selection(), frozen);

        let mut fresh = SessionRecord::new(config.session.workspace_root.clone());
        fresh.account = config.session.account.clone();
        fresh.model = config.session.model.clone();
        assert!(root_tool_selection(&fresh).selection.is_empty());
        config.session = fresh;
        freeze_root_tools(&mut config).unwrap();
        assert_eq!(
            root_tool_selection(&config.session).selection,
            crate::tool_plan::TemplateToolSelection::parse(["fetch_url", "image_gen"]).unwrap()
        );

        let mut off_tuple = SessionRecord::new(config.session.workspace_root.clone());
        off_tuple.account = config.session.account.clone();
        off_tuple.model = Some("other".into());
        config.session = off_tuple;
        freeze_root_tools(&mut config).unwrap();
        assert_eq!(
            config.session.frozen_tools,
            Some(crate::frozen_tools::FrozenRootTools::new(None)),
            "a session created off the template's tuple has no tools for life"
        );
    }

    #[test]
    fn exact_native_refresh_preserves_semantics_and_account_with_monotonic_generation() {
        use plakous::model::DigestDomain;
        fn digest(domain: DigestDomain, value: &'static [u8]) -> Digest {
            Digest::sha256(domain, 5, &[value]).unwrap()
        }
        fn selected(value: &'static [u8]) -> SelectedProviderConfigurationId {
            serde_json::from_value(
                serde_json::to_value(digest(DigestDomain::SelectedProviderConfiguration, value))
                    .unwrap(),
            )
            .unwrap()
        }
        fn effective(value: &'static [u8]) -> EffectiveProviderConfigurationId {
            serde_json::from_value(
                serde_json::to_value(digest(DigestDomain::EffectiveProviderConfiguration, value))
                    .unwrap(),
            )
            .unwrap()
        }

        let old = FreshNativeBinding {
            selected: selected(b"selected"),
            effective: effective(b"effective"),
            account: digest(DigestDomain::ModelBinding, b"account"),
            credential_authority: digest(DigestDomain::ModelBinding, b"authority"),
            selection: digest(DigestDomain::ModelBinding, b"selection epoch"),
            generation: 7,
        };
        let mut fresh = FreshNativeBinding {
            generation: 8,
            ..old
        };
        assert!(old.admits(&fresh));
        fresh.generation = 6;
        assert!(!old.admits(&fresh), "generation rollback");
        fresh.generation = 8;
        fresh.account = digest(DigestDomain::ModelBinding, b"foreign account");
        assert!(!old.admits(&fresh));
        fresh.account = old.account;
        fresh.credential_authority = digest(DigestDomain::ModelBinding, b"foreign slot");
        assert!(!old.admits(&fresh));
        fresh.credential_authority = old.credential_authority;
        fresh.selection = digest(DigestDomain::ModelBinding, b"relogin epoch");
        assert!(!old.admits(&fresh));
        fresh.selection = old.selection;
        fresh.effective = effective(b"changed model semantics");
        assert!(!old.admits(&fresh));
        fresh.effective = old.effective;
        fresh.selected = selected(b"changed selection");
        assert!(!old.admits(&fresh));
    }

    #[test]
    fn unavailable_native_authority_names_its_login_remedy() {
        let failure = native_authority_unavailable("Codex", "codex");
        assert!(failure.message().contains("run `plak login codex`"));
        assert!(failure.message().contains("earlier build"));
    }

    #[test]
    fn exact_openrouter_resume_binds_selected_and_resolved_wire() {
        let mut current = SessionRecord::new("/work/openrouter".into());
        current.account = Some("openrouter".into());
        current.model = Some("vendor/model".into());
        current.composition = crate::binding::CompositionSelection::Direct;
        current.target_selection = TargetSelection::OpenRouter {
            wire: WireBinding::resolve(ProtocolSelection::Auto),
            routing: OpenRouterRoutingSelection::GatewayManaged,
        };
        let mut candidate = current.clone();
        assert!(exact_resume_target_matches(
            &current,
            &candidate,
            "vendor/model"
        ));

        candidate.target_selection = TargetSelection::OpenRouter {
            wire: WireBinding::resolve(ProtocolSelection::Responses),
            routing: OpenRouterRoutingSelection::GatewayManaged,
        };
        assert!(!exact_resume_target_matches(
            &current,
            &candidate,
            "vendor/model"
        ));
    }

    #[test]
    fn delayed_transition_stage_times_out_without_detaching_source() {
        let executor = ApplicationExecutor::new().unwrap();
        let started = Instant::now();
        let outcome = executor
            .block_on(bounded_transition_stage(Duration::from_millis(20), async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }))
            .unwrap();
        assert_eq!(outcome, Err(()));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn code_mode_attach_failure_names_js_sidecar() {
        let guidance = attach_repair_guidance(
            "Codex Code Mode sidecar is unavailable: plakous-js-host not found",
        );
        assert!(guidance.contains("PLAK_JS_HOST"));
        assert!(!guidance.contains("PLAK_CODE_MODE_HOST"));
        assert!(!guidance.contains("0.149.0"));
    }

    #[test]
    fn product_javascript_attach_failure_names_the_sidecar_repair() {
        let guidance = attach_repair_guidance(
            "product JavaScript host is unavailable: plakous-js-host not found",
        );

        assert!(guidance.contains("PLAK_JS_HOST"));
        assert!(guidance.contains("plakous-js-host"));
        assert!(!guidance.contains("PLAK_CODE_MODE_HOST"));
        assert!(!guidance.contains("/login"));
    }

    #[test]
    fn failed_startup_is_joined_before_error_returns() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (action_tx, _action_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        drop(event_tx);
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        let joined = Arc::new(AtomicBool::new(false));
        let thread_joined = Arc::clone(&joined);
        let handle = thread::spawn(move || {
            startup_tx.send(Err("startup refused".to_owned())).unwrap();
            thread::sleep(Duration::from_millis(20));
            thread_joined.store(true, Ordering::Release);
            Err(CliFailure::new("startup refused"))
        });

        let Err(error) = await_host_startup(startup_rx, action_tx, event_rx, handle) else {
            panic!("failed startup unexpectedly attached");
        };
        assert_eq!(error.message(), "startup refused");
        assert!(joined.load(Ordering::Acquire));
    }

    #[test]
    fn failed_startup_preserves_recovery_required_class_after_join() {
        let (action_tx, _action_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        drop(event_tx);
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            startup_tx.send(Err("unfinished head".to_owned())).unwrap();
            Err(CliFailure::recovery_required("unfinished head"))
        });
        let Err(error) = await_host_startup(startup_rx, action_tx, event_rx, handle) else {
            panic!("failed startup unexpectedly attached");
        };
        assert_eq!(error, CliFailure::recovery_required("unfinished head"));
        assert_eq!(error.exit_code(), 5);
        assert_eq!(error.message(), "unfinished head");
    }

    /// The child-binding seam: a host loop configured with an injected child
    /// runtime installs it for exactly its own lifetime, and
    /// `agent::children::child_session_runtime_binding` consults it before it
    /// resolves anything.
    ///
    /// One test, not three, because the factory is process-global: split across
    /// test functions these assertions would race each other under a
    /// thread-per-test runner.
    #[test]
    fn an_injected_child_runtime_is_installed_for_the_life_of_its_host_loop() {
        use crate::agent::injected_child_binding;
        use crate::runtime::test_support::fresh_injected_attached_host;

        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        assert!(
            injected_child_binding::lookup_for_test().is_none(),
            "nothing may be installed before a host loop asks for it"
        );
        assert!(
            install_injected_child_runtime(&config).is_none(),
            "a config with no child factory installs nothing, so production is untouched"
        );

        let injection = config
            .test_injection
            .as_ref()
            .expect("the fixture carries an injection");
        config.test_injection = Some(Arc::new(TestHostInjection {
            runtime: Arc::clone(&injection.runtime),
            rebind_target: injection.rebind_target.clone(),
            observe: Arc::clone(&injection.observe),
            child_runtime: Some(Arc::new(|agent_id: &str, path: &str, _| {
                Err(format!("injected child binding for {agent_id} at {path}"))
            })),
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        }));

        {
            let _guard = install_injected_child_runtime(&config)
                .expect("a config carrying a child factory installs it");
            let installed = injected_child_binding::lookup_for_test()
                .expect("the child binder must see the installed factory");
            assert_eq!(
                installed(
                    "a2",
                    "/work/a2",
                    &crate::agent::ProductStateRoots::with_doubles(
                        root.path().to_path_buf(),
                        root.path().join("state"),
                    )
                )
                .err(),
                Some("injected child binding for a2 at /work/a2".to_owned()),
                "the binder must reach the test's own factory, not a resolved provider"
            );
        }
        assert!(
            injected_child_binding::lookup_for_test().is_none(),
            "the guard must uninstall, so one test's fake cannot outlive it"
        );
    }
    #[test]
    fn deferred_startup_never_claims_attachment_before_failed_open_falls_back() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("p");
        let session_home = SessionHome::new(home.join("sessions"));
        let store = Arc::new(crate::session_store::MemoryStore::new());
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.account = Some("codex".into());
        session.model = Some("gpt-existing".into());
        store.save(&mut session).unwrap();
        let config_id = session.id.clone();
        let revision = session.store_revision;
        let publication = Arc::new(AtomicBool::new(false));
        let config = HostConfig {
            surface: test_kit::recording_surface(),
            auto_continue: true,
            topology_admission: None,
            state_roots: crate::agent::ProductStateRoots::with_doubles(
                home.clone(),
                home.join("state"),
            ),
            product_home: home.clone(),
            home: session_home.clone(),
            store: Arc::clone(&store) as Arc<dyn SessionRecordStore>,
            session,
            target: HostTargetAuthority::Injected {
                provider: NativeProvider::Codex,
            },
            model_id: "gpt-existing".into(),
            effort: Some(ReasoningEffort::High),
            effort_explicit: true,
            compaction_enabled: true,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            topology_enabled: true,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: root.path().to_path_buf(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
            resume_publication: Some(Arc::clone(&publication)),
            test_injection: None,
            ready_path: ReadyPathTiming::default(),
            seed_history: None,
            source_releases: SourceReleases::default(),
        };
        let shell_host = test_kit::doubles_shell_host();
        let (actions, events, worker) = spawn_deferred_tui_host(move || TuiHostPreparation {
            providers: Vec::new(),
            mode: TuiHostMode::Attached(config),
            catalog_refresh: None,
            shell_host,
        });

        let mut claimed_ready = false;
        loop {
            let event = events.recv_timeout(Duration::from_secs(2)).unwrap();
            match event {
                HostEvent::AgentAttached(true) => claimed_ready = true,
                HostEvent::AgentAttached(false) => break,
                _ => {}
            }
        }
        assert!(!claimed_ready);
        assert!(!publication.load(Ordering::Acquire));
        let unchanged = store.load(&config_id).unwrap();
        assert_eq!(unchanged.store_revision, revision);
        assert!(
            session_log::read_tail(&session_home, &config_id, 8)
                .unwrap()
                .iter()
                .all(|line| !line.contains("session.resumed"))
        );
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
    }
}
