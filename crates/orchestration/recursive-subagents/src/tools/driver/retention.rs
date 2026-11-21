//! Child conversation retirement and candidate-pin cleanup.

use super::{ProductAgentTree, TopologyError, TopologyErrorCode, TreeDriver, VcsError};

impl TreeDriver {
    /// Retires sealed dormant conversations after publishing exact deletion intent.
    /// Unadopted nonempty work refs are retained independently of runtime sessions.
    pub async fn collect_dormant_children(
        &self,
        sessions: &plakous::sessions::AgentSessions,
    ) -> Result<(), TopologyError> {
        let driver = self.clone();
        let tickets = self
            .write(move |tree| {
                let mut snapshot = None;
                let result = tree.prepare_child_retention_guarded(
                    plakous::model::now_unix_millis(),
                    |before| {
                        snapshot = Some(before.clone());
                    },
                );
                let result = result.and_then(|tickets| {
                    // Even an old ticket needs the custody check before dispatch.
                    if !tickets.is_empty() || snapshot.is_some() {
                        driver.save_guarded(tree)?;
                    }
                    Ok(tickets)
                });
                if result.is_err()
                    && let Some(snapshot) = snapshot
                {
                    **tree = snapshot;
                }
                result
            })
            .await?;
        for ticket in tickets? {
            self.stop_agent_jobs(std::slice::from_ref(&ticket.agent_id))
                .await?;
            if let Some(raw) = &ticket.locator {
                let locator: plakous::sessions::SessionLocator = raw.parse().map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "retired child locator is invalid; session retained",
                    )
                })?;
                match sessions.delete_archived(locator.session_id()).await {
                    Ok(_) => {}
                    Err(error)
                        if error.kind()
                            == plakous::sessions::AgentSessionsErrorKind::InvalidInput =>
                    {
                        let archived = sessions.archive(locator).await.map_err(|_| TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired, "child session retirement refused; exact session and work refs retained"))?;
                        sessions
                            .delete(archived.session_id(), archived.archived_revision())
                            .await
                            .map_err(|_| {
                                TopologyError::new(
                                    TopologyErrorCode::OperationRecoveryRequired,
                                    "child session deletion remains pending; work refs retained",
                                )
                            })?;
                    }
                    Err(_) => {
                        return Err(TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "child session deletion remains pending; work refs retained",
                        ));
                    }
                }
            }
            let driver = self.clone();
            let (finished, effects) = self
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| tree.finish_child_retention(&ticket))
                })
                .await?;
            self.settle(effects).await?;
            finished?;
        }
        self.cleanup_candidate_pins().await
    }

    async fn cleanup_candidate_pins(&self) -> Result<(), TopologyError> {
        let pending = self.read(ProductAgentTree::pending_pin_cleanup)??;
        if pending.is_empty() {
            return Ok(());
        }
        let (namespace, launch, vcs) = self.read(|tree| {
            (
                plakous::model::sha256_hex(&[tree.tree_id.as_bytes()]),
                tree.launch_checkout.clone(),
                tree.vcs_port(),
            )
        })?;
        for ticket in pending {
            let unpinned = {
                let (vcs, launch, namespace, candidates) = (
                    vcs.clone(),
                    launch.clone(),
                    namespace.clone(),
                    ticket.candidates.clone(),
                );
                plakous_workspace::occupancy_vcs::stage::unstaged(
                    "candidate unpin, retained as an intent until acknowledged",
                    async move {
                        for candidate in &candidates {
                            vcs.unpin(&launch, &namespace, candidate.as_str()).await?;
                        }
                        Ok::<(), VcsError>(())
                    },
                )
                .await
            };
            if unpinned.is_err() {
                return Err(TopologyError::new(
                    TopologyErrorCode::WorkspaceRecoveryRequired,
                    "collected candidate pin cleanup remains pending; intent retained",
                ));
            }
            let driver = self.clone();
            let (finished, effects) = self
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| tree.acknowledge_pin_cleanup(&ticket))
                })
                .await?;
            self.settle(effects).await?;
            finished?;
        }
        Ok(())
    }
}
