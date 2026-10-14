//! Non-interactive waiting owns no turn; continuations use ordinary admission.

use super::{CliFailure, CliResult, HostLoop, HostStep, InterruptWatch};
use crate::runtime::oneshot::{OneShotOutcome, OneShotState};
use crate::sessions::{NoticeScope, TranscriptRow};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn next_run_prompt(&mut self, state: &mut OneShotState) -> CliResult<HostStep> {
        let mut waiting_noted = false;
        loop {
            let Some(run) = state.run.as_ref() else {
                return Ok(HostStep::Quit);
            };
            let timeout = run.timed_out.load(Ordering::Relaxed)
                || run
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline);
            let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
            watch.poll();
            let interrupted = watch.interrupted();
            let tree_id =
                super::durable_tree_id(&self.config.topology_rows(), &self.config.session);
            let runtime = crate::topology_runtime::lookup(&tree_id);
            let (waiting_rows, mail) = if let Some(runtime) = runtime {
                runtime
                    .read(|tree| {
                        let children = waiting_children(tree);
                        let mail = tree
                            .pending_mail(&tree.root_agent_id)
                            .into_iter()
                            .filter(|mail| {
                                matches!(
                                    mail.purpose,
                                    crate::agent_tree_protocol::MessagePurpose::Result
                                        | crate::agent_tree_protocol::MessagePurpose::Question
                                )
                            })
                            .map(|mail| {
                                (
                                    mail.message_id,
                                    format!(
                                        "{} {}",
                                        mail.from,
                                        super::super::result_mail::purpose_name(mail.purpose)
                                    ),
                                )
                            })
                            .collect::<Vec<_>>();
                        (children, mail)
                    })
                    .map_err(|error| CliFailure::new(error.message))?
            } else {
                (Vec::new(), Vec::new())
            };
            let children = waiting_rows
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let jobs = self
                .opened
                .agent()
                .map_or_else(Vec::new, plakous::agent::Agent::jobs);
            let running = jobs
                .iter()
                .filter(|job| job.status == plakous::tools::JobStatus::Running)
                .collect::<Vec<_>>();
            let notices = jobs
                .iter()
                .filter(|job| job.status != plakous::tools::JobStatus::Running)
                .collect::<Vec<_>>();
            let pending = !notices.is_empty() || !mail.is_empty();
            let live = !running.is_empty() || !children.is_empty();
            let completed = matches!(state.result, Some(Ok(OneShotOutcome::Completed { .. })));
            let resuming = self.pending_resume.is_some();
            // A deadline interrupt can race an already complete final turn.
            let interrupted = interrupted && (!timeout || !completed || pending || live);
            let bound = if timeout && (pending || live || !completed) {
                Some("timeout".to_owned())
            } else if (resuming || (completed && (pending || live)))
                && run.turns >= run.bounds.max_turns
            {
                Some(format!("max-turns {}", run.bounds.max_turns))
            } else {
                None
            };
            if bound.is_some() || interrupted || (!completed && !resuming) {
                self.cancel_transient_resume();
                let stopped = running
                    .iter()
                    .map(|job| format!("stopped p{}", job.number))
                    .collect::<Vec<_>>();
                let mut failures = Vec::new();
                match self.executor.block_on(
                    self.config
                        .state_roots
                        .job_registry()
                        .stop_all(Duration::from_millis(200)),
                ) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => failures.push(error),
                    Err(_) => failures.push("job stop execution failed".to_owned()),
                }
                let root = self.opened.agent().cloned();
                let runtime = crate::topology_runtime::lookup(&tree_id);
                // Transfer retirement closes child control ingress. Record
                // quiescent terminals while their drivers still own authority;
                // working children carry the evidence in cancellation commits.
                match self.executor.block_on(async {
                    let mut owners = Vec::new();
                    if let Some(runtime) = runtime {
                        let live = runtime.live.lock().await;
                        owners.extend(
                            live.keys()
                                .filter_map(|id| Some((live.get(id)?.agent()?, true))),
                        );
                    }
                    owners.extend(root.map(|agent| (agent, false)));
                    let mut errors = Vec::new();
                    for (agent, child) in owners {
                        if let Err(error) =
                            terminal_recording_result(child, agent.record_job_terminals().await)
                        {
                            errors.push(error.to_string());
                        }
                    }
                    errors
                }) {
                    Ok(errors) => failures.extend(errors),
                    Err(_) => failures.push("job terminal recording execution failed".to_owned()),
                }
                match self
                    .executor
                    .block_on(super::super::topology_state::mark_root_interrupted(
                        &tree_id,
                    )) {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => failures.push(error),
                    Err(error) => failures.push(error.to_string()),
                }
                if let Some(bound) = bound {
                    if let Some(session_id) =
                        crate::product_observation::session_id_from_text(&self.config.session.id)
                    {
                        self.config.state_roots.emit_product(
                            crate::product_observation::ProductObservation::RunBoundExit {
                                at: crate::product_observation::wall_timestamp(),
                                session_id,
                                bound: if timeout {
                                    crate::product_observation::RunBound::Timeout
                                } else {
                                    crate::product_observation::RunBound::MaxTurns
                                },
                            },
                        );
                    }

                    let assistant = match state.result.take() {
                        Some(Ok(
                            OneShotOutcome::Completed { assistant }
                            | OneShotOutcome::Interrupted { assistant }
                            | OneShotOutcome::Refused { assistant, .. },
                        )) => assistant,
                        result => {
                            state.result = result;
                            String::new()
                        }
                    };
                    let mut details = stopped;
                    details.extend(children.iter().map(|id| format!("interrupted {id}")));
                    let suffix = if details.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", details.join("; "))
                    };
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Tree,
                        format!("plak: run stopped at {bound}{suffix}"),
                    ));
                    state.result = Some(Ok(OneShotOutcome::Bounded { assistant, bound }));
                } else if interrupted && completed {
                    if let Some(Ok(OneShotOutcome::Completed { assistant })) = state.result.take() {
                        state.result = Some(Ok(OneShotOutcome::Interrupted { assistant }));
                    }
                }
                if let Err(error) = self.pause_run_queue() {
                    failures.push(error.message().to_owned());
                }
                return if failures.is_empty() {
                    Ok(HostStep::Quit)
                } else {
                    Err(CliFailure::new(failures.join("; ")))
                };
            }
            if resuming {
                match self.transient_resume_prompt()? {
                    HostStep::Continue => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    step => return Ok(step),
                }
            }
            if pending {
                let (command, batch) = super::super::result_mail::select(
                    &self.config.topology_rows(),
                    &self.config.session,
                    super::wake::WAKE_PROMPT,
                )
                .map_err(CliFailure::new)?;
                if !self.fit_child_wake(&command)? {
                    if let Some(Ok(OneShotOutcome::Completed { assistant })) = state.result.take() {
                        state.result = Some(Ok(OneShotOutcome::Interrupted { assistant }));
                    }
                    continue;
                }
                let trigger_ids = notices
                    .iter()
                    .map(|job| format!("p{}", job.number))
                    .chain(mail.iter().map(|(id, _)| id.clone()))
                    .collect::<Vec<_>>();
                let item = self
                    .config
                    .session
                    .enqueue_live(super::wake::WAKE_PROMPT.into())
                    .map_err(|_| CliFailure::new("run continuation queue admission was refused"))?;
                self.config.session = super::super::result_mail::prepare_instruction(
                    &self.config.topology_rows(),
                    self.config.store.as_ref(),
                    &self.config.session,
                    &item,
                    &command,
                    batch,
                    &trigger_ids,
                )
                .map_err(CliFailure::new)?;
                let triggers = notices
                    .iter()
                    .map(|job| format!("p{} {}", job.number, job_status(job.status)))
                    .chain(mail.iter().map(|(_, label)| label.clone()))
                    .collect::<Vec<_>>();
                let _ = self.events.notice(TranscriptRow::info(
                    NoticeScope::Tree,
                    format!(
                        "plak: continuing (turn {}: {})",
                        run.turns + 1,
                        triggers.join(", ")
                    ),
                ));
                return Ok(HostStep::Prompt {
                    text: item.text.clone(),
                    queued_delivery: Some(item),
                    user_echoed: true,
                });
            }
            if !live {
                self.pause_run_queue()?;
                return Ok(HostStep::Quit);
            }
            if !waiting_noted {
                let waiting = running
                    .iter()
                    .map(|job| format!("p{} ({})", job.number, job.command))
                    .chain(waiting_rows.iter().map(|(_, label)| label.clone()))
                    .collect::<Vec<_>>();
                let _ = self.events.notice(TranscriptRow::info(
                    NoticeScope::Tree,
                    format!("plak: waiting for {}; Ctrl-C to stop", waiting.join(", ")),
                ));
                waiting_noted = true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pause_run_queue(&mut self) -> CliResult<()> {
        self.config.session.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
        self.config
            .store
            .save(&mut self.config.session)
            .map_err(|error| {
                super::super::admission::log_queue_persist_failed(
                    &self.config.home,
                    &self.config.session,
                    super::super::admission::QueuePersistAction::PauseDrain,
                    &error,
                );
                CliFailure::new(format!("queue pause could not be committed: {error}"))
            })
    }
}

/// The children a run still waits for, sorted, each with its waiting-line
/// label. A live owner still fences work after its terminal projects idle.
/// A parked child still runs: the host wakes it on its own work (spec
/// 03 §5.2), so the run keeps waiting and names what the child waits on.
fn waiting_children(tree: &crate::agent_tree::ProductAgentTree) -> Vec<(String, String)> {
    let mut children = tree
        .nodes()
        .values()
        .filter(|node| {
            node.agent_id != tree.root_agent_id
                && (tree.has_turn_owner(&node.agent_id) || tree.is_parked(&node.agent_id))
        })
        .map(|node| {
            let id = node.agent_id.clone();
            let label = match tree.waiting_on(&id) {
                Some(waiting) if waiting.processes.is_empty() && waiting.agents.is_empty() => {
                    format!("{id} (parked)")
                }
                Some(waiting) => {
                    let on = waiting
                        .processes
                        .iter()
                        .chain(&waiting.agents)
                        .cloned()
                        .collect::<Vec<_>>();
                    format!("{id} (parked on {})", on.join(", "))
                }
                None => id.clone(),
            };
            (id, label)
        })
        .collect::<Vec<_>>();
    children.sort();
    children
}

fn job_status(status: plakous::tools::JobStatus) -> String {
    match status {
        plakous::tools::JobStatus::Exited(Some(code)) => format!("exited {code}"),
        plakous::tools::JobStatus::Exited(None) => "exited".into(),
        plakous::tools::JobStatus::Stopped(_) => "stopped".into(),
        plakous::tools::JobStatus::OutcomeUnknown => "outcome_unknown".into(),
        plakous::tools::JobStatus::Running => "running".into(),
    }
}

fn terminal_recording_result(
    child: bool,
    result: Result<bool, plakous::agent::AgentError>,
) -> Result<(), plakous::agent::AgentError> {
    match result {
        Err(error)
            if child
                && matches!(
                    error.kind(),
                    plakous::agent::AgentErrorKind::Shutdown
                        | plakous::agent::AgentErrorKind::Backpressure
                ) =>
        {
            // Retired children have no ingress; busy drivers record terminals
            // in their cancellation/release commits instead.
            Ok(())
        }
        result => result.map(|_| ()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::{terminal_recording_result, waiting_children};
    use plakous::agent::{AgentError, AgentErrorKind};

    #[test]
    fn stopped_or_busy_child_ingress_is_not_a_run_publication_failure() {
        for kind in [AgentErrorKind::Shutdown, AgentErrorKind::Backpressure] {
            assert!(
                terminal_recording_result(true, Err(AgentError::new(kind, "child ingress")))
                    .is_ok()
            );
            assert!(
                terminal_recording_result(false, Err(AgentError::new(kind, "root ingress")))
                    .is_err()
            );
        }
    }

    #[test]
    fn real_terminal_publication_failures_remain_fatal() {
        for child in [true, false] {
            assert!(
                terminal_recording_result(
                    child,
                    Err(AgentError::new(
                        AgentErrorKind::Session,
                        "publication failed"
                    ))
                )
                .is_err()
            );
            assert!(terminal_recording_result(child, Ok(false)).is_ok());
            assert!(terminal_recording_result(child, Ok(true)).is_ok());
        }
    }

    #[test]
    fn a_live_owner_keeps_the_run_waiting_after_the_turn_view_is_idle() {
        let root = tempfile::tempdir().unwrap();
        let mut tree =
            crate::topology_runtime::test_support::bootstrap("run-wait", root.path().to_owned());
        let child = tree
            .spawn_agent(
                "root",
                &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap()
            .agent_id;
        tree.bind_turn_owner(&child, "00000000-0000-0000-0000-000000000001".to_owned());
        assert!(!tree.node(&child).unwrap().active_turn);
        assert!(tree.attempt(&child).unwrap().turn_id.is_none());
        assert_eq!(waiting_children(&tree), [(child.clone(), child.clone())]);
        tree.release_turn_owner(&child);
        assert!(waiting_children(&tree).is_empty());
    }

    /// `plak run` keeps waiting for a parked child and names what it waits on.
    #[test]
    fn a_parked_child_is_still_running_and_named_on_the_waiting_line() {
        let root = tempfile::tempdir().unwrap();
        let mut tree =
            crate::topology_runtime::test_support::bootstrap("run-wait", root.path().to_owned());
        let spawn = crate::agent_tree_protocol::SpawnAgentRequest::test_review();
        let mut child = || {
            let id = tree.spawn_agent("root", &spawn, None).unwrap().agent_id;
            tree.bind_session_locator(&id, format!("locator-{id}"))
                .unwrap();
            tree.set_active_turn(&id, true).unwrap();
            tree.bind_attempt_turn(&id, "00000000-0000-0000-0000-000000000001")
                .unwrap();
            id
        };
        let (busy, parked, idle) = (child(), child(), child());
        tree.park_child(&parked, vec!["p3".to_owned()]).unwrap();
        tree.park_child(&idle, Vec::new()).unwrap();
        assert_eq!(
            waiting_children(&tree),
            [
                (busy.clone(), busy.clone()),
                (parked.clone(), format!("{parked} (parked on p3)")),
                (idle.clone(), format!("{idle} (parked)")),
            ]
        );
        // A wake in flight is plain running work again.
        tree.begin_wake(&parked);
        assert_eq!(waiting_children(&tree)[1], (parked.clone(), parked));
    }
}
