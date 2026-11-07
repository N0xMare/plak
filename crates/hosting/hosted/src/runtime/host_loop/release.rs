//! Release canonical ownership; the feed projector owns the head view.

use super::{CliFailure, CliResult, RootActor, delete_unpublished_runtime};

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Releases the attachment, then ends every task the session started:
    /// no session task outlives release.
    pub(super) async fn release(&mut self) -> CliResult<()> {
        let released = self.release_attachment().await;
        self.role.executor.end_tasks().await;
        released
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Host release drains terminal publication and pending cancellations before dropping runtime and topology ownership."
    )]
    async fn release_attachment(&mut self) -> CliResult<()> {
        // Taken first, whether or not this release succeeds: a later
        // release never inherits a planned handoff.
        let handoff = self.role.actions.take_planned_handoff();
        self.role.pending_resume = None;
        self.quit_abandoned_turn =
            self.role.events.quit_receipt.outcome() == super::super::HostQuitOutcome::Escalated;
        let abandon = self.quit_abandoned_turn;
        self.opened.close_admission();
        let published = self.role.config.session.store_revision != 0;
        if !self.role.projection_finished {
            self.finish_projection(false).await?;
        }
        // The handoff record is durable before any lease is released.
        // The tree's runtime is kept until the release ends, which gives
        // its custody up explicitly (spec 03 §14).
        let handed_off_tree = if handoff {
            self.record_planned_handoff().await;
            crate::topology_runtime::lookup(&super::durable_tree_id(
                &self.role.config.topology_rows(),
                &self.role.config.session,
            ))
        } else {
            None
        };
        if abandon && let Some(agent) = self.opened.agent() {
            agent.begin_abandon();
        }
        let registry = self.role.config.state_roots.job_registry().clone();
        let deadline = self.quit_started_at.unwrap_or_else(super::Instant::now)
            + std::time::Duration::from_secs(1);
        let job_budget = || {
            deadline
                .saturating_duration_since(super::Instant::now())
                .saturating_sub(std::time::Duration::from_millis(150))
                .min(std::time::Duration::from_millis(400))
        };
        let keep_jobs = self.role.config.state_roots.daemon_processes();
        let stop_failure = if keep_jobs {
            None
        } else if abandon {
            let pending = self.quit_job_stops.take();
            let tree_id = super::durable_tree_id(
                &self.role.config.topology_rows(),
                &self.role.config.session,
            );
            let root = self.opened.agent().cloned();
            async {
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
            }
            .await
            .err()
        } else {
            async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    registry.stop_all(std::time::Duration::from_millis(200)),
                )
                .await
                .map_err(|_| "background process stop deadline exceeded".to_owned())?
            }
            .await
            .err()
        };
        self.update_background_process_count();
        let runtime_session = self.opened.locator().session_id();
        let released = if abandon {
            Ok(self
                .opened
                .take_for_release()
                .abandon_and_release(job_budget())
                .await)
        } else {
            tokio::time::timeout(
                super::super::HOST_RELEASE_DEADLINE,
                self.opened.take_for_release().release(),
            )
                .await
                .map_err(|_| CliFailure::new(
                    "session release exceeded its shutdown deadline; cleanup is incomplete and recovery may be required",
                ))
        };
        let trees_failure = if abandon {
            None
        } else {
            // The attachments the host's switches left behind are released
            // first, so a tree a hand-off among them kept attached is
            // unattached now. Their join blocks, so it runs off the actor.
            let left_behind = std::mem::take(&mut self.role.config.source_releases);
            let joined = tokio::task::spawn_blocking(move || drop(left_behind)).await;
            let ended = end_handed_off_trees().await;
            match (joined, ended) {
                (Ok(()), ended) => ended,
                (Err(_), Some(ended)) => Some(format!("source releases did not join; {ended}")),
                (Err(_), None) => Some("source releases did not join".to_owned()),
            }
        };
        let stop_failure = match (stop_failure, trees_failure) {
            (Some(stop), Some(trees)) => Some(format!("{stop}; {trees}")),
            (stop, trees) => stop.or(trees),
        };
        // A planned handoff gives the tree's custody up here, cut short or
        // not, not when the last driver clone drops: a child the release let
        // go of while it ran holds a clone until its task ends. Its runtime
        // branch lease guards its session meanwhile, so the next host's
        // reopen of it is refused until then and retries.
        if let Some(runtime) = handed_off_tree
            && let Err(error) = runtime.driver().release_custody()
        {
            super::log_ev(
                self.role.log.as_ref(),
                &self.role.config,
                super::SessionEvent::new("session.handoff_custody_kept")
                    .str("error", error.to_string()),
            );
        }
        // A planned handoff that let go of a child still running is no
        // failure: its handoff row stands, and the next host continues it
        // once (spec 03 §14.1). A plain quit's is a loss, and fails.
        let handed_off_cut_short = match &released {
            Ok(Err(error)) if handoff => error.only_children_cut_short().map(<[String]>::to_vec),
            _ => None,
        };
        let (locator, release_failure) = match released {
            Ok(Ok(locator)) => (Some(locator), None),
            Ok(Err(error)) if let Some(children) = &handed_off_cut_short => {
                self.note_cut_short_handoff(children);
                (error.released_locator(), None)
            }
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
            let projected = async {
                let sessions = self.role.config.state_roots.runtime_sessions()?;
                let pass = super::super::projection::project_to_tail(
                    &sessions,
                    &self.role.config.store,
                    &self.role.config.session.id,
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
                    projected.map(|_| ()).map_err(CliFailure::from)
                } else {
                    super::log_ev(
                        self.role.log.as_ref(),
                        &self.role.config,
                        super::SessionEvent::new("projector.deferred").str(
                            "error",
                            "the post-release projector pass exceeded its bound",
                        ),
                    );
                    Ok(())
                }
            }
            .await;
            match projected {
                Ok(()) => None,
                Err(error)
                    if self.quit_started_at.is_some()
                        && error.store_refusal()
                            == Some(crate::error::StoreRefusal::StorageFull) =>
                {
                    super::log_ev(
                        self.role.log.as_ref(),
                        &self.role.config,
                        super::SessionEvent::new("projector.deferred")
                            .str("error", error.message()),
                    );
                    None
                }
                Err(error) => Some(error),
            }
        } else {
            delete_unpublished_runtime(&self.role.config.state_roots, locator)
                .await
                .err()
        };
        for terminal in super::take_pending_usage(&self.role.pending_usage, runtime_session) {
            super::super::usage::log_projected_turn_usage(
                &self.role.config.session.id,
                terminal.turn_id,
                terminal.usage,
                self.role.log.as_ref(),
            );
        }
        CliFailure::shutdown(release_failure, cleanup_failure)
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Says which children a planned handoff let go of while their turn was
    /// still running: the next host continues each once.
    fn note_cut_short_handoff(&self, children: &[String]) {
        let names = children.join(", ");
        super::log_ev(
            self.role.log.as_ref(),
            &self.role.config,
            super::SessionEvent::new("session.handoff_cut_short").str("children", &names),
        );
        let _ = self.role.events.send(crate::surface_protocol::ContentEvent::Row(
            crate::sessions::TranscriptRow::warning(
                crate::sessions::NoticeScope::Tree,
                format!(
                    "handed off {names} with its turn still running; the next host continues it once"
                ),
            ),
        ));
    }
}

/// The host's quit ends every tree a rebind's hand-off left in custody with
/// its joined shutdown (spec 03 §14).
async fn end_handed_off_trees() -> Option<String> {
    match tokio::time::timeout(
        super::super::HOST_RELEASE_DEADLINE,
        crate::topology_runtime::shut_down_handed_off_trees(),
    )
    .await
    {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("agent tree shutdown exceeded its deadline".to_owned()),
    }
}

#[cfg(test)]
mod tests;
