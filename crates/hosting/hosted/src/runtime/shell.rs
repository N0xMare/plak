//! Detached shell host loop and its binding intents.

use crate::sessions::queue::{drop_last_mutable, edit_mutable, move_mutable, remove_mutable};

use std::sync::mpsc::RecvTimeoutError;

use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, TargetSelection, WireBinding};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::SessionRecord;

use super::admission::{
    QueuePersistAction, commit_session_update, emit_prompt_history, emit_queue_snapshot,
    queue_edit_applied, queue_edit_refused, queue_refusal, recovery_blocked_action,
    refuse_detached_recovery_action, reject_draft, reject_stale_queue_action,
    report_queue_persist_failed,
};
use super::logging::{log_event, project_session_snapshot};
use super::models::oauth_provider_for_account;
use super::resume::emit_resume_choices;
use super::usage::{report_unavailable_context, request_subscription_usage_cached};
use super::{
    APPROVAL_REFUSED_NOTE, HeldUsage, HostConfig, HostEvent, IDLE_OBSERVE_POLL_INTERVAL,
    ProductAuthHost, RootBindingSaveError, Sender, SessionLedger, ShellBindingOutcome,
    ShellHostConfig, SubscriptionUsageRefresh, SurfaceHandler, UserAction, queue,
};
use crate::sessions::{NoticeScope, TranscriptRow};

pub fn run_shell_host_loop<P: plakous_host_protocol::ProtocolExtension>(
    mut config: ShellHostConfig<P>,
    actions: crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    clear_publication: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) {
    let log = (config.session.store_revision != 0)
        .then(|| SessionLog::open(&config.home, &config.session.id).ok())
        .flatten();
    let recovery_blocked = config.reason.starts_with("recovery_required:");
    log_event(
        log.as_ref(),
        &config.session.id,
        SessionEvent::new("host.shell")
            .str("reason", &config.reason)
            .opt_str("account", config.session.account.as_deref())
            .opt_str("model", config.session.model.as_deref()),
    );

    // The not-ready shell is reachable as a fallback *after* the canonical
    // attach published `SessionOpening` and then failed. Nothing else retires
    // that startup row, so the TUI kept its 80 ms spinner cadence alive for the
    // life of the process on a session that had stopped opening — a periodic
    // repaint with no demand behind it, and the dominant idle-CPU cost on any
    // host where attach cannot complete. Zero is truthful here: no session was
    // opened.
    let _ = events.send(HostEvent::SessionOpened {
        session_open_ms: None,
    });
    emit_shell_empty_state(&events, &config);
    send_shell_binding(&events, &config);
    let _ = events.send(project_session_snapshot(&config.session));
    emit_prompt_history(&events, &config.session);
    emit_queue_snapshot(&events, &config.session);
    let _ = events.send(HostEvent::Status {
        phase: "not_ready".into(),
        queue_len: config.session.pending_queue.len(),
        detail: Some(format!("not ready · {}", config.reason)),
    });
    let _ = events.send(HostEvent::SetBusy(false));
    let held_usage = HeldUsage::default();
    let session_ledger = SessionLedger::default();
    let subscription_usage_refresh = SubscriptionUsageRefresh::new(
        &events,
        std::sync::Arc::clone(&config.openrouter_credentials),
        held_usage.clone(),
        session_ledger.clone(),
    );

    loop {
        let delivery = match actions.recv_timeout(IDLE_OBSERVE_POLL_INTERVAL) {
            Ok(delivery) => delivery,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(started) = delivery.started() {
            let _ = events.send(started);
        }
        let (action, _acknowledgement) = delivery.into_parts();
        if recovery_blocked && recovery_blocked_action(&action) {
            refuse_detached_recovery_action(&events, &action);
            continue;
        }
        match action {
            UserAction::Recover => {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Session,
                    "no session is attached; /attach reopens it, and reopening recovers any \
                     interrupted turn",
                ));
            }
            UserAction::Quit => break,
            UserAction::Background => {}
            action @ (UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_)) => {
                refuse_not_ready(&events, &config, log.as_ref());
                reject_draft(&events, action);
            }
            UserAction::DropQueued => {
                match commit_session_update(
                    &*config.store,
                    &config.home,
                    &mut config.session,
                    drop_last_mutable,
                ) {
                    Ok(true) => emit_queue_snapshot(&events, &config.session),
                    Ok(false) => {}
                    Err(error) => report_queue_persist_failed(
                        &events,
                        &config.home,
                        &config.session,
                        QueuePersistAction::Drop,
                        &error,
                    ),
                }
            }
            UserAction::ResolveApproval { .. } => {
                // No turn is attached, so no decision was ever offered. The
                // host refuses without contacting any runtime.
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Approval,
                    APPROVAL_REFUSED_NOTE,
                ));
            }
            UserAction::DeleteQueued { id, revision } => {
                match commit_session_update(
                    &*config.store,
                    &config.home,
                    &mut config.session,
                    |candidate| remove_mutable(candidate, &id, revision),
                ) {
                    Ok(true) => emit_queue_snapshot(&events, &config.session),
                    Ok(false) => reject_stale_queue_action(&events, &config.session),
                    Err(error) => report_queue_persist_failed(
                        &events,
                        &config.home,
                        &config.session,
                        QueuePersistAction::Delete,
                        &error,
                    ),
                }
            }
            UserAction::EditQueued { id, revision, text } => {
                let mut refusal = None;
                match commit_session_update(
                    &*config.store,
                    &config.home,
                    &mut config.session,
                    |candidate| match edit_mutable(candidate, &id, revision, &text) {
                        Ok(()) => true,
                        Err(error) => {
                            refusal = Some(error);
                            false
                        }
                    },
                ) {
                    Ok(true) => {
                        emit_queue_snapshot(&events, &config.session);
                        queue_edit_applied(&events, id);
                    }
                    Ok(false) => {
                        emit_queue_snapshot(&events, &config.session);
                        queue_edit_refused(&events, id);
                        queue_refusal(
                            &events,
                            "queue edit",
                            refusal.unwrap_or(queue::Refusal::Stale),
                        );
                    }
                    Err(error) => {
                        report_queue_persist_failed(
                            &events,
                            &config.home,
                            &config.session,
                            QueuePersistAction::Edit,
                            &error,
                        );
                        queue_edit_refused(&events, id);
                    }
                }
            }
            UserAction::MoveQueued {
                id,
                revision,
                new_index,
            } => {
                match commit_session_update(
                    &*config.store,
                    &config.home,
                    &mut config.session,
                    |candidate| move_mutable(candidate, &id, revision, new_index),
                ) {
                    Ok(true) => emit_queue_snapshot(&events, &config.session),
                    Ok(false) => reject_stale_queue_action(&events, &config.session),
                    Err(error) => report_queue_persist_failed(
                        &events,
                        &config.home,
                        &config.session,
                        QueuePersistAction::Move,
                        &error,
                    ),
                }
            }
            UserAction::Interrupt | UserAction::StopProcesses { .. } => {
                let _ = events.send(HostEvent::Status {
                    phase: "not_ready".into(),
                    queue_len: config.session.pending_queue.len(),
                    detail: Some("nothing to interrupt; no agent turn is attached".into()),
                });
            }
            UserAction::SendQueuedNow { .. } | UserAction::ResumeQueueDrain => {
                let _ = events.send(HostEvent::Status {
                    phase: "not_ready".into(),
                    queue_len: config.session.pending_queue.len(),
                    detail: Some("queue delivery requires an attached canonical agent".into()),
                });
            }
            UserAction::ClearSession => {
                if let Err(error) = commit_detached_clear(&mut config.session, |candidate| {
                    config.store.save(candidate)
                }) {
                    let _ = events.notice(TranscriptRow::error(
                        NoticeScope::Session,
                        format!(
                            "clear session was not saved; previous session remains active: {error}"
                        ),
                    ));
                    continue;
                }
                let _ = events.send(HostEvent::ClearView);
                let _ = events.send(project_session_snapshot(&config.session));
                emit_shell_empty_state(&events, &config);
                send_shell_binding(&events, &config);
                let _ = events.notice(TranscriptRow::info(
                    NoticeScope::Session,
                    format!("new session {} (still not ready)", config.session.id),
                ));
                log_event(
                    log.as_ref(),
                    &config.session.id,
                    SessionEvent::new("session.cleared").str("id", &config.session.id),
                );
                if recovery_blocked {
                    if let Some(publication) = clear_publication
                        && let Ok(mut published) = publication.lock()
                    {
                        *published = Some(config.session.id.clone());
                    }
                    let _ = events.send(HostEvent::ReattachReady);
                    break;
                }
            }
            UserAction::Compact => {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Compaction,
                    "compact unavailable: no exact Plakous session is attached; source unchanged",
                ));
            }
            UserAction::RefreshTemplates => {
                super::surface::publish_template_readiness(
                    config.surface.as_deref(),
                    &config.ready_catalog.accounts,
                    &events,
                );
            }
            UserAction::ResumeSession { id: None } => {
                handle_resume_picker_shell(&config, &events);
            }
            UserAction::ResumeSession { id: Some(id) } => {
                handle_resume_load_shell(&mut config, &events, &id);
            }
            UserAction::ResumeSessionHandoff { .. } => {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Session,
                    "resume relaunch must be handled by the terminal owner",
                ));
            }
            UserAction::ForkSession {
                account,
                compact: _,
            } => {
                let _ = account;
                handle_fork_shell(&events);
            }
            // The one product-intent arm; see the idle loop's twin. The cached
            // allowance below is the shell's own projection.
            UserAction::Extension(intent) => {
                if super::surface::needs_usage(config.surface.as_deref(), &intent) {
                    report_unavailable_context(&events);
                    request_subscription_usage_cached(
                        &held_usage,
                        &session_ledger,
                        &events,
                        &subscription_usage_refresh,
                        config.subscriptions.clone(),
                    );
                }
                let effect = super::surface::dispatch(
                    config.surface.as_deref(),
                    intent,
                    super::surface::SurfaceFacts {
                        session: Some(super::surface::session_facts(&config.session, None)),
                        phase: super::surface::SurfacePhase::Detached,
                        attached_one_shot: false,
                        catalog: &config.ready_catalog.accounts,
                        queue_len: config.session.pending_queue.len(),
                    },
                    &events,
                );
                let Some(fork) = effect.profile_fork else {
                    continue;
                };
                // Nothing to fork: an unattached session with no conversation
                // takes the profile's root, and the agent attaches on it.
                if !config.session.transcript.is_empty() {
                    fork.refuse(
                        "this session is not attached; /attach it before forking onto a profile",
                    );
                    continue;
                }
                match handle_shell_set_model(
                    &mut config,
                    &events,
                    log.as_ref(),
                    &fork.account,
                    &fork.model,
                    fork.effort.as_deref(),
                    fork.extra.as_deref(),
                    fork.composition,
                    fork.protocol,
                ) {
                    ShellBindingOutcome::Committed => {
                        (fork.finish)(super::surface::ProfileForkOutcome::Bound);
                        let _ = events.send(HostEvent::ReattachReady);
                        break;
                    }
                    ShellBindingOutcome::RecoveryRequired => {
                        fork.refuse("this session requires recovery before it can bind a profile");
                        break;
                    }
                    ShellBindingOutcome::Refused => {
                        fork.refuse("the profile's root could not be bound to this session");
                    }
                }
            }
            UserAction::RetryAttach => {
                let _ = events.send(HostEvent::Activity {
                    label: "resolving account/model".into(),
                    active: true,
                });
            }
            UserAction::RefreshReadiness => {
                refresh_credential_readiness(&mut config, &events);
            }
            UserAction::OpenAgentTranscript { agent_id } => {
                let _ = events.send(HostEvent::AgentTranscript(
                    plakous_host_protocol::views::AgentTranscript {
                        agent_id,
                        rows: Vec::new(),
                        live_turn_id: None,
                        note: Some("no session is attached yet".to_owned()),
                    },
                ));
            }
            UserAction::CloseAgentTranscript => {}
            UserAction::OpenTopologyNavigator
            | UserAction::RefreshTopologyTree
            | UserAction::ResumeTopologyTree
            | UserAction::InterruptTopologyNode { .. }
            | UserAction::CloseTopologyNode { .. }
            | UserAction::StopAllTopology => {
                crate::topology_navigator::emit_navigator_unavailable(
                    |event| {
                        let _ = events.send(event);
                    },
                    "agent tree navigator requires an attached canonical session",
                );
            }
            UserAction::InvokeTool { .. } => {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Tree,
                    "invoke_tool requires an attached canonical session",
                ));
            }
            UserAction::RequestCallDetail {
                call_id,
                agent_id,
                session_id,
            } => {
                // Same rule as an attached host: a request for some other
                // session is not this shell's to answer.
                if session_id == config.session.id {
                    let _ = events.send(HostEvent::CallDetail(crate::call_detail::unavailable(
                        call_id,
                        agent_id,
                        session_id,
                        "call detail requires an attached canonical session".into(),
                    )));
                }
            }
        }
    }
}

pub(super) fn commit_detached_clear(
    session: &mut SessionRecord,
    save: impl FnOnce(&mut SessionRecord) -> Result<(), String>,
) -> Result<(), String> {
    let mut next = SessionRecord::new(session.workspace_root.clone());
    next.account.clone_from(&session.account);
    next.model.clone_from(&session.model);
    next.effort.clone_from(&session.effort);
    next.composition = session.composition;
    next.target_selection.clone_from(&session.target_selection);
    next.touch();
    save(&mut next)?;
    *session = next;
    Ok(())
}

pub(super) fn emit_shell_empty_state<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &ShellHostConfig<P>,
) {
    let account = config.session.account.as_deref().unwrap_or("-");
    let model = config.session.model.as_deref().unwrap_or("-");
    let effort = config.session.effort.as_deref().unwrap_or("-");
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Session,
        format!("agent not attached · account={account} model={model} effort={effort}"),
    ));
    if config.reason.starts_with("resolve_failed") {
        let detail = config
            .reason
            .strip_prefix("resolve_failed:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("OAuth model resolve failed");
        let _ = events.notice(TranscriptRow::info(
            NoticeScope::Session,
            format!("attach failed: {detail}"),
        ));
        let _ = events.notice(TranscriptRow::info(
            NoticeScope::Session,
            attach_repair_guidance(detail).to_owned(),
        ));
    } else {
        match config.reason.as_str() {
            "missing_defaults" => {
                let _ = events.notice(TranscriptRow::info(
                    NoticeScope::Session,
                    "choose a profile with /agents; the agent attaches automatically",
                ));
            }
            "effort_required" => {
                let _ = events.notice(TranscriptRow::info(NoticeScope::Session, format!(
                    "model={model} needs an explicit effort — choose a profile whose root names one (/agents)"
                )));
            }
            other => {
                let _ = events.notice(TranscriptRow::info(
                    NoticeScope::Session,
                    format!("reason: {other} — /agents · /attach"),
                ));
            }
        }
    }
}

pub(super) fn attach_repair_guidance(detail: &str) -> &'static str {
    if detail.contains("product JavaScript host is unavailable")
        || detail.contains("JavaScript host is unavailable")
        || detail.contains("Code Mode sidecar is unavailable")
    {
        "fix: set PLAK_JS_HOST or place plakous-js-host beside plak, then /attach"
    } else if detail.contains("product Python host is unavailable")
        || detail.contains("Python host is unavailable")
    {
        "fix: set PLAK_MONTY_WORKER or place monty beside plak, then /attach"
    } else {
        "fix: run plak login in a shell, then choose a profile with /agents; the agent attaches automatically"
    }
}

pub(super) fn send_shell_binding<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &ShellHostConfig<P>,
) {
    let _ = events.send(HostEvent::Binding {
        session_id: Some(config.session.id.clone()),
        account: config.session.account.clone(),
        model: config.session.model.clone(),
        effort: config.session.effort.clone(),
        extra: (config.session.target_selection == TargetSelection::NativePriority)
            .then(|| "priority".to_owned()),
        composition: config.session.composition.into(),
        protocol: match &config.session.target_selection {
            TargetSelection::OpenRouter { wire, .. } => match wire.selection {
                ProtocolSelection::Auto => plakous_host_protocol::ProtocolSelection::Auto,
                ProtocolSelection::Chat => plakous_host_protocol::ProtocolSelection::Chat,
                ProtocolSelection::Responses => plakous_host_protocol::ProtocolSelection::Responses,
                ProtocolSelection::Messages => plakous_host_protocol::ProtocolSelection::Messages,
            },
            TargetSelection::Platform { provider } => match provider.protocol() {
                crate::binding::ResolvedProtocol::Responses => {
                    plakous_host_protocol::ProtocolSelection::Responses
                }
                crate::binding::ResolvedProtocol::Messages => {
                    plakous_host_protocol::ProtocolSelection::Messages
                }
                crate::binding::ResolvedProtocol::Chat => {
                    plakous_host_protocol::ProtocolSelection::Chat
                }
            },
            TargetSelection::Native | TargetSelection::NativePriority => {
                plakous_host_protocol::ProtocolSelection::Auto
            }
        },
        effective_presentation: crate::agent::effective_presentation_label(
            config.session.composition,
            config
                .session
                .account
                .as_deref()
                .and_then(oauth_provider_for_account),
            None,
        )
        .to_owned(),
    });
}

pub(super) fn refuse_not_ready<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &ShellHostConfig<P>,
    log: Option<&SessionLog>,
) {
    log_event(
        log,
        &config.session.id,
        SessionEvent::new("turn.refused").str("reason", "not_ready"),
    );
    let _ = events.notice(TranscriptRow::info(NoticeScope::Session,
        "not ready — choose a profile with /agents and the agent attaches automatically. Or /attach."
            ,
    ));
    let _ = events.send(HostEvent::Status {
        phase: "not_ready".into(),
        queue_len: 0,
        detail: Some(format!("prompt refused · {}", config.reason)),
    });
}

impl<P: plakous_host_protocol::ProtocolExtension> ProductAuthHost<P> for ShellHostConfig<P> {
    fn replace_ready_catalog(&mut self, catalog: plakous_host_protocol::views::ReadyCatalog) {
        self.ready_catalog = catalog;
    }

    fn ready_catalog(&self) -> &plakous_host_protocol::views::ReadyCatalog {
        &self.ready_catalog
    }

    fn surface(&self) -> Option<&dyn SurfaceHandler<P>> {
        self.surface.as_deref()
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> ProductAuthHost<P> for HostConfig<P> {
    fn replace_ready_catalog(&mut self, catalog: plakous_host_protocol::views::ReadyCatalog) {
        self.ready_catalog = catalog;
    }

    fn ready_catalog(&self) -> &plakous_host_protocol::views::ReadyCatalog {
        &self.ready_catalog
    }

    fn surface(&self) -> Option<&dyn SurfaceHandler<P>> {
        self.surface.as_deref()
    }
}

/// Re-read credential readiness and republish both projections (spec 02 §5.5).
pub(super) fn refresh_credential_readiness<P: plakous_host_protocol::ProtocolExtension>(
    host: &mut impl ProductAuthHost<P>,
    events: &Sender<P>,
) {
    let refreshed = host.surface().map_or_else(
        || Err("no product surface is attached".to_owned()),
        SurfaceHandler::refresh_readiness,
    );
    match refreshed {
        Ok((providers, model_sections)) => {
            host.replace_ready_catalog(plakous_host_protocol::views::ReadyCatalog::new(
                model_sections,
            ));
            let _ = events.send(HostEvent::ProviderSnapshot { providers });
        }
        Err(error) => {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Settings,
                format!("provider readiness refresh failed: {error}"),
            ));
        }
    }
    // A credential change moves template readiness, so the Agents
    // projection is republished from the freshly read catalog.
    let catalog = host.ready_catalog().accounts.clone();
    super::surface::publish_template_readiness(host.surface(), &catalog, events);
}

/// The target selection for a profile root: Platform for an API account,
/// the priority lane only for a Codex root on the fast lane, Native otherwise.
/// The target a profile's root names: its account's direct API route, the
/// `OpenRouter` gateway on the root's wire, or a native subscription (Codex
/// alone takes the priority lane).
pub(super) fn profile_target_selection(
    account: &str,
    requested_tier: Option<&str>,
    protocol: Option<ProtocolSelection>,
) -> Result<TargetSelection, &'static str> {
    if account == "openrouter" {
        if requested_tier.is_some_and(|tier| tier != "standard") {
            return Err("model: provider-specific option is not admitted for an OpenRouter target");
        }
        return Ok(TargetSelection::OpenRouter {
            wire: WireBinding::resolve(protocol.unwrap_or(ProtocolSelection::Auto)),
            routing: OpenRouterRoutingSelection::GatewayManaged,
        });
    }
    if protocol.is_some_and(|protocol| protocol != ProtocolSelection::Auto) {
        return Err("model: a protocol is admitted only for an OpenRouter target");
    }
    if let Some(provider) = crate::binding::PlatformProvider::for_account(account) {
        if requested_tier.is_some_and(|tier| tier != "standard") {
            return Err("model: provider-specific option is not admitted for this API target");
        }
        return Ok(TargetSelection::Platform { provider });
    }
    match requested_tier {
        None | Some("standard") => Ok(TargetSelection::Native),
        Some("priority") if account == "codex" => Ok(TargetSelection::NativePriority),
        Some(_) => Err("model: provider-specific option is not admitted for this target"),
    }
}

/// The effort and composition an API root binds with, as a new session
/// started from the same template would: `OpenRouter` carries no effort and
/// no native-ptc; a direct API account takes the template's effort, which must
/// be on its model's published ladder (nothing is defaulted), and the
/// composition its enrolled model row admits (`crate::platform::api_composition`).
pub(super) fn api_root_binding(
    target: &TargetSelection,
    model: &str,
    effort: Option<&str>,
    composition: crate::binding::CompositionSelection,
) -> Result<(Option<String>, crate::binding::CompositionSelection), &'static str> {
    use crate::binding::CompositionSelection;
    let effort = match target {
        TargetSelection::OpenRouter { .. } => None,
        TargetSelection::Platform { provider } => {
            let ladder = crate::platform::platform_model(*provider, model)
                .map(|row| crate::platform::effort_names(&row.efforts))
                .unwrap_or_default();
            match effort.and_then(crate::effort::parse) {
                Some(effort) if ladder.contains(&effort) => Some(effort.to_owned()),
                _ => return Err("an API root takes an effort its model publishes"),
            }
        }
        TargetSelection::Native | TargetSelection::NativePriority => {
            return Err("a native target is not an API root");
        }
    };
    let composition = match (target, composition) {
        (TargetSelection::Platform { provider }, _) => {
            crate::platform::api_composition(*provider, model, composition)?
        }
        (_, CompositionSelection::FollowCatalog | CompositionSelection::Direct) => {
            CompositionSelection::Direct
        }
        (_, CompositionSelection::JsPtc | CompositionSelection::PyPtc) => composition,
        (_, CompositionSelection::NativePtc) => {
            return Err(crate::binding::OPENROUTER_NATIVE_PTC_REFUSAL);
        }
    };
    Ok((effort, composition))
}

pub(super) fn handle_shell_set_model<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut ShellHostConfig<P>,
    events: &Sender<P>,
    log: Option<&SessionLog>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    extra: Option<&str>,
    composition: Option<crate::binding::CompositionSelection>,
    protocol: Option<ProtocolSelection>,
) -> ShellBindingOutcome {
    let model = model.trim().to_owned();
    if model.is_empty() {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Settings,
            "model: empty id",
        ));
        return ShellBindingOutcome::Refused;
    }
    let account = account.trim().to_ascii_lowercase();
    if account.is_empty() {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Settings,
            "account: empty id",
        ));
        return ShellBindingOutcome::Refused;
    }
    let mut candidate = config.session.clone();
    candidate.account = Some(account.clone());
    candidate.model = Some(model.clone());
    let effort = effort
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let effort = effort.or_else(|| {
        candidate
            .account
            .as_deref()
            .and_then(|account| {
                config
                    .ready_catalog
                    .accounts
                    .iter()
                    .find(|section| section.ready && section.account == account)
            })
            .and_then(|section| section.entries.iter().find(|entry| entry.id == model))
            .filter(|entry| entry.effort_options.iter().any(|value| value == "medium"))
            .map(|_| "medium".to_owned())
    });
    candidate.effort.clone_from(&effort);
    let binding = match config.binding.open() {
        Ok(binding) => binding,
        Err(error) => {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Settings,
                format!("model: config load failed: {error}; source was unchanged"),
            ));
            return ShellBindingOutcome::Refused;
        }
    };
    candidate.composition = composition.unwrap_or_else(|| {
        binding.composition_for(candidate.account.as_deref(), candidate.model.as_deref())
    });
    candidate.target_selection = match profile_target_selection(&account, extra, protocol) {
        Ok(selection) => selection,
        Err(message) => {
            let _ = events.notice(TranscriptRow::error(NoticeScope::Settings, message));
            return ShellBindingOutcome::Refused;
        }
    };
    let effort = if candidate.target_selection.api_target().is_some() {
        match api_root_binding(
            &candidate.target_selection,
            &model,
            effort.as_deref(),
            candidate.composition,
        ) {
            Ok((effort, composition)) => {
                candidate.effort.clone_from(&effort);
                candidate.composition = composition;
                effort
            }
            Err(message) => {
                let _ = events.notice(TranscriptRow::error(NoticeScope::Settings, message));
                return ShellBindingOutcome::Refused;
            }
        }
    } else {
        effort
    };
    let mut root = binding.current().unwrap_or_default();
    root.account = account;
    root.model = model.clone();
    root.effort.clone_from(&effort);
    root.extra = (candidate.target_selection == TargetSelection::NativePriority)
        .then(|| "priority".to_owned());
    root.composition = Some(candidate.composition);
    root.protocol = match &candidate.target_selection {
        TargetSelection::OpenRouter { wire, .. } => Some(wire.selection),
        _ => None,
    };
    let outcome = commit_shell_binding_candidate(config, events, "model", candidate, move || {
        binding.commit(root)
    });
    if outcome != ShellBindingOutcome::Committed {
        return outcome;
    }
    send_shell_binding(events, config);
    log_event(
        log,
        &config.session.id,
        SessionEvent::new("binding.model")
            .str("model", &model)
            .opt_str("effort", effort.as_deref()),
    );
    let effort_note = effort
        .as_deref()
        .map(|e| format!(" effort={e}"))
        .unwrap_or_default();
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Settings,
        format!("model → `{model}`{effort_note} (session + config)"),
    ));
    maybe_note_ready_hint(config, events);
    ShellBindingOutcome::Committed
}

/// Writes a pre-attach shell binding into the `[root] agent` template. A
/// partial tuple is not an expressible template, so it
/// drops the root bind instead of persisting half a construction.
pub(super) fn commit_shell_binding_candidate<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut ShellHostConfig<P>,
    events: &Sender<P>,
    label: &str,
    candidate: SessionRecord,
    save_config: impl FnOnce() -> Result<(), RootBindingSaveError>,
) -> ShellBindingOutcome {
    let mut prior = config.session.clone();
    let mut candidate = candidate;
    if let Err(error) = config.store.save(&mut candidate) {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Settings,
            format!("{label}: session write failed: {error}; source was unchanged"),
        ));
        return ShellBindingOutcome::Refused;
    }
    match save_config() {
        Ok(()) => config.session = candidate,
        Err(RootBindingSaveError::BeforePublication(error)) => {
            prior.store_revision = candidate.store_revision;
            let diagnostic = match config.store.save(&mut prior) {
                Ok(()) => {
                    config.session = prior;
                    format!("{label}: config write failed: {error}; session change rolled back")
                }
                Err(rollback) => format!(
                    "{label}: config write failed: {error}; session rollback failed: {rollback}; recovery required"
                ),
            };
            let _ = events.notice(TranscriptRow::error(NoticeScope::Settings, diagnostic));
            return ShellBindingOutcome::Refused;
        }
        Err(RootBindingSaveError::PublishedDurabilityUncertain(error)) => {
            config.session = candidate;
            send_shell_binding(events, config);
            let _ = events.notice(TranscriptRow::error(NoticeScope::Settings, format!(
                "{label}: config was published but directory sync failed: {error}; durability uncertain; recovery required"
            )));
            return ShellBindingOutcome::RecoveryRequired;
        }
    }
    ShellBindingOutcome::Committed
}

pub(super) fn maybe_note_ready_hint<P: plakous_host_protocol::ProtocolExtension>(
    config: &ShellHostConfig<P>,
    events: &Sender<P>,
) {
    match (
        config.session.account.as_deref(),
        config.session.model.as_deref(),
    ) {
        (Some(a), Some(m)) => {
            let effort = config.session.effort.as_deref().unwrap_or("—");
            let _ = events.notice(TranscriptRow::info(
                NoticeScope::Settings,
                format!("binding ready (account={a} model={m} effort={effort}) — attaching agent…"),
            ));
            let _ = events.send(HostEvent::Status {
                phase: "not_ready".into(),
                queue_len: 0,
                detail: Some("reattach pending".into()),
            });
        }
        _ => {
            let _ = events.send(HostEvent::Status {
                phase: "not_ready".into(),
                queue_len: 0,
                detail: Some("choose a profile with /agents".into()),
            });
        }
    }
}

pub(super) fn handle_resume_picker_shell<P: plakous_host_protocol::ProtocolExtension>(
    config: &ShellHostConfig<P>,
    events: &Sender<P>,
) {
    match config.store.list_for_workspace(&config.cwd) {
        Ok(sessions) => {
            emit_resume_choices(events, sessions, |_| false);
        }
        Err(err) => {
            let _ = events.notice(TranscriptRow::info(
                NoticeScope::Session,
                format!("resume: {err}"),
            ));
        }
    }
}

pub(super) fn handle_resume_load_shell<P: plakous_host_protocol::ProtocolExtension>(
    _config: &mut ShellHostConfig<P>,
    events: &Sender<P>,
    id: &str,
) {
    let _ = events.notice(TranscriptRow::error(NoticeScope::Session, format!(
        "exact resume of session {id} requires the canonical cross-process Plakous store; current session was unchanged"
    )));
}

pub(super) fn handle_fork_shell<P: plakous_host_protocol::ProtocolExtension>(events: &Sender<P>) {
    let _ = events.notice(TranscriptRow::error(
        NoticeScope::Session,
        "canonical fork unavailable: no exact Plakous session is attached; source unchanged",
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostConfig = crate::runtime::HostConfig<TestExtension>;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;
    type ShellHostConfig = crate::runtime::ShellHostConfig<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::binding::CompositionSelection as ProductComposition;
    use crate::binding::TargetSelection;
    use crate::sessions::SessionRecord;
    use plakous::agent::ReasoningEffort;
    use plakous::subscriptions::NativeProvider;

    /// An OpenRouter root binds the wire its template names on the
    /// gateway-managed route; a direct API root binds its account's route.
    /// Effort and composition follow what a new session from that template
    /// binds.
    #[test]
    fn a_profile_root_names_its_api_or_openrouter_target_exactly() {
        let openrouter =
            profile_target_selection("openrouter", None, Some(ProtocolSelection::Responses))
                .unwrap();
        assert_eq!(
            openrouter,
            TargetSelection::OpenRouter {
                wire: WireBinding::resolve(ProtocolSelection::Responses),
                routing: OpenRouterRoutingSelection::GatewayManaged,
            }
        );
        assert_eq!(
            profile_target_selection("openrouter", None, None).unwrap(),
            TargetSelection::OpenRouter {
                wire: WireBinding::resolve(ProtocolSelection::Auto),
                routing: OpenRouterRoutingSelection::GatewayManaged,
            }
        );
        assert!(profile_target_selection("openrouter", Some("priority"), None).is_err());
        assert!(profile_target_selection("claude", None, Some(ProtocolSelection::Chat)).is_err());
        let platform = profile_target_selection("openai", None, None).unwrap();
        assert_eq!(
            platform,
            TargetSelection::Platform {
                provider: crate::binding::PlatformProvider::OpenAi
            }
        );

        assert_eq!(
            api_root_binding(
                &openrouter,
                "openai/gpt-5.5",
                Some("high"),
                ProductComposition::PyPtc
            ),
            Ok((None, ProductComposition::PyPtc))
        );
        assert_eq!(
            api_root_binding(
                &openrouter,
                "openai/gpt-5.5",
                None,
                ProductComposition::FollowCatalog
            ),
            Ok((None, ProductComposition::Direct))
        );
        // The template's effort binds as written; nothing defaults it.
        assert_eq!(
            api_root_binding(&platform, "gpt-5.6-luna", None, ProductComposition::JsPtc),
            Err("an API root takes an effort its model publishes")
        );
        assert_eq!(
            api_root_binding(
                &platform,
                "gpt-5.6-luna",
                Some("medium"),
                ProductComposition::JsPtc
            ),
            Ok((Some("medium".to_owned()), ProductComposition::JsPtc))
        );
        assert_eq!(
            api_root_binding(
                &platform,
                "gpt-5.6-luna",
                Some("low"),
                ProductComposition::Direct
            ),
            Ok((Some("low".to_owned()), ProductComposition::Direct))
        );
        assert_eq!(
            api_root_binding(
                &platform,
                "gpt-5.6-luna",
                Some("high"),
                ProductComposition::Direct
            ),
            Ok((Some("high".to_owned()), ProductComposition::Direct))
        );
        // `ultra` is a Codex client level, not an API effort.
        assert!(
            api_root_binding(
                &platform,
                "gpt-5.6-luna",
                Some("ultra"),
                ProductComposition::Direct
            )
            .is_err()
        );
        // The OpenAI API follows its enrolled tool-mode table: a CodeModeOnly
        // row binds native-ptc by default and admits it explicitly.
        assert_eq!(
            api_root_binding(
                &platform,
                "gpt-5.6-luna",
                Some("low"),
                ProductComposition::NativePtc
            ),
            Ok((Some("low".to_owned()), ProductComposition::NativePtc))
        );
        assert_eq!(
            api_root_binding(
                &platform,
                "gpt-6-sol",
                Some("medium"),
                ProductComposition::FollowCatalog
            ),
            Ok((Some("medium".to_owned()), ProductComposition::NativePtc))
        );
        assert!(
            api_root_binding(
                &platform,
                "gpt-3",
                Some("low"),
                ProductComposition::NativePtc
            )
            .is_err()
        );
        for provider in [
            crate::binding::PlatformProvider::Anthropic,
            crate::binding::PlatformProvider::Xai,
        ] {
            let model = crate::platform::platform_models(provider)[0].model.clone();
            let target = TargetSelection::Platform { provider };
            assert!(
                api_root_binding(&target, &model, Some("low"), ProductComposition::NativePtc)
                    .is_err(),
                "{provider:?}"
            );
            assert_eq!(
                api_root_binding(
                    &target,
                    &model,
                    Some("low"),
                    ProductComposition::FollowCatalog
                ),
                Ok((Some("low".to_owned()), ProductComposition::Direct)),
                "{provider:?}"
            );
        }
        assert!(
            api_root_binding(
                &openrouter,
                "openai/gpt-5.5",
                None,
                ProductComposition::NativePtc
            )
            .is_err()
        );
        assert!(
            api_root_binding(
                &TargetSelection::Native,
                "gpt-5.6-luna",
                None,
                ProductComposition::Direct
            )
            .is_err()
        );
    }

    #[test]
    fn native_model_transition_selects_the_codex_fast_lane_only() {
        assert_eq!(
            profile_target_selection("codex", None, None).unwrap(),
            TargetSelection::Native
        );
        assert_eq!(
            profile_target_selection("codex", Some("priority"), None).unwrap(),
            TargetSelection::NativePriority
        );
        assert_eq!(
            profile_target_selection("grok", Some("priority"), None).unwrap_err(),
            "model: provider-specific option is not admitted for this target"
        );
        assert_eq!(
            profile_target_selection("openai", Some("priority"), None).unwrap_err(),
            "model: provider-specific option is not admitted for this API target"
        );
    }

    #[test]
    fn shell_binding_config_failure_rolls_back_session_and_live_candidate() {
        let root = tempfile::tempdir().unwrap();
        let home_dir = root.path().join("p");
        let record_store = Arc::new(crate::session_store::MemoryStore::new());
        let binding_document = test_kit::MemoryBindingDocument::empty();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.account = Some("codex".into());
        record_store.save(&mut session).unwrap();
        let id = session.id.clone();
        let mut config = ShellHostConfig {
            surface: test_kit::recording_surface(),
            binding: Arc::new(Arc::clone(&binding_document)) as Arc<dyn RootBindingDocument>,
            subscriptions: None,
            openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
            home: SessionHome::new(home_dir.join("sessions")),
            store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
            product_home: home_dir.clone(),
            session: session.clone(),
            cwd: root.path().to_path_buf(),
            reason: "test".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        };
        let mut candidate = session.clone();
        candidate.account = Some("grok".into());
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        assert_eq!(
            commit_shell_binding_candidate(&mut config, &events, "account", candidate, || {
                Err(RootBindingSaveError::BeforePublication(
                    "injected refusal".into(),
                ))
            }),
            ShellBindingOutcome::Refused
        );
        assert_eq!(config.session.account.as_deref(), Some("codex"));
        let persisted = config.store.load(&id).unwrap();
        assert_eq!(persisted.account.as_deref(), Some("codex"));
        let mut second = config.session.clone();
        second.account = Some("openai".into());
        assert_eq!(
            commit_shell_binding_candidate(&mut config, &events, "account", second.clone(), || {
                panic!("an incomplete API tuple must refuse before configuration publication")
            }),
            ShellBindingOutcome::Refused
        );
        assert_eq!(config.session.account.as_deref(), Some("codex"));
        assert_eq!(
            config.store.load(&id).unwrap().account.as_deref(),
            Some("codex")
        );
        second.model = Some("gpt-5.6-luna".into());
        second.effort = Some("low".into());
        second.composition = ProductComposition::Direct;
        second.target_selection = TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::OpenAi,
        };

        assert_eq!(
            commit_shell_binding_candidate(&mut config, &events, "account", second, || Ok(())),
            ShellBindingOutcome::Committed
        );
        assert_eq!(config.session.account.as_deref(), Some("openai"));
        assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(config.session.effort.as_deref(), Some("low"));
        assert_eq!(
            config.session.target_selection,
            TargetSelection::Platform {
                provider: crate::binding::PlatformProvider::OpenAi,
            }
        );

        assert_eq!(
            config.store.load(&id).unwrap().account.as_deref(),
            Some("openai")
        );
        assert!(rx.try_iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("injected refusal") && note.contains("rolled back")
        )));
    }

    #[test]
    fn shell_binding_session_failure_never_publishes_config_or_live_candidate() {
        let root = tempfile::tempdir().unwrap();
        let home_dir = root.path().join("p");
        // The session write fails: a binding that cannot be persisted must
        // refuse, and must not publish the configuration half either.
        let record_store = Arc::new(fault_store::FaultStore::refusing(
            "product session store is unreadable",
        ));
        let binding_document = test_kit::MemoryBindingDocument::empty();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.account = Some("codex".into());
        let mut config = ShellHostConfig {
            surface: test_kit::recording_surface(),
            binding: Arc::new(Arc::clone(&binding_document)) as Arc<dyn RootBindingDocument>,
            subscriptions: None,
            openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
            home: SessionHome::new(home_dir.join("sessions")),
            store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
            product_home: home_dir.clone(),
            session: session.clone(),
            cwd: root.path().to_path_buf(),
            reason: "test".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        };
        let mut candidate = session;
        candidate.account = Some("grok".into());
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let config_published = std::cell::Cell::new(false);

        assert_eq!(
            commit_shell_binding_candidate(&mut config, &events, "account", candidate, || {
                config_published.set(true);
                Ok(())
            }),
            ShellBindingOutcome::Refused
        );
        assert!(!config_published.get());
        assert_eq!(config.session.account.as_deref(), Some("codex"));
        assert!(rx.try_iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("session write failed") && note.contains("source was unchanged")
        )));
    }

    #[test]
    fn shell_binding_durability_uncertainty_keeps_published_authorities_aligned() {
        let root = tempfile::tempdir().unwrap();
        let home_dir = root.path().join("p");
        let record_store = Arc::new(crate::session_store::MemoryStore::new());
        let binding_document = test_kit::MemoryBindingDocument::empty();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.account = Some("codex".into());
        record_store.save(&mut session).unwrap();
        let id = session.id.clone();
        let mut config = ShellHostConfig {
            surface: test_kit::recording_surface(),
            binding: Arc::new(Arc::clone(&binding_document)) as Arc<dyn RootBindingDocument>,
            subscriptions: None,
            openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
            home: SessionHome::new(home_dir.join("sessions")),
            store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
            product_home: home_dir.clone(),
            session: session.clone(),
            cwd: root.path().to_path_buf(),
            reason: "test".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        };
        let mut candidate = session;
        candidate.account = Some("grok".into());
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        assert_eq!(
            commit_shell_binding_candidate(&mut config, &events, "account", candidate, || {
                Err(RootBindingSaveError::PublishedDurabilityUncertain(
                    "injected directory sync failure".into(),
                ))
            }),
            ShellBindingOutcome::RecoveryRequired
        );
        assert_eq!(config.session.account.as_deref(), Some("grok"));
        assert_eq!(
            config.store.load(&id).unwrap().account.as_deref(),
            Some("grok")
        );
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.contains("durability uncertain") && note.contains("recovery required")
        )));
        assert!(observed.iter().any(|event| matches!(
            event,
            HostEvent::Binding { account: Some(account), .. } if account == "grok"
        )));
        assert!(!observed.contains(&HostEvent::ReattachReady));
    }

    #[test]
    fn attached_oauth_ready_refresh_never_rebinds_canonical_agent() {
        let root = tempfile::tempdir().unwrap();
        let home_dir = root.path().join("p");
        let record_store = Arc::new(crate::session_store::MemoryStore::new());
        let _binding_document = test_kit::MemoryBindingDocument::empty();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.account = Some("codex".into());
        session.model = Some("gpt-existing".into());
        let mut config = HostConfig {
            surface: test_kit::recording_surface(),
            auto_continue: true,
            topology_admission: None,
            state_roots: crate::agent::ProductStateRoots::with_doubles(
                home_dir.clone(),
                home_dir.join("state"),
            ),
            home: SessionHome::new(home_dir.join("sessions")),
            store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
            topology_enabled: true,
            product_home: home_dir.clone(),
            session,
            target: HostTargetAuthority::Injected {
                provider: NativeProvider::Codex,
            },
            model_id: "gpt-existing".into(),
            effort: Some(ReasoningEffort::High),
            effort_explicit: true,
            compaction_enabled: true,
            max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: root.path().to_path_buf(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
            resume_publication: None,
            test_injection: None,
            ready_path: ReadyPathTiming::default(),
            seed_history: None,
            source_releases: SourceReleases::default(),
        };
        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);

        // A readiness refresh re-reads external credential authority only: the
        // attached branch keeps its exact provider/model binding.
        refresh_credential_readiness(&mut config, &events);
        assert_eq!(config.target.native_provider(), Some(NativeProvider::Codex));
        assert_eq!(config.session.account.as_deref(), Some("codex"));
        assert_eq!(config.session.model.as_deref(), Some("gpt-existing"));
        assert_eq!(config.model_id, "gpt-existing");
    }
}
