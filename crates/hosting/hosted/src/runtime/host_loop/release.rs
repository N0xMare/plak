//! Release canonical ownership; the feed projector owns the head view.

use super::{CliFailure, CliResult, HostLoop, delete_unpublished_runtime};

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn release(mut self) -> CliResult<()> {
        self.pending_resume = None;
        let abandon = self.quit_abandoned_turn;
        self.opened.close_admission();
        let published = self.config.session.store_revision != 0;
        if !self.projection_finished {
            self.finish_projection(false)?;
        }
        if abandon && let Some(agent) = self.opened.agent() {
            agent.begin_abandon();
        }
        let registry = self.config.state_roots.job_registry().clone();
        let deadline = self.quit_started_at.unwrap_or_else(super::Instant::now)
            + std::time::Duration::from_secs(1);
        let job_budget = || {
            deadline
                .saturating_duration_since(super::Instant::now())
                .saturating_sub(std::time::Duration::from_millis(150))
                .min(std::time::Duration::from_millis(400))
        };
        let stop_failure = if abandon {
            let pending = self.quit_job_stops.take();
            let tree_id =
                super::durable_tree_id(&self.config.topology_rows(), &self.config.session);
            let root = self.opened.agent().cloned();
            self.executor
                .block_on(async {
                    let mut agents = crate::topology_runtime::abandon_live_agents(&tree_id).await;
                    agents.extend(root);
                    for agent in &agents {
                        agent.begin_abandon();
                    }
                    let owners = agents
                        .iter()
                        .map(|agent| agent.id().to_string())
                        .collect::<Vec<_>>();
                    let stops = async {
                        if let Some(pending) = pending {
                            pending
                                .await
                                .map_err(|_| "background process stop task failed".to_owned())?
                        } else {
                            registry
                                .stop_all_except(std::time::Duration::from_millis(200), &owners)
                                .await
                        }
                    };
                    tokio::time::timeout(job_budget(), stops)
                        .await
                        .map_err(|_| "background process stop deadline exceeded".to_owned())?
                })
                .map_err(|_| "application execution failed".to_owned())
                .and_then(std::convert::identity)
                .err()
        } else {
            self.executor
                .block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        registry.stop_all(std::time::Duration::from_millis(200)),
                    )
                    .await
                    .map_err(|_| "background process stop deadline exceeded".to_owned())?
                })
                .map_err(|_| "application execution failed".to_owned())
                .and_then(std::convert::identity)
                .err()
        };
        self.update_background_process_count();
        let runtime_session = self.opened.locator().session_id();
        let released = if abandon {
            self.executor
                .block_on(self.opened.abandon_and_release(job_budget()))
                .map_err(|_| CliFailure::new("application execution failed"))
        } else {
            self.executor.block_on(async {
                tokio::time::timeout(super::super::HOST_RELEASE_DEADLINE, self.opened.release()).await
            }).map_err(|_| CliFailure::new("application execution failed"))
                .and_then(|release| release.map_err(|_| CliFailure::new(
                    "session release exceeded its shutdown deadline; cleanup is incomplete and recovery may be required",
                )))
        };
        let (locator, release_failure) = match released {
            Ok(Ok(locator)) => (Some(locator), None),
            Ok(Err(error)) => (
                error.released_locator(),
                Some(CliFailure::new(error.to_string())),
            ),
            Err(error) => (None, Some(error)),
        };
        let release_failure = match (stop_failure, release_failure) {
            (Some(stop), Some(release)) => Some(release.with_cleanup_failure(stop)),
            (Some(stop), None) => Some(CliFailure::new(stop)),
            (None, release) => release,
        };
        let Some(locator) = locator else {
            return CliFailure::shutdown(release_failure, None);
        };
        let cleanup_failure = if published {
            let deadline = self
                .quit_started_at
                .map(|started| started + std::time::Duration::from_secs(1))
                .unwrap_or_else(|| super::Instant::now() + super::FINAL_PROJECTION_BOUND);
            let projected = self.executor.block_on(async {
                let sessions = self.config.state_roots.runtime_sessions()?;
                let pass = super::super::projection::project_to_tail(
                    &sessions,
                    &self.config.store,
                    &self.config.session.id,
                    locator,
                );
                #[cfg(test)]
                let pass = super::super::projection::probe::CALLER
                    .scope(super::super::projection::probe::Caller::Release, pass);
                // A spent budget starts no pass: a timeout only drops its
                // future, so a page commit already on the blocking pool lands.
                let projected = if super::Instant::now() < deadline {
                    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), pass)
                        .await
                        .ok()
                } else {
                    None
                };
                if let Some(projected) = projected {
                    projected.map(|_| ()).map_err(CliFailure::new)
                } else {
                    super::log_ev(
                        self.log.as_ref(),
                        &self.config,
                        super::SessionEvent::new("projector.deferred").str(
                            "error",
                            "the post-release projector pass exceeded its bound",
                        ),
                    );
                    Ok(())
                }
            });
            match projected {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(_) => Some(CliFailure::new(
                    "the post-release projector pass could not run",
                )),
            }
        } else {
            delete_unpublished_runtime(&self.executor, &self.config.state_roots, locator).err()
        };
        for terminal in super::take_pending_usage(&self.pending_usage, runtime_session) {
            super::super::usage::log_projected_turn_usage(
                &self.config.session.id,
                terminal.turn_id,
                terminal.usage,
                self.log.as_ref(),
            );
        }
        CliFailure::shutdown(release_failure, cleanup_failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Sender;
    use crate::runtime::startup::build_session_from_config;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn quit_projection_stages_share_the_original_deadline() {
        use crate::runtime::test_support::SlowProjection;
        use std::time::{Duration, Instant};
        for publish in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (config, _) = fresh_injected_attached_host(root.path());
            let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
            let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
            host.config.store.save(&mut host.config.session).unwrap();
            host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                host.config.store.clone(),
                SlowProjection {
                    delay: Duration::from_secs(1),
                    refuse: false,
                },
            ));
            host.quit_started_at = Instant::now().checked_sub(Duration::from_millis(900));
            let started = Instant::now();
            if publish {
                host.publish_projected_transcript(false);
            } else {
                host.finish_projection(false).unwrap();
            }
            assert!(
                started.elapsed() < Duration::from_millis(300),
                "quit projection stacked another bound: {:?}",
                started.elapsed()
            );
            let started = Instant::now();
            host.finish_projection(false).unwrap();
            host.release().unwrap();
            assert!(
                started.elapsed() < Duration::from_millis(300),
                "finish/tree/post-release stacked another bound: {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn terminal_committed_during_release_is_projected_before_shutdown_returns() {
        use plakous::agent::AgentEventKind;
        use plakous::model::ModelEvent;
        use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest};

        let root = tempfile::tempdir().unwrap();
        let plan = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version":1,
                "steps":[{"stream":[{"text":"release prefix"},{"stall":30000}]}]
            }))
            .unwrap(),
        );
        let (config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        let sessions = host.opened.shared_sessions();
        let runtime_id = host.opened.locator().session_id();
        let agent = host.opened.agent().unwrap().clone();
        let turn = host.executor.block_on(async {
            let mut turn = agent.prompt("release while active").await.unwrap();
            let mut events = turn.take_events().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let event = events.recv().await.unwrap();
                    if matches!(event.kind(), AgentEventKind::Model { event: Some(model), .. }
                        if matches!(model.as_ref(), ModelEvent::TextDelta(text) if text.bytes() == b"release prefix")) {
                        break;
                    }
                }
            }).await.unwrap();
            turn
        }).unwrap();
        host.finish_projection(false).unwrap();
        let before = store.load_views(&id).unwrap().unwrap();
        assert!(before.state.head_working);
        assert!(before.state.open_turns.contains_key(&turn.id().to_string()));
        super::super::super::projection::probe::track(&id);
        let home = host.config.home.clone();
        let ledger = host.session_ledger.clone();
        let before_ledger = ledger.project();
        events.try_iter().for_each(drop);
        host.release().unwrap();
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, crate::surface_protocol::HostEvent::TurnUsage { .. }))
        );
        assert_eq!(
            ledger.project(),
            before_ledger,
            "post-release drain is log-only"
        );
        let rows = crate::session_log::read_tail(&home, &id, 100).unwrap();
        let usage = rows
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| row["kind"] == "turn.usage")
            .collect::<Vec<_>>();
        assert_eq!(
            usage.len(),
            1,
            "release terminal usage must reach the session log"
        );
        assert_eq!(usage[0]["turn_id"], turn.id().to_string());
        let counts = super::super::super::projection::probe::take(&id);
        assert_eq!(counts.release, 1);
        assert!(counts.writes > 0);
        let after = store.load_views(&id).unwrap().unwrap();
        assert!(after.state.released);
        assert!(!after.state.head_working);
        assert!(after.state.open_turns.is_empty());
        assert!(
            after
                .rows
                .values()
                .any(|row| *row == crate::sessions::TranscriptRow::Interrupted)
        );
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        executor
            .block_on(async {
                let FeedPage::Records { records, tail, .. } = sessions
                    .feed_page(FeedPageRequest::after(FeedCursor::new(
                        runtime_id,
                        before.cursor,
                    )))
                    .await
                    .unwrap()
                else {
                    panic!("fixture feed pruned")
                };
                assert!(records.iter().flat_map(plakous::sessions::feed::FeedRecord::facts).any(|fact|
                matches!(fact, FeedFact::TurnTerminal { turn_id, .. } if *turn_id == turn.id())));
                assert_eq!(after.cursor, tail);
                assert!(matches!(
                    turn.result().await.unwrap().outcome(),
                    plakous::agent::TurnResultOutcome::Cancelled(_)
                ));
            })
            .unwrap();
    }

    #[test]
    fn post_release_projection_refusal_is_cleanup_and_preserves_release_failure() {
        for fail_child in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (config, _) = fresh_injected_attached_host(root.path());
            if fail_child {
                let executor = plakous::application::ApplicationExecutor::new().unwrap();
                executor
                    .block_on(crate::runtime::test_kit::install_failing_child_shutdown(
                        &config.session.id,
                        &config.session.workspace_root,
                    ))
                    .unwrap();
            }
            let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
            let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
            host.config.store.save(&mut host.config.session).unwrap();
            let inner = host.config.store.clone();
            let id = host.config.session.id.clone();
            host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                inner.clone(),
                crate::runtime::test_support::SlowProjection {
                    delay: std::time::Duration::ZERO,
                    refuse: true,
                },
            ));
            let error = host.release().unwrap_err();
            let failures = error.shutdown_failures().unwrap();
            assert!(
                failures
                    .cleanup
                    .as_ref()
                    .unwrap()
                    .message()
                    .contains("injected projector commit refusal")
            );
            assert_eq!(failures.release.is_some(), fail_child);
            if fail_child {
                assert!(
                    failures
                        .release
                        .as_ref()
                        .unwrap()
                        .message()
                        .contains("injected child release refusal")
                );
            }
            let (mut config, _) = fresh_injected_attached_host(root.path());
            config.store = inner.clone();
            config.session = inner.load(&id).unwrap();
            let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            HostLoop::open(config, &rx, Sender::over(tx), None)
                .unwrap()
                .release()
                .unwrap();
        }
    }

    #[test]
    fn child_retirement_failure_releases_without_a_product_save() {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        executor
            .block_on(crate::runtime::test_kit::install_failing_child_shutdown(
                &config.session.id,
                &config.session.workspace_root,
            ))
            .unwrap();
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            store.clone(),
            SaveFailure {
                fail: AtomicBool::new(true),
            },
        ));
        let error = host.release().unwrap_err();
        let failures = error.shutdown_failures().unwrap();
        assert!(failures.cleanup.is_none());
        assert!(
            failures
                .release
                .as_ref()
                .unwrap()
                .message()
                .contains("injected child release refusal")
        );
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        HostLoop::open(config, &rx, Sender::over(tx), None)
            .unwrap()
            .release()
            .unwrap();
    }

    #[test]
    fn release_without_a_product_save_fences_a_retained_agent_and_reopens() {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        let old_agent = host.opened.agent().unwrap().clone();
        let (mut contender, _) = fresh_injected_attached_host(root.path());
        contender.store = host.config.store.clone();
        contender.session = contender.store.load(&host.config.session.id).unwrap();
        let error = host
            .executor
            .block_on(build_session_from_config(&mut contender, &mut Vec::new()))
            .unwrap()
            .err()
            .expect("a live owner must exclude a second owner");
        assert!(error.message().contains("ownership"), "{error:?}");
        let store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            host.config.store.clone(),
            SaveFailure {
                fail: AtomicBool::new(true),
            },
        ));
        host.config.store = store.clone();
        let id = host.config.session.id.clone();
        host.release().unwrap();
        store.faults.fail.store(false, Ordering::SeqCst);
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.inner.clone();
        config.session = config.store.load(&id).unwrap();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let reopened = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        assert!(
            reopened
                .executor
                .block_on(old_agent.prompt("must not run"))
                .unwrap()
                .is_err()
        );
        drop(actions);
        reopened.release().unwrap();
    }
}
