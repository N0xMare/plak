//! Host startup: worker spawn, lifecycle phases, and session opening.
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{AttachFailure, DetachedReason, HostPhase, HostStatusDetail};
use std::future::Future;
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};

use plakous::application::ApplicationExecutor;
use plakous::model::{
    Clock, Digest, EffectiveProviderConfigurationId, RuntimeModelAuthorityError,
    SelectedProviderConfigurationId, SessionLineage, SystemClock,
};
use plakous::sessions::SessionRuntimeBinding;
use plakous::subscriptions::{
    NativeAuthStatus, NativeProvider, NativeSubscriptions, ResolvedProvider,
};

use crate::agent::{
    AttachedCodingSession, ProductAgentSpec, SessionToolFence, api_session_runtime_binding,
    api_topology_parent, coding_session_runtime_binding, open_api_coding_session_with_fence,
    open_coding_session,
};
use crate::binding::TargetSelection;
use crate::sessions::SessionRecord;
use crate::{CliFailure, CliResult};

use super::HostWorkerHandle;
use super::host_loop::run_host_loop;
use super::info::plan_state_authority;
use super::models::effort_from_record;
use super::rebind::durable_tree_id;
use super::{
    HostConfig, HostLaunchMode, HostLaunchPreparation, HostLifecyclePhase, HostTargetAuthority,
    OpenRouterHostEvidence, RuntimeRole, Sender, ShellHostConfig,
};
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) fn observe_phase<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    phase: HostLifecyclePhase,
    started: Instant,
    queue_len: usize,
) {
    let _ = events.send(ContentEvent::Status {
        phase: match phase {
            HostLifecyclePhase::ClearResolveAuthority
            | HostLifecyclePhase::ClearBuildCandidate
            | HostLifecyclePhase::ClearPublishCandidate
            | HostLifecyclePhase::ClearActivate => HostPhase::Clear,
            _ => HostPhase::Rebind,
        },
        queue_len,
        detail: Some(HostStatusDetail::TransitionStage {
            stage: phase,
            cumulative_elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }),
    });
}

pub(super) async fn bounded_transition_stage<T>(
    timeout: Duration,
    stage: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::time::timeout(timeout, stage).await.map_err(|_| ())
}

/// Control mailbox, bounded observations, and the worker owned by one driver.
pub type HostWorker<P = plakous_host_protocol::Core> = (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    HostWorkerHandle,
);

/// Spawn host worker thread and return channels for the TUI.
pub fn spawn_host_worker<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
) -> CliResult<HostWorker<P>> {
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
    let event_tx = Sender::over(event_tx);
    let handle = HostWorkerHandle::spawn_with_receipt(move |receipt| {
        let mut event_tx = event_tx;
        event_tx.quit_receipt = receipt;
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
            let _ = event_tx.notice(
                TranscriptRow::info(NoticeScope::Host, format!("host error: {}", err.message()))
                    .with_optional_remedy(err.remedy()),
            );
            let _ = event_tx.send(ContentEvent::Status {
                phase: HostPhase::Error,
                queue_len: 0,
                detail: Some(HostStatusDetail::Failure {
                    failure: err.host_failure(),
                }),
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
    handle: HostWorkerHandle,
) -> CliResult<HostWorker<P>> {
    match startup_rx.recv() {
        Ok(Ok(())) => Ok((action_tx, event_rx, handle)),
        Ok(Err(error)) => {
            drop(action_tx);
            match handle
                .join()
                .map_err(|_| CliFailure::new("plak product host failed while joining startup"))?
            {
                Ok(_) => Err(CliFailure::new(error)),
                Err(termination) => Err(termination),
            }
        }
        Err(_) => {
            drop(action_tx);
            match handle
                .join()
                .map_err(|_| CliFailure::new("plak product host panicked during startup"))?
            {
                Ok(_) => Err(CliFailure::new(
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
pub fn spawn_deferred_host<P: plakous_host_protocol::ProtocolExtension>(
    prepare: impl FnOnce() -> HostLaunchPreparation<P> + Send + 'static,
) -> (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    HostWorkerHandle,
) {
    spawn_deferred_host_with_clear_publication(prepare, None)
}

pub fn spawn_deferred_host_with_clear_publication<P: plakous_host_protocol::ProtocolExtension>(
    prepare: impl FnOnce() -> HostLaunchPreparation<P> + Send + 'static,
    clear_publication: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) -> (
    crate::surface_protocol::UserActionSender<P>,
    crate::surface_protocol::HostEventReceiver<P>,
    HostWorkerHandle,
) {
    let (action_tx, action_rx) = crate::surface_protocol::user_action_channel();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel();
    let event_tx = Sender::over(event_tx);
    let handle = HostWorkerHandle::spawn_with_receipt(move |receipt| {
        let mut event_tx = event_tx;
        event_tx.quit_receipt = receipt;
        let _ = event_tx.send(ContentEvent::Activity {
            label: "discovering providers and preparing session".into(),
            active: true,
        });
        let prepared = prepare();
        let _ = event_tx.send(ContentEvent::ProviderSnapshot {
            providers: prepared.providers.clone(),
        });
        if let Some(refresh) = prepared.catalog_refresh {
            refresh(event_tx.clone());
        }
        let shell_host = prepared.shell_host;
        match prepared.mode {
            HostLaunchMode::Attached(config) => {
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
                        DetachedReason::RecoveryRequired(error.host_failure())
                    } else {
                        DetachedReason::ResolveFailed(error.host_failure())
                    };
                    if let Ok(authoritative) = fallback.store.load(&fallback.session.id) {
                        fallback.session = authoritative;
                    }
                    let _ = event_tx.send(ContentEvent::AgentAttached(false));
                    let _ = event_tx.notice(
                        TranscriptRow::error(NoticeScope::Host, error.message().to_owned())
                            .with_optional_remedy(error.remedy()),
                    );
                    return shell_host.run(
                        fallback,
                        action_rx,
                        event_tx,
                        clear_publication.clone(),
                    );
                }
                Ok(())
            }
            HostLaunchMode::Shell(config) => {
                let _ = event_tx.send(ContentEvent::AgentAttached(false));
                let _ = event_tx.send(ContentEvent::Activity {
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
    Ok(create_seeded_session_with_preflight(config, |_| Ok(()))?.into_session())
}

/// A canonical seed was committed, even when its release or product save failed.
pub struct SeededSessionCreation {
    /// The committed successor, with its exact runtime locator.
    pub session: SessionRecord,
    /// Advisory construction output.
    pub notes: String,
    /// Post-commit failure; never permission to create another successor.
    pub completion_error: Option<CliFailure>,
}

impl SeededSessionCreation {
    /// Keeps the committed successor and records any completion failure as a
    /// repair notice rather than discarding its identity.
    #[must_use]
    pub fn into_session(mut self) -> (SessionRecord, String) {
        if let Some(error) = self.completion_error {
            let notice = format!(
                "session {} was created but completion failed: {}; continue with this committed successor, not its source",
                self.session.id,
                error.message()
            );
            self.session
                .record_notice(TranscriptRow::warning(NoticeScope::Session, &notice));
            self.notes.push_str(&format!("\n{notice}"));
        }
        (self.session, self.notes)
    }
}

/// Creates a seed after a host-owned advisory capacity callback, reusing the
/// exact runtime binding that will be created. No inference is dispatched.
///
/// # Errors
/// Returns failures before canonical create commits. A subsequent release or
/// persistence failure is returned with the committed successor, never as a
/// refusal that would authorize creating another one.
pub fn create_seeded_session_with_preflight<P: plakous_host_protocol::ProtocolExtension>(
    mut config: HostConfig<P>,
    preflight: impl FnOnce(plakous::sessions::SessionInputWindow) -> CliResult<()> + Send + 'static,
) -> CliResult<SeededSessionCreation> {
    if config.session.plakous_locator.is_some() {
        return Err(CliFailure::new(
            "a carried history seeds only a session that has no runtime yet",
        ));
    }
    let history = config
        .seed_history
        .take()
        .ok_or_else(|| CliFailure::new("no carried history to seed the session with"))?;
    let seed = crate::agent::SeededHistory::with_preflight(history, preflight)
        .with_line(config.seed_line.take());
    let executor = ApplicationExecutor::new().map_err(|e| CliFailure::new(e.to_string()))?;
    let mut stderr_buf = Vec::new();
    let factory = std::sync::Arc::clone(&config.runtime_factory);
    super::factory::observe_boundary(
        factory.as_ref(),
        super::HostWorkflow::SeededCreate,
        super::HostBoundary::Prepared,
    );
    let opened = executor
        .block_on(create_session(&mut config, Some(seed), &mut stderr_buf))
        .map_err(|e| CliFailure::new(e.to_string()))??;
    super::factory::observe_boundary(
        factory.as_ref(),
        super::HostWorkflow::SeededCreate,
        super::HostBoundary::RuntimeReturned,
    );
    record_replay_fallback(
        &config.home,
        &mut config.session,
        opened.replay_validation_error(),
    );
    let notes = String::from_utf8_lossy(&stderr_buf).into_owned();
    let completion = (|| -> CliResult<()> {
        // The save that binds the locator ends a carry's write-ahead intent.
        config.session.carry_intent = None;
        config
            .session
            .bind_plakous_locator(opened.locator())
            .map_err(CliFailure::new)?;
        let locator = executor
            .block_on(opened.release())
            .map_err(|e| CliFailure::new(e.to_string()))?
            .map_err(|e| CliFailure::new(e.to_string()))?;
        config
            .session
            .bind_plakous_locator(locator)
            .map_err(CliFailure::new)?;
        config.session.touch();
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)
    })();
    Ok(SeededSessionCreation {
        session: config.session,
        notes,
        completion_error: completion.err(),
    })
}

fn record_replay_fallback(
    home: &crate::session_home::SessionHome,
    session: &mut SessionRecord,
    error: Option<&plakous::agent::AgentError>,
) {
    let Some(error) = error else { return };
    crate::session_log::emit(
        home,
        &session.id,
        crate::session_log::SessionEvent::new("session.replay_stripped")
            .str("reason", error.message()),
    );
    session.record_notice(TranscriptRow::warning(
        NoticeScope::Settings,
        "Provider replay was incompatible with this build and was stripped; the conversation \
         was carried without it.",
    ));
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
        // An interrupted carry's record names the create it is waiting for:
        // only its settlement may create that runtime (spec 02 §2.6).
        if seed.is_none() && config.session.carry_intent.is_some() {
            return Err(CliFailure::new(format!(
                "session {} is an interrupted carry; resume it to finish the carry",
                config.session.id
            )));
        }
        let line = config.seed_line.take();
        return create_session(
            config,
            seed.map(|history| crate::agent::SeededHistory::new(history).with_line(line)),
            stderr_buf,
        )
        .await;
    }
    // Spec 02 §4.2: step 0 names the head, step 1 opens exactly it. A stale
    // refusal means another owner committed in between: step 0 runs once
    // more, and a second stale answer is lease contention.
    let mut retry = crate::driver::reopen::ExactRetry::default();
    loop {
        let (head, projection_error) = projected_head(config).await?;
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
                if !retry.adopt() {
                    return Err(CliFailure::new(format!(
                        "{}; another process is using this session",
                        error.failure.message()
                    )));
                }
            }
            Err(error) => return Err(*error.failure),
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

/// Step 1 of the reopen: the exact open of `head` under a fresh lease.
pub(super) async fn production_open_head<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    head: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
) -> Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure> {
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

/// The verification twin of [`production_open_head`]: the same authority
/// refresh and runtime compilation, answered without owning the head.
pub(super) async fn production_verify_head<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    head: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
) -> Result<(), crate::agent::CodingSessionOpenFailure> {
    if config.target.is_api() {
        return Box::pin(crate::agent::verify_api_coding_session(
            api_parent_from_config(config, &config.session)?,
            head,
            openrouter_cached_evidence(config),
        ))
        .await;
    }
    refresh_native_target(config).await?;
    Box::pin(crate::agent::verify_coding_session(
        native_spec(config)?,
        head,
        stderr_buf,
    ))
    .await
}

pub(super) async fn production_create_session<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    seed: Option<crate::agent::SeededHistory>,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<AttachedCodingSession> {
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

pub(super) fn native_spec<P: plakous_host_protocol::ProtocolExtension>(
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

/// Re-resolve stale native authority, retaining the exact model and
/// selected login. Returns whether the attached host must reopen under the
/// fresh receipt.
pub(super) async fn refresh_native_target<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
) -> CliResult<bool> {
    let HostTargetAuthority::Native {
        provider,
        resolved,
        subscriptions,
        ..
    } = &config.target
    else {
        return Ok(false);
    };
    let provider = *provider;
    let resolved = std::sync::Arc::clone(resolved);
    let subscriptions = subscriptions.clone();
    let (label, account) = match provider {
        NativeProvider::Codex => ("Codex", "codex"),
        NativeProvider::Xai => ("xAI", "grok"),
        NativeProvider::Claude => ("Claude", "claude"),
        _ => return Ok(false),
    };
    Box::pin(refresh_native_authority(
        resolved.model(),
        label,
        account,
        async {
            let previous = resolved.as_ref();
            let selected = previous.configuration().selected().clone();
            let limits = crate::agent::product_subscription_request_limits()?;
            let fresh = match subscriptions
                .resolve_turn_scoped_provider(selected, limits, SessionLineage::Root)
                .await
            {
                Ok(fresh) => fresh,
                Err(error) => {
                    let repair =
                        native_credential_failure(&subscriptions, provider, label, account).await;
                    return Err(CliFailure::new(format!(
                        "exact {label} resolution failed: {}; {}",
                        error.kind,
                        repair.message()
                    )));
                }
            };
            let previous_binding = FreshNativeBinding::from_resolved(previous)?;
            let fresh_binding = FreshNativeBinding::from_resolved(&fresh)?;
            if !previous_binding.admits(&fresh_binding) {
                return Err(previous_binding.refusal(&fresh_binding, label, account));
            }
            let HostTargetAuthority::Native { resolved, .. } = &mut config.target else {
                unreachable!("the native target was checked above");
            };
            *resolved = std::sync::Arc::new(fresh);
            Ok(true)
        },
        native_credential_failure(&subscriptions, provider, label, account),
    ))
    .await
}

pub(super) async fn refresh_native_authority(
    model: std::sync::Arc<dyn plakous::model::LanguageModel>,
    label: &str,
    account: &str,
    resolve: impl Future<Output = CliResult<bool>>,
    unavailable: impl Future<Output = CliFailure>,
) -> CliResult<bool> {
    match model.revalidate_runtime_authority(SystemClock.now()).await {
        Ok(_) => Ok(false),
        Err(RuntimeModelAuthorityError::AccountChanged) => {
            Err(native_account_changed(label, account))
        }
        // A pre-admission login wait must re-resolve too: Ready can carry a
        // fresh receipt, while missing/quarantined credentials name the repair.
        Err(
            RuntimeModelAuthorityError::Stale
            | RuntimeModelAuthorityError::ForeignAuthority
            | RuntimeModelAuthorityError::AwaitingCredential,
        ) => resolve.await,
        Err(RuntimeModelAuthorityError::Unavailable) => Err(unavailable.await),
        Err(error) => Err(CliFailure::new(format!(
            "{label} runtime authority cannot be refreshed: {error}"
        ))),
    }
}

#[cfg(test)]
pub(in crate::runtime) mod native_refresh_tests;

async fn native_credential_failure(
    subscriptions: &NativeSubscriptions,
    provider: NativeProvider,
    label: &str,
    account: &str,
) -> CliFailure {
    let quarantined = subscriptions
        .status(provider)
        .await
        .is_ok_and(|status| status.credential() == NativeAuthStatus::Quarantined);
    native_credential_message(label, account, quarantined)
}

fn native_account_changed(label: &str, account: &str) -> CliFailure {
    let account = plakous_host_protocol::notice_remedy::AccountTarget::from_account(account);
    CliFailure::new(format!(
        "{label} provider account changed; restore the original account for exact reopen, or \
         explicitly carry history into a new session under the current account"
    ))
    .with_remedy(
        plakous_host_protocol::notice_remedy::NoticeRemedy::RestoreAccountOrRebind { account },
    )
    .with_attach_failure(AttachFailure::AccountChanged)
    .with_origin(
        plakous_host_protocol::status::HostFailureOrigin::NativeBinding {
            label: label.to_owned(),
            account,
        },
    )
}

fn native_credential_message(label: &str, account: &str, quarantined: bool) -> CliFailure {
    if quarantined {
        CliFailure::new(format!(
            "{label} credential was rejected at refresh; restore usable credentials"
        ))
        .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::Login {
            account: plakous_host_protocol::notice_remedy::AccountTarget::from_account(account),
        })
        .with_attach_failure(AttachFailure::CredentialRejected)
        .with_origin(
            plakous_host_protocol::status::HostFailureOrigin::NativeBinding {
                label: label.to_owned(),
                account: plakous_host_protocol::notice_remedy::AccountTarget::from_account(account),
            },
        )
    } else {
        native_authority_unavailable(label, account)
    }
}

/// A renewal that found no usable credential names the repair, as other
/// credential failures do.
pub(super) fn native_authority_unavailable(label: &str, account: &str) -> CliFailure {
    let account = plakous_host_protocol::notice_remedy::AccountTarget::from_account(account);
    CliFailure::new(format!(
        "{label} credential is unavailable (signed out, quarantined, or a login made by an \
         earlier build); restore usable credentials"
    ))
    .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::Login { account })
    .with_attach_failure(AttachFailure::CredentialUnavailable)
    .with_origin(
        plakous_host_protocol::status::HostFailureOrigin::NativeBinding {
            label: label.to_owned(),
            account,
        },
    )
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
    fn refusal(&self, fresh: &Self, label: &str, account: &str) -> CliFailure {
        if self.selected == fresh.selected
            && self.effective == fresh.effective
            && self.account == fresh.account
            && self.credential_authority == fresh.credential_authority
            && self.selection != fresh.selection
        {
            let account =
                plakous_host_protocol::notice_remedy::AccountTarget::from_account(account);
            CliFailure::new(format!(
                "exact reopen refused: {label} login selection changed; explicitly carry history \
                 under the current login; an attached-session fork requires an attachment"
            ))
            .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::CarryCurrentLogin {})
            .with_attach_failure(AttachFailure::LoginSelectionChanged)
            .with_origin(
                plakous_host_protocol::status::HostFailureOrigin::NativeBinding {
                    label: label.to_owned(),
                    account,
                },
            )
        } else {
            let account =
                plakous_host_protocol::notice_remedy::AccountTarget::from_account(account);
            CliFailure::new(format!(
                "exact reopen refused: {label} is bound to a different account or model; restore \
                 the original account and configuration, or explicitly carry history into a new \
                 session"
            ))
            .with_remedy(
                plakous_host_protocol::notice_remedy::NoticeRemedy::RestoreBindingOrCarry {
                    account,
                },
            )
            .with_attach_failure(AttachFailure::BindingChanged)
            .with_origin(
                plakous_host_protocol::status::HostFailureOrigin::NativeBinding {
                    label: label.to_owned(),
                    account,
                },
            )
        }
    }

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

/// The profile `session`'s genesis freezes (spec 02 §2.6): the root
/// template and working set the roster source gives its root bind, the same
/// answer a tree genesis freezes. `None` for a record with no bound tuple yet.
///
/// # Errors
/// The roster cannot be read (an unreadable product policy).
pub(super) fn genesis_root_profile(
    roots: &crate::agent::ProductStateRoots,
    session: &SessionRecord,
) -> CliResult<Option<crate::sessions::RootProfile>> {
    let (Some(account), Some(model)) = (session.account.as_deref(), session.model.as_deref())
    else {
        return Ok(None);
    };
    let roster = roots
        .roster_source()
        .roster(account, model, session.effort.as_deref())?;
    Ok(Some(crate::sessions::RootProfile::from_roster(&roster)))
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
        crate::capabilities::require_feature("mcp")?;
        let bound = crate::mcp_binding::bind_mcp_tools(
            &tools.selection,
            &tools.tool_sections,
            &config.session.workspace_root,
            config.state_roots.named_secrets(),
            None,
        )?;
        frozen = frozen.with_mcp_tools(bound.placed_records());
        for notice in bound.notices {
            config
                .session
                .record_notice(TranscriptRow::warning(NoticeScope::Settings, notice));
        }
    }
    config.session.frozen_tools = Some(frozen);
    Ok(())
}

pub(super) fn openrouter_cached_evidence<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> Option<crate::openrouter::OpenRouterChatModelEvidence> {
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

pub(super) async fn api_host_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<SessionRuntimeBinding> {
    let parent = api_parent_from_config(config, &config.session)?;
    api_session_runtime_binding(
        &parent,
        true,
        SessionToolFence::Full,
        openrouter_cached_evidence(config),
    )
    .await
    .map(|prepared| prepared.runtime)
}

pub(super) async fn production_api_runtime_for_record<
    P: plakous_host_protocol::ProtocolExtension,
>(
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

pub(super) async fn api_compaction_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
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
        HostTargetAuthority::OpenRouter { evidence, .. } => evidence
            .as_ref()
            .filter(|cached| cached.matches(&literal_model, target.protocol()))
            .map(|cached| cached.evidence.clone()),
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
    api_session_runtime_binding(&parent, true, SessionToolFence::Full, cached_evidence)
        .await?
        .runtime
        .with_compaction_output_limit()
        .map_err(|_| CliFailure::new("compaction generation policy is invalid"))
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

pub(super) async fn production_host_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
    role: RuntimeRole,
) -> CliResult<SessionRuntimeBinding> {
    if role == RuntimeRole::Compact && config.target.is_api() {
        return api_compaction_runtime_binding(config).await;
    }
    if config.target.is_api() {
        return api_host_runtime_binding(config).await;
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
    native_runtime_for_record(spec, role, stderr_buf).await
}

pub(super) async fn native_runtime_for_record(
    spec: ProductAgentSpec,
    role: RuntimeRole,
    stderr: &mut (dyn std::io::Write + Send),
) -> CliResult<SessionRuntimeBinding> {
    let binding = coding_session_runtime_binding(spec, stderr).await?;
    if role == RuntimeRole::Compact {
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
pub(super) async fn production_open_session_for_host<
    P: plakous_host_protocol::ProtocolExtension,
>(
    config: &HostConfig<P>,
    session: &SessionRecord,
    locator: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
    role: RuntimeRole,
) -> Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure> {
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

async fn open_head<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    head: plakous::sessions::SessionLocator,
    stderr: &mut Vec<u8>,
) -> Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure> {
    let factory = config.runtime_factory.clone();
    factory.open_head(config, head, stderr).await
}
async fn create_session<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    seed: Option<crate::agent::SeededHistory>,
    stderr: &mut Vec<u8>,
) -> CliResult<AttachedCodingSession> {
    freeze_root_tools(config)?;
    if config.session.root_profile.is_none() {
        config.session.root_profile = genesis_root_profile(&config.state_roots, &config.session)?;
    }
    let factory = config.runtime_factory.clone();
    factory.create(config, seed, stderr).await
}
pub(super) async fn host_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    stderr: &mut Vec<u8>,
    role: RuntimeRole,
) -> CliResult<SessionRuntimeBinding> {
    config.runtime_factory.runtime(config, role, stderr).await
}
pub(super) async fn open_session_for_host<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    record: &SessionRecord,
    head: plakous::sessions::SessionLocator,
    stderr: &mut Vec<u8>,
    role: RuntimeRole,
) -> Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure> {
    config
        .runtime_factory
        .open(config, record, head, role, stderr)
        .await
}

pub(super) async fn api_runtime_for_record<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    record: &SessionRecord,
    opening_durable_session: bool,
    topology_attach: crate::topology_runtime::TopologyHubAttach,
    tree_id: Option<&str>,
    attached_tree: &mut Option<String>,
) -> CliResult<SessionRuntimeBinding> {
    config
        .runtime_factory
        .api_runtime(
            config,
            record,
            opening_durable_session,
            topology_attach,
            tree_id,
            attached_tree,
        )
        .await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reopen_tests;
