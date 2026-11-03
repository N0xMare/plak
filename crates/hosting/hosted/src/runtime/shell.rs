//! Detached shell host loop and its binding intents.

use crate::sessions::queue::{drop_last_mutable, edit_mutable, move_mutable, remove_mutable};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{
    AttachFailure, DetachedReason, HostFailure, HostPhase, HostStatusDetail,
};

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
    APPROVAL_REFUSED_NOTE, HeldUsage, HostConfig, IDLE_OBSERVE_POLL_INTERVAL, ProductAuthHost,
    RootBindingSaveError, Sender, SessionLedger, ShellBindingOutcome, ShellHostConfig,
    SubscriptionUsageRefresh, SurfaceHandler, UserAction, queue,
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
    let recovery_blocked = matches!(config.reason, DetachedReason::RecoveryRequired(_));
    log_event(
        log.as_ref(),
        &config.session.id,
        SessionEvent::new("host.shell")
            .str("reason", config.reason.kind())
            .opt_str("account", config.session.account.as_deref())
            .opt_str("model", config.session.model.as_deref()),
    );

    publish_shell_startup(&config, &events);
    let held_usage = HeldUsage::default();
    let session_ledger = SessionLedger::default();
    let subscription_usage_refresh = SubscriptionUsageRefresh::new(
        &events,
        std::sync::Arc::clone(&config.openrouter_credentials),
        held_usage.clone(),
        session_ledger.clone(),
    );
    let mut state = ShellState {
        held_usage,
        session_ledger,
        subscription_usage_refresh,
        readiness: ReadinessRefresh::new(),
        recovery_blocked,
        clear_publication,
    };

    loop {
        state.readiness.apply_settled(&mut config, &events);
        let delivery = match actions.recv_timeout(IDLE_OBSERVE_POLL_INTERVAL) {
            Ok(delivery) => delivery,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(started) = delivery.started() {
            let _ = events.send(started);
        }
        let (action, _acknowledgement) = delivery.into_parts();
        if state.recovery_blocked && recovery_blocked_action(&action) {
            refuse_detached_recovery_action(&events, &action);
            continue;
        }
        if matches!(
            handle_shell_action(&mut config, &events, log.as_ref(), &mut state, action),
            ShellStep::Quit
        ) {
            break;
        }
    }
}

struct ShellState<P: plakous_host_protocol::ProtocolExtension> {
    readiness: ReadinessRefresh,
    subscription_usage_refresh: SubscriptionUsageRefresh<P>,
    session_ledger: SessionLedger,
    held_usage: HeldUsage,
    recovery_blocked: bool,
    clear_publication: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
}

enum ShellStep {
    Continue,
    Quit,
}

fn publish_shell_startup<P: plakous_host_protocol::ProtocolExtension>(
    config: &ShellHostConfig<P>,
    events: &Sender<P>,
) {
    // The not-ready shell is reachable as a fallback *after* the canonical
    // attach published `SessionOpening` and then failed. Nothing else retires
    // that startup row, so the TUI kept its 80 ms spinner cadence alive for the
    // life of the process on a session that had stopped opening — a periodic
    // repaint with no demand behind it, and the dominant idle-CPU cost on any
    // host where attach cannot complete. Zero is truthful here: no session was
    // opened.
    let _ = events.send(ContentEvent::SessionOpened {
        session_open_ms: None,
    });
    emit_shell_empty_state(events, config);
    send_shell_binding(events, config);
    let _ = events.replace_transcript(project_session_snapshot(&config.session));
    emit_prompt_history(events, &config.session);
    emit_queue_snapshot(events, &config.session);
    let _ = events.send(ContentEvent::Status {
        phase: HostPhase::NotReady,
        queue_len: config.session.pending_queue.len(),
        detail: Some(HostStatusDetail::Detached {
            reason: config.reason.clone(),
        }),
    });
    let _ = events.send(ContentEvent::SetBusy(false));
}

#[allow(
    clippy::too_many_lines,
    reason = "Shell action dispatch preserves the individual model, profile, queue, compaction, and navigation refusal paths."
)]
fn handle_shell_action<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut ShellHostConfig<P>,
    events: &Sender<P>,
    log: Option<&SessionLog>,
    state: &mut ShellState<P>,
    action: UserAction<P>,
) -> ShellStep {
    match action {
        UserAction::Recover => {
            let _ = events.notice(
                TranscriptRow::error(
                    NoticeScope::Session,
                    "no session is attached; reopening recovers any interrupted turn",
                )
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::Attach {}),
            );
        }
        UserAction::Quit => return ShellStep::Quit,
        UserAction::Background => {}
        action @ (UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_)) => {
            refuse_not_ready(events, config, log);
            reject_draft(events, action);
        }
        UserAction::DropQueued => {
            handle_shell_queue_edit(config, events, ShellQueueEdit::Drop);
        }
        UserAction::DeleteQueued { id, revision } => {
            handle_shell_queue_edit(config, events, ShellQueueEdit::Delete { id, revision });
        }
        UserAction::EditQueued { id, revision, text } => {
            handle_shell_queue_edit(config, events, ShellQueueEdit::Edit { id, revision, text });
        }
        UserAction::MoveQueued {
            id,
            revision,
            new_index,
        } => {
            handle_shell_queue_edit(
                config,
                events,
                ShellQueueEdit::Move {
                    id,
                    revision,
                    new_index,
                },
            );
        }
        UserAction::ResolveApproval { .. } => {
            // No turn is attached, so no decision was ever offered. The
            // host refuses without contacting any runtime.
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Approval,
                APPROVAL_REFUSED_NOTE,
            ));
        }
        UserAction::Interrupt | UserAction::StopProcesses { .. } => {
            let _ = events.send(ContentEvent::Status {
                phase: HostPhase::NotReady,
                queue_len: config.session.pending_queue.len(),
                detail: Some(HostStatusDetail::NoAttachedTurnToInterrupt),
            });
        }
        UserAction::SendQueuedNow { .. } | UserAction::ResumeQueueDrain => {
            let _ = events.send(ContentEvent::Status {
                phase: HostPhase::NotReady,
                queue_len: config.session.pending_queue.len(),
                detail: Some(HostStatusDetail::QueueDeliveryNeedsAttachment),
            });
        }
        UserAction::NewSession => {
            if let Err(error) = commit_detached_clear(&mut config.session, |candidate| {
                config.store.save(candidate).map_err(String::from)
            }) {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Session,
                    format!("new session was not saved; this session remains active: {error}"),
                ));
                return ShellStep::Continue;
            }
            send_shell_binding(events, config);
            let _ = events.send(ContentEvent::ClearView);
            let _ = events.replace_transcript(project_session_snapshot(&config.session));
            emit_shell_empty_state(events, config);
            let _ = events.notice(TranscriptRow::info(
                NoticeScope::Session,
                format!("new session {} (still not ready)", config.session.id),
            ));
            log_event(
                log,
                &config.session.id,
                SessionEvent::new("session.cleared").str("id", &config.session.id),
            );
            if state.recovery_blocked {
                if let Some(publication) = &state.clear_publication
                    && let Ok(mut published) = publication.lock()
                {
                    *published = Some(config.session.id.clone());
                }
                let _ = events.send(ContentEvent::ReattachReady);
                return ShellStep::Quit;
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
                events,
            );
        }
        UserAction::ResumeSession { id: None } => {
            handle_resume_picker_shell(config, events);
        }
        UserAction::ResumeSession { id: Some(id) } => {
            handle_resume_load_shell(config, events, &id);
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
            handle_fork_shell(events);
        }
        // The one product-intent arm; see the idle loop's twin. The cached
        // allowance below is the shell's own projection.
        UserAction::Extension(intent) => {
            return handle_shell_extension(config, events, log, state, intent);
        }
        UserAction::RetryAttach => {
            let _ = events.send(ContentEvent::Activity {
                label: "resolving account/model".into(),
                active: true,
            });
        }
        UserAction::RefreshReadiness => state.readiness.start(config),
        UserAction::OpenAgentTranscript { agent_id } => {
            super::child_live::send_agent_transcript(
                events,
                &agent_id,
                Err("no session is attached yet".to_owned()),
            );
        }
        UserAction::CloseAgentTranscript => events.retire_agent_transcript(),
        UserAction::ResyncTranscript { scope } => {
            events.resync_transcript(&scope);
        }
        UserAction::OpenTopologyNavigator
        | UserAction::RefreshTopologyTree
        | UserAction::ResumeTopologyTree
        | UserAction::InterruptTopologyNode { .. }
        | UserAction::CloseTopologyNode { .. }
        | UserAction::StopAllTopology
        | UserAction::RecoverTopologyNode { .. } => {
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
                let _ = events.send(ContentEvent::CallDetail(crate::call_detail::unavailable(
                    call_id,
                    agent_id,
                    session_id,
                    "call detail requires an attached canonical session".into(),
                )));
            }
        }
    }
    ShellStep::Continue
}

enum ShellQueueEdit {
    Drop,
    Delete {
        id: String,
        revision: u64,
    },
    Edit {
        id: String,
        revision: u64,
        text: String,
    },
    Move {
        id: String,
        revision: u64,
        new_index: usize,
    },
}

fn handle_shell_queue_edit<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut ShellHostConfig<P>,
    events: &Sender<P>,
    action: ShellQueueEdit,
) {
    match action {
        ShellQueueEdit::Drop => {
            match commit_session_update(
                &*config.store,
                &config.home,
                &mut config.session,
                drop_last_mutable,
            ) {
                Ok(true) => emit_queue_snapshot(events, &config.session),
                Ok(false) => {}
                Err(error) => report_queue_persist_failed(
                    events,
                    &config.home,
                    &config.session,
                    QueuePersistAction::Drop,
                    &error,
                ),
            }
        }
        ShellQueueEdit::Delete { id, revision } => {
            match commit_session_update(
                &*config.store,
                &config.home,
                &mut config.session,
                |candidate| remove_mutable(candidate, &id, revision),
            ) {
                Ok(true) => emit_queue_snapshot(events, &config.session),
                Ok(false) => reject_stale_queue_action(events, &config.session),
                Err(error) => report_queue_persist_failed(
                    events,
                    &config.home,
                    &config.session,
                    QueuePersistAction::Delete,
                    &error,
                ),
            }
        }
        ShellQueueEdit::Edit { id, revision, text } => {
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
                    emit_queue_snapshot(events, &config.session);
                    queue_edit_applied(events, id);
                }
                Ok(false) => {
                    emit_queue_snapshot(events, &config.session);
                    queue_edit_refused(events, id);
                    queue_refusal(
                        events,
                        "queue edit",
                        refusal.unwrap_or(queue::Refusal::Stale),
                    );
                }
                Err(error) => {
                    report_queue_persist_failed(
                        events,
                        &config.home,
                        &config.session,
                        QueuePersistAction::Edit,
                        &error,
                    );
                    queue_edit_refused(events, id);
                }
            }
        }
        ShellQueueEdit::Move {
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
                Ok(true) => emit_queue_snapshot(events, &config.session),
                Ok(false) => reject_stale_queue_action(events, &config.session),
                Err(error) => report_queue_persist_failed(
                    events,
                    &config.home,
                    &config.session,
                    QueuePersistAction::Move,
                    &error,
                ),
            }
        }
    }
}

fn handle_shell_extension<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut ShellHostConfig<P>,
    events: &Sender<P>,
    log: Option<&SessionLog>,
    state: &ShellState<P>,
    intent: P::Intent,
) -> ShellStep {
    if super::surface::needs_usage(config.surface.as_deref(), &intent) {
        report_unavailable_context(events);
        if super::surface::fresh_usage(config.surface.as_deref(), &intent) {
            state
                .subscription_usage_refresh
                .request_fresh(config.subscriptions.clone());
        } else {
            request_subscription_usage_cached(
                &state.held_usage,
                &state.session_ledger,
                events,
                &state.subscription_usage_refresh,
                config.subscriptions.clone(),
            );
        }
    }
    let with_tools = super::surface::needs_session_tools(config.surface.as_deref(), &intent);
    let effect = super::surface::dispatch(
        config.surface.as_deref(),
        intent,
        super::surface::SurfaceFacts {
            session: Some(super::surface::session_facts(
                &config.session,
                None,
                with_tools,
            )),
            phase: super::surface::SurfacePhase::Detached,
            attached_one_shot: false,
            catalog: &config.ready_catalog.accounts,
            queue_len: config.session.pending_queue.len(),
            usage_reads: state.subscription_usage_refresh.reads(),
        },
        events,
    );
    let Some(fork) = effect.profile_fork else {
        return ShellStep::Continue;
    };
    // Nothing to fork: an unattached session with no conversation
    // takes the profile's root, and the agent attaches on it.
    if !config.session.transcript.is_empty() {
        fork.refuse(
            crate::CliFailure::new(
                "this session is not attached; attach it before forking onto a profile",
            )
            .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::Attach {})
            .host_failure(),
        );
        return ShellStep::Continue;
    }
    match handle_shell_set_model(
        config,
        events,
        log,
        &fork.account,
        &fork.model,
        fork.effort.as_deref(),
        fork.extra.as_deref(),
        fork.composition,
        fork.protocol,
    ) {
        ShellBindingOutcome::Committed => {
            (fork.finish)(super::surface::ProfileForkOutcome::Bound);
            let _ = events.send(ContentEvent::ReattachReady);
            return ShellStep::Quit;
        }
        ShellBindingOutcome::RecoveryRequired => {
            fork.refuse("this session requires recovery before it can bind a profile");
            return ShellStep::Quit;
        }
        ShellBindingOutcome::Refused => {
            fork.refuse("the profile's root could not be bound to this session");
        }
    }
    ShellStep::Continue
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
    use plakous_host_protocol::notice_remedy::NoticeRemedy;
    let account = config.session.account.as_deref().unwrap_or("-");
    let model = config.session.model.as_deref().unwrap_or("-");
    let effort = config.session.effort.as_deref().unwrap_or("-");
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Session,
        format!("agent not attached · account={account} model={model} effort={effort}"),
    ));
    match &config.reason {
        DetachedReason::ResolveFailed(failure) | DetachedReason::RecoveryRequired(failure) => {
            let _ = events.notice(
                TranscriptRow::info(
                    NoticeScope::Session,
                    format!("attach failed: {}", failure.diagnostic.as_str()),
                )
                .with_optional_remedy(failure.remedy.as_ref()),
            );
            let _ = events.notice(attach_repair_notice(failure, account));
        }
        DetachedReason::MissingDefaults => {
            let _ = events.notice(
                TranscriptRow::info(
                    NoticeScope::Session,
                    "choose a profile; the agent attaches automatically",
                )
                .with_remedy(NoticeRemedy::ChooseProfile {}),
            );
        }
        DetachedReason::EffortRequired => {
            let _ = events.notice(TranscriptRow::info(NoticeScope::Session,
                format!("model={model} needs an explicit effort; choose a profile whose root supplies one")).with_remedy(NoticeRemedy::ChooseProfileEffort {}));
        }
        DetachedReason::CredentialUnusable(failure) => {
            let _ = events.notice(
                TranscriptRow::info(NoticeScope::Session, failure.diagnostic.as_str())
                    .with_optional_remedy(failure.remedy.as_ref()),
            );
        }
        DetachedReason::Other(diagnostic) => {
            let _ = events.notice(
                TranscriptRow::info(
                    NoticeScope::Session,
                    format!(
                        "reason: {}; select a profile or retry attachment after repair",
                        diagnostic.as_str()
                    ),
                )
                .with_remedy(NoticeRemedy::ChooseProfileOrAttach {}),
            );
        }
    }
}

pub(super) fn attach_repair_notice(failure: &HostFailure, original_account: &str) -> TranscriptRow {
    use AttachFailure as A;
    use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy as R};
    let account = AccountTarget::from_account(original_account);
    let (text, remedy) = match failure.cause {
        A::JavaScriptHostUnavailable => (
            "the JavaScript runtime artifact is unavailable; supply the configured runtime artifact, then retry attachment",
            R::RepairJavaScriptHost {},
        ),
        A::PythonHostUnavailable => (
            "the Python runtime artifact is unavailable; supply the configured runtime artifact, then retry attachment",
            R::RepairPythonHost {},
        ),
        A::LoginSelectionChanged => (
            "the live login-selection fence changed; explicitly carry this history under the current login; logging in to the same account cannot restore the old selection",
            R::CarryCurrentLogin {},
        ),
        A::AccountChanged => (
            "restore the original account for exact reopen, or explicitly carry history into a new session under the newly logged-in account",
            R::RestoreAccountOrRebind { account },
        ),
        A::BindingChanged => (
            "explicitly carry the full history into a new session under the current login; restoring the original account and configuration reopens it exactly instead; profile selection cannot repair a frozen binding",
            R::RestoreBindingOrCarry { account },
        ),
        A::CredentialRejected => (
            "the credential was rejected or its refresh outcome is unknown; restore usable credentials, then retry attachment; same-account login preserves the session where the route attests it",
            R::LoginThenAttach { account },
        ),
        A::CredentialUnavailable => (
            "no usable login is available; restore credentials, then retry attachment",
            R::LoginThenAttach { account },
        ),
        A::TreeHostedElsewhere => (
            "another plak process holds this session's agent tree; quit that plak, then retry attachment here; if it ran on another machine or is gone, `plak tree release --force <tree-id>` releases it",
            R::RepairThenAttach {},
        ),
        A::Other => (
            "address the attach failure above, then retry attachment; profile selection only chooses a different profile",
            R::RepairThenAttach {},
        ),
    };
    TranscriptRow::info(NoticeScope::Session, text)
        .with_remedy(failure.remedy.clone().unwrap_or(remedy))
}

pub(super) fn send_shell_binding<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &ShellHostConfig<P>,
) {
    let _ = events.send(ContentEvent::Binding {
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
    use plakous_host_protocol::notice_remedy::{AccountTarget, CredentialCondition, NoticeRemedy};

    log_event(
        log,
        &config.session.id,
        SessionEvent::new("turn.refused").str("reason", "not_ready"),
    );
    let row =
        match &config.reason {
            DetachedReason::CredentialUnusable(failure) => {
                let remedy = failure.remedy.clone().unwrap_or(
                    NoticeRemedy::RepairCredentialOrChooseProfile {
                        account: config
                            .session
                            .account
                            .as_deref()
                            .map_or(AccountTarget::Unspecified, AccountTarget::from_account),
                        condition: CredentialCondition::Unavailable,
                    },
                );
                TranscriptRow::info(
                    NoticeScope::Session,
                    format!(
                        "not ready — {}; restore usable credentials or choose another profile",
                        failure.diagnostic.as_str()
                    ),
                )
                .with_remedy(remedy)
            }
            _ => TranscriptRow::info(
                NoticeScope::Session,
                "not ready; choose a profile for automatic attachment, or retry attachment",
            )
            .with_remedy(NoticeRemedy::ChooseProfileOrAttach {}),
        };
    let _ = events.notice(row);
    let _ = events.send(ContentEvent::Status {
        phase: HostPhase::NotReady,
        queue_len: 0,
        detail: Some(HostStatusDetail::DetachedPromptRefused {
            reason: config.reason.clone(),
        }),
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

type ReadinessResult = Result<
    (
        Vec<plakous_host_protocol::views::ProviderDef>,
        Vec<plakous_host_protocol::views::ProviderSection>,
    ),
    String,
>;

/// Credential readiness re-reads (spec 02 §5.5). The product's read may wait on
/// subscription locks, so it runs on its own thread; the action loop applies
/// only the newest settled read, between actions.
pub(super) struct ReadinessRefresh {
    generation: u64,
    sender: std::sync::mpsc::Sender<(u64, ReadinessResult)>,
    settled: std::sync::mpsc::Receiver<(u64, ReadinessResult)>,
}

impl ReadinessRefresh {
    pub(super) fn new() -> Self {
        let (sender, settled) = std::sync::mpsc::channel();
        Self {
            generation: 0,
            sender,
            settled,
        }
    }

    /// Starts one re-read without waiting for it.
    pub(super) fn start<P: plakous_host_protocol::ProtocolExtension>(
        &mut self,
        host: &impl ProductAuthHost<P>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let sender = self.sender.clone();
        let Some(read) = host.surface().map(SurfaceHandler::readiness_read) else {
            let _ = sender.send((generation, Err("no product surface is attached".to_owned())));
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("plak-readiness-refresh".to_owned())
            .spawn({
                let sender = sender.clone();
                move || {
                    let _ = sender.send((generation, read()));
                }
            });
        if spawned.is_err() {
            let _ = sender.send((
                generation,
                Err("readiness refresh could not start".to_owned()),
            ));
        }
    }

    /// Republishes both projections from the newest settled re-read; an older
    /// read that settles later is discarded.
    pub(super) fn apply_settled<P: plakous_host_protocol::ProtocolExtension>(
        &self,
        host: &mut impl ProductAuthHost<P>,
        events: &Sender<P>,
    ) {
        let Some(refreshed) = self
            .settled
            .try_iter()
            .filter(|(generation, _)| *generation == self.generation)
            .last()
            .map(|(_, refreshed)| refreshed)
        else {
            return;
        };
        match refreshed {
            Ok((providers, model_sections)) => {
                host.replace_ready_catalog(plakous_host_protocol::views::ReadyCatalog::new(
                    model_sections,
                ));
                let _ = events.send(ContentEvent::ProviderSnapshot { providers });
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
        (
            _,
            CompositionSelection::JsPtc | CompositionSelection::PyPtc | CompositionSelection::PySdk,
        ) => composition,
        (_, CompositionSelection::NativePtc) => {
            return Err(crate::binding::OPENROUTER_NATIVE_PTC_REFUSAL);
        }
    };
    Ok((effort, composition))
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Shell model selection resolves the exact catalog entry before rebinding and reporting its result to the prompt loop."
)]
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
    // A home that already loads a default root keeps it: Use now binds this
    // session alone, and only a home without one gets the profile's root.
    let keeps_default = binding.current().is_some();
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
        if keeps_default {
            Ok(())
        } else {
            binding.commit(root)
        }
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
        format!(
            "model → `{model}`{effort_note} ({})",
            if keeps_default {
                "session"
            } else {
                "session + config"
            }
        ),
    ));
    maybe_note_ready_hint(config, events);
    ShellBindingOutcome::Committed
}

/// Writes a pre-attach shell binding into the `[root] agent` template. A
/// partial tuple is refused by the binding document without changing the
/// configured root template.
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
            let _ = events.send(ContentEvent::Status {
                phase: HostPhase::NotReady,
                queue_len: 0,
                detail: Some(HostStatusDetail::ReattachPending),
            });
        }
        _ => {
            let _ = events.send(ContentEvent::Status {
                phase: HostPhase::NotReady,
                queue_len: 0,
                detail: Some(HostStatusDetail::ChooseProfile),
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
mod tests;
