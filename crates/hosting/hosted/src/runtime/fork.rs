//! `/fork` workflow.

use plakous::application::ApplicationExecutor;
use plakous::sessions::{AgentSessionsErrorKind, ForkSessionOutcome, SessionOperationId};

use crate::agent::AttachedCodingSession;
use crate::interrupt::{InterruptWatch, run_watched};
use crate::sessions::{self};
use crate::{CliFailure, CliResult};

use super::admission::emit_queue_snapshot;
use super::info::{send_binding, send_last_user_prompt_at};
use super::rebind::{
    durable_tree_id, instruction_sources, interrupted_before_dispatch, note_reread_instructions,
    note_reread_instructions_at, restore_detached_source,
};
#[cfg(any(test, feature = "test-support"))]
use super::startup::injected_runtime;
#[cfg(any(test, feature = "test-support"))]
use super::startup::observe_test_boundary;
use super::startup::{api_runtime_for_record, host_runtime_binding, open_session_for_host};
use super::{HostConfig, Sender, TestRuntimeRole};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow};
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) fn handle_fork<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    requested_account: Option<&str>,
) -> CliResult<()> {
    super::jobs_transition_guard(config, opened)?;
    watch.begin();
    if requested_account.is_some_and(|account| config.session.account.as_deref() != Some(account)) {
        return Err(CliFailure::new(
            "cross-binding fork requires the confirmed rebind workflow; source unchanged",
        ));
    }
    if watch.observe_now() {
        return Err(interrupted_before_dispatch("fork"));
    }
    if config.session.store_revision == 0 {
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)?;
    }
    project_fork_source(executor, watch, config, opened, false)?;
    let intents = config
        .store
        .load_intents(&config.session.id)
        .map_err(CliFailure::new)?;
    let views = config
        .store
        .load_views(&config.session.id)
        .map_err(CliFailure::new)?
        .ok_or_else(|| CliFailure::new("fork requires projected views"))?;
    if !config.session.pending_queue.is_empty()
        || !sessions::projector::pending_command_ids(&views, &intents).is_empty()
        || intents.workflow_requests.iter().any(|request| {
            views
                .state
                .workflows
                .get(&request.operation_id)
                .is_none_or(|workflow| workflow.settled.is_none())
        })
    {
        return Err(CliFailure::new(
            "fork requires an idle session with no queued or unresolved product work",
        ));
    }
    let attached_locator = opened.locator();
    if config.session.plakous_locator != Some(attached_locator) {
        return Err(CliFailure::new(
            "fork source does not match the exact durable product binding",
        ));
    }
    // nothing durable has happened yet, so an intent observed
    // here refuses outright and the exact source stays attached.
    if watch.observe_now() {
        return Err(interrupted_before_dispatch("fork"));
    }
    let operation_id = format!("plak:fork:{}", uuid::Uuid::new_v4());
    let facade_operation = SessionOperationId::new(&operation_id)
        .map_err(|_| CliFailure::new("fork operation identity is invalid"))?;
    let instructions_before = instruction_sources(opened);
    let source_locator = run_watched(executor, watch, opened.detach())
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|error| CliFailure::new(format!("fork source release failed: {error}")))?;
    if watch.observe_now() {
        restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
        return Err(interrupted_before_dispatch("fork"));
    }
    let request = sessions::projector::WorkflowRequest {
        operation_id: operation_id.clone(),
        source_locator,
        kind: sessions::SessionWorkflowKind::Fork,
    };
    if let Err(error) = config
        .store
        .submit_workflow_request(&mut config.session, &request)
    {
        restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
        return Err(CliFailure::new(error));
    }
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Fork,
        TestHostBoundary::Prepared,
    );
    if watch.observe_now() {
        restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
        discard_unstarted_fork(executor, watch, config, opened, &operation_id)?;
        return Err(interrupted_before_dispatch("fork"));
    }
    stderr_buf.clear();
    let runtime = match fork_runtime_binding(executor, config, stderr_buf) {
        Ok(runtime) => runtime,
        Err(error) => {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            discard_unstarted_fork(executor, watch, config, opened, &operation_id)?;
            return Err(error);
        }
    };
    if watch.observe_now() {
        restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
        discard_unstarted_fork(executor, watch, config, opened, &operation_id)?;
        return Err(interrupted_before_dispatch("fork"));
    }
    let outcome = match run_watched(executor, watch, opened.fork(facade_operation, runtime)) {
        Ok(Ok(outcome)) => outcome,
        // Refused before anything canonical happened: nothing was forked.
        Ok(Err(error))
            if matches!(
                error.kind(),
                AgentSessionsErrorKind::InvalidInput
                    | AgentSessionsErrorKind::IdentityConflict
                    | AgentSessionsErrorKind::Revalidation
                    | AgentSessionsErrorKind::StoragePressure
            ) =>
        {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            discard_unstarted_fork(executor, watch, config, opened, &operation_id)?;
            if let Some(refusal) = fork_refusal(error.kind(), &error.to_string()) {
                return Err(CliFailure::new(refusal));
            }
            let raw = format!("canonical fork failed: {error}");
            return Err(CliFailure::new(
                crate::storage_pressure::sessions_storage_pressure(&error, &raw).unwrap_or(raw),
            ));
        }
        // Possibly dispatched: the workflow stays unresolved for exact
        // reconciliation and is never replayed.
        Ok(Err(error)) => {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            return Err(CliFailure::new(format!("canonical fork failed: {error}")));
        }
        Err(_) => {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            return Err(CliFailure::new("application execution failed"));
        }
    };
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Fork,
        TestHostBoundary::RuntimeReturned,
    );
    let target_locator = match outcome {
        ForkSessionOutcome::Forked {
            source: exact_source,
            target,
            ..
        } if exact_source == source_locator => target,
        _ => {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            discard_unstarted_fork(executor, watch, config, opened, &operation_id)?;
            return Err(CliFailure::new(request.withdrawal_note()));
        }
    };
    // The branch fact and request, not this runtime receipt, publish the
    // successor and settle the source atomically under the projector cursor.
    // Finish that publication before honouring an interrupt on successor catch-up.
    if let Err(error) = project_fork_source(executor, watch, config, opened, true) {
        restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
        return Err(error);
    }
    let target_id = request.successor_product_id();
    let target_result = (|| {
        run_watched(
            executor,
            watch,
            super::projection::project_to_tail_interruptible(
                opened.sessions(),
                &config.store,
                &target_id,
                target_locator,
                || watch.observe_now(),
            ),
        )
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(CliFailure::new)?;
        config.store.load(&target_id).map_err(CliFailure::new)
    })();
    let target = match target_result {
        Ok(target) => target,
        Err(error) => {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            return Err(CliFailure::new(format!(
                "the fork was saved as session {target_id} but could not project ({}); this session stays attached, and /resume {target_id} opens the fork",
                error.message(),
            )));
        }
    };
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Fork,
        TestHostBoundary::ProductPublished,
    );
    stderr_buf.clear();
    let replacement = match run_watched(
        executor,
        watch,
        open_session_for_host(
            config,
            &target,
            target_locator,
            stderr_buf,
            TestRuntimeRole::Primary,
        ),
    ) {
        Ok(Ok(replacement)) => replacement,
        result => {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            let detail = match result {
                Ok(Err(error)) => error.failure.message().to_owned(),
                _ => "application execution failed".to_owned(),
            };
            note_reread_instructions_at(
                executor,
                config,
                events,
                instructions_before,
                target_locator,
            );
            return Err(CliFailure::new(format!(
                "the fork was saved as session {target_id} but could not attach ({detail}); this session stays attached, and /resume {target_id} opens the fork"
            )));
        }
    };
    let previous = std::mem::replace(opened, replacement);
    config.session = target;
    let release_error = match run_watched(executor, watch, previous.release()) {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(error) => Some(error.to_string()),
    };
    if let Some(error) = release_error {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Session,
            format!("fork target is active; source release could not be joined: {error}"),
        ));
    }
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Fork,
        TestHostBoundary::ProductCommitted,
    );
    emit_queue_snapshot(events, &config.session);
    let _ = events.send(super::logging::project_session_snapshot(&config.session));
    send_binding(events, config);
    send_last_user_prompt_at(events, opened);
    note_reread_instructions(events, instructions_before, opened);
    if watch.observe_now() {
        // The canonical fork was already dispatched when the intent arrived.
        // Saying so is the only truthful option: no terminal is undone here.
        let _ = events.notice(TranscriptRow::info(
            NoticeScope::Session,
            "interrupt arrived after the canonical fork was dispatched; the fork completed",
        ));
    }
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Session,
        format!(
            "forked session {}…; source remains resumable",
            &config.session.id[..8]
        ),
    ));
    Ok(())
}

/// The `/fork` wording for a refusal the operator can act on. A fork
/// recompiles the session's own configuration; when its tools changed
/// (Plakous: "fork changed canonical tool authority") only a new session can
/// use them.
pub(super) fn fork_refusal(kind: AgentSessionsErrorKind, message: &str) -> Option<&'static str> {
    (kind == AgentSessionsErrorKind::Revalidation
        && message.contains("fork changed canonical tool authority"))
    .then_some("cannot fork: this session's tools changed; start a new session")
}

/// The runtime a fork recompiles from: this session's own binding and tree,
/// built from the files as they are now. It takes no topology lease.
pub(super) fn fork_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(runtime) = injected_runtime(config, TestRuntimeRole::Primary) {
        return runtime;
    }
    if config.session.target_selection.api_target().is_none() {
        return host_runtime_binding(config, stderr_buf, TestRuntimeRole::Primary);
    }
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let mut attached = None;
    executor
        .block_on(api_runtime_for_record(
            config,
            &config.session,
            false,
            crate::topology_runtime::TopologyHubAttach::LookupOnly,
            Some(&tree_id),
            &mut attached,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?
}

fn project_fork_source<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    opened: &AttachedCodingSession,
    finish_publication: bool,
) -> CliResult<()> {
    run_watched(
        executor,
        watch,
        super::projection::project_to_tail_interruptible(
            opened.sessions(),
            &config.store,
            &config.session.id,
            opened.locator(),
            || !finish_publication && watch.observe_now(),
        ),
    )
    .map_err(|_| CliFailure::new("application execution failed"))?
    .map(|_| ())
    .map_err(CliFailure::new)
}

fn discard_unstarted_fork<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    operation: &str,
) -> CliResult<()> {
    project_fork_source(executor, watch, config, opened, true)?;
    config
        .store
        .discard_unadmitted_workflow(&mut config.session, operation)
        .map_err(CliFailure::new)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;

    use super::*;

    use crate::interrupt::{InterruptWatch, InterruptWatchState};
    use crate::runtime::test_support::*;
    use plakous::application::ApplicationExecutor;
    use std::cell::RefCell;

    #[test]
    fn live_fork_refuses_without_withdrawing_another_pending_request() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let mut opened = executor
            .block_on(super::super::startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        config.store.save(&mut config.session).unwrap();
        let request = sessions::projector::WorkflowRequest {
            operation_id: "plak:compact:pending-at-fork".into(),
            source_locator: opened.locator(),
            kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
        };
        config
            .store
            .submit_workflow_request(&mut config.session, &request)
            .unwrap();
        let (events, rx) = host_event_sink();
        let (_actions, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let error = handle_fork(
            &executor,
            InterruptWatch::new(&actions_rx, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        )
        .unwrap_err();
        assert!(error.message().contains("unresolved product work"));
        assert!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_request(&request.operation_id)
                .is_some()
        );
        assert!(
            rx.try_iter()
                .all(|event| !matches!(event, HostEvent::Row(TranscriptRow::Notice(_))))
        );
    }

    #[test]
    fn attached_no_effort_openrouter_fork_uses_real_host_workflow() {
        let fork_root = tempfile::tempdir().unwrap();
        let fork_events = run_attached_actions(
            no_effort_openrouter_injected_host(fork_root.path()),
            vec![UserAction::ForkSession {
                account: None,
                compact: false,
            }],
        );
        assert!(fork_events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("forked session")
        )));
        assert!(fork_events.iter().all(|event| !matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("effort")
        )));
    }

    #[test]
    fn fork_then_compact_on_a_short_session_keeps_the_fork() {
        let root = tempfile::tempdir().unwrap();
        let events = run_attached_actions(
            no_effort_openrouter_injected_host(root.path()),
            vec![UserAction::ForkSession {
                account: None,
                compact: true,
            }],
        );
        assert!(events.iter().any(|event| matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("forked session")
        )));
        assert!(events.iter().any(|event| {
            let text = match event {
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
                )) => note.as_str(),
                _ => return false,
            };
            text.contains("no eligible") || text.contains("the forked session is active")
        }));
        assert!(events.iter().all(|event| !matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.starts_with("fork:")
        )));
    }

    /// The canonical fork landed but its session could not attach: the fork
    /// is published as its own resumable session, the workflow resolves, and
    /// this session stays attached and promptable.
    #[test]
    fn a_fork_that_cannot_attach_is_saved_and_the_source_keeps_working() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let (mut config, injection) = fresh_injected_attached_host(root.path());
        let primaries = std::sync::Arc::new(AtomicUsize::new(0));
        let runtime = std::sync::Arc::clone(&injection.runtime);
        let counted = std::sync::Arc::clone(&primaries);
        config.test_injection = Some(std::sync::Arc::new(crate::runtime::TestHostInjection {
            // Primary runtimes: the source, the fork's compilation, then the
            // fork's own session.
            runtime: std::sync::Arc::new(move |role| {
                if role == TestRuntimeRole::Primary && counted.fetch_add(1, Ordering::SeqCst) == 2 {
                    return Err("injected: the fork target cannot attach".to_owned());
                }
                runtime(role)
            }),
            rebind_target: injection.rebind_target.clone(),
            observe: std::sync::Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        }));
        let store = std::sync::Arc::clone(&config.store);
        let source_id = config.session.id.clone();
        let events = run_attached_actions(
            config,
            vec![
                UserAction::ForkSession {
                    account: None,
                    compact: false,
                },
                UserAction::Prompt("still here".into()),
            ],
        );
        let note = events
            .iter()
            .find_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
                )) if note.contains("could not attach") => Some(note.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no attach failure reported: {events:#?}"));
        assert!(note.contains("this session stays attached"), "{note}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HostEvent::PromptStarted(Some(_)))),
            "the source took the next prompt: {events:#?}"
        );

        let intents = store.load_intents(&source_id).unwrap();
        let request = intents.workflow_requests.first().unwrap();
        assert_eq!(request.kind, sessions::SessionWorkflowKind::Fork);
        let views = store.load_views(&source_id).unwrap().unwrap();
        assert!(
            views.state.workflows[&request.operation_id]
                .settled
                .is_some()
        );
        let target_id = note
            .split_whitespace()
            .skip_while(|word| *word != "session")
            .nth(1)
            .unwrap();
        let target = store.load(target_id).unwrap();
        assert_eq!(
            target.source_product_session_id.as_deref(),
            Some(source_id.as_str())
        );
    }

    /// A fork refused because the session's tools changed says so in `/fork`
    /// terms; other refusals keep the canonical text.
    #[test]
    fn a_fork_refused_for_changed_tools_says_so() {
        assert_eq!(
            fork_refusal(
                AgentSessionsErrorKind::Revalidation,
                "fork changed canonical tool authority outside its authority",
            ),
            Some("cannot fork: this session's tools changed; start a new session")
        );
        assert_eq!(
            fork_refusal(
                AgentSessionsErrorKind::Revalidation,
                "fork changed effect policy outside its authority",
            ),
            None
        );
    }

    #[test]
    fn interrupt_refuses_fork_before_any_durable_work() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let mut opened = memory_attached_stub();
        let source = opened.locator();
        config.session.bind_plakous_locator(source).unwrap();
        let (events, _rx) = host_event_sink();
        let (actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        actions_tx.send(UserAction::Interrupt).unwrap();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);

        let error = handle_fork(
            &ApplicationExecutor::new().unwrap(),
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        )
        .expect_err("an interrupted fork must not commit");

        assert!(
            error.message().contains("interrupted"),
            "fork refusal must name the interrupt: {}",
            error.message()
        );
        assert_eq!(config.session.plakous_locator, Some(source));
    }
}
