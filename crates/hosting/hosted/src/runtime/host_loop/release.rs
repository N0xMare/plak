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
mod tests;
