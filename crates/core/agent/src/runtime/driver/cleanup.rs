//! Joined process and interpreter lifetime cleanup.

use crate::manifest::{ProcessSessionId, ProcessSessionOwnerGeneration};
use crate::runtime::OperationId;
use crate::runtime::checkpoint::io::DurabilityCommitTelemetry;
use crate::runtime::cleanup::{
    combine_cleanup_results, contain_cleanup_panic, error_with_cleanup_disposition,
};
use crate::runtime::composition::LocalCompositionShutdownTelemetry;
use crate::runtime::composition::projection::local_composition_started_error;
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::driver::Driver;
use crate::runtime::terminal::tool_cleanup_error;
use crate::{
    AgentError, AgentErrorKind, DurabilityAttemptKind, DurableOperationKind, DurableOperationState,
    RuntimeComponentKind, StoreAdmission,
};
use plakous_model::{Digest, DigestDomain, TurnId};
use std::collections::BTreeSet;
use std::num::NonZeroU64;
use std::sync::Arc;
/// Reports whether cleanup lost a child this turn is still accountable for.
///
/// The two sets answer different questions, and equality was the wrong test
/// between them. `durable` is what this turn dispatched and has not resolved.
/// `live` is everything the backend still holds, which by design includes
/// sessions that have already exited and been joined — those are retained so
/// their final output stays drainable and their exit status readable, which is
/// what makes polling a finished session work — and sessions an earlier turn
/// owns, because the retention outlives the turn that created it.
///
/// So a live session with no dispatched record for this turn is ordinary. The
/// product started that child, joined it, and is deliberately remembering it,
/// and its outcome is already a definite durable record written when it was
/// joined. Reading that as a lost correlation fenced the session over the
/// product's own bookkeeping, and nothing needs restating here: cleanup
/// correlates, it does not re-record.
///
/// One direction is a genuine loss: a session this turn is accountable for that
/// the backend no longer holds, where nothing can say whether the child was
/// reaped. A backend that reports itself undrained while holding nothing live
/// contradicts itself, and that is a loss too.
fn process_session_correlation_is_lost(
    durable: &BTreeSet<([u8; 16], u64)>,
    live: &BTreeSet<([u8; 16], u64)>,
    drained: bool,
) -> bool {
    !durable.is_subset(live) || (!drained && live.is_empty())
}

impl Driver {
    pub(super) async fn cleanup_session(&mut self) -> Result<(), AgentError> {
        let telemetry = DurabilityCommitTelemetry {
            emitter: Arc::clone(&self.telemetry),
            clock: Arc::clone(&self.clock),
            context: self.telemetry_context,
        };
        let release_mode = *self
            .job_release_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        plakous_tools::internal::close_job_admission(
            &self.job_sessions,
            &self.agent_id.to_string(),
        );
        let shutdown_telemetry = LocalCompositionShutdownTelemetry {
            emitter: Arc::clone(&self.telemetry),
            clock: Arc::clone(&self.clock),
            context: self.telemetry_context,
        };
        let local_composition_result = if let Some(composition) = self.local_composition.as_mut() {
            contain_cleanup_panic(composition.retire_session(shutdown_telemetry)).await
        } else {
            Ok(())
        };
        let tool_started = std::time::Instant::now();
        let tool_result = contain_cleanup_panic(async {
            tokio::time::timeout(
                self.config.model_cleanup_timeout,
                self.tools.shutdown_tasks(),
            )
            .await
            .map_err(|_| tool_cleanup_error())
            .and_then(|shutdown| {
                if shutdown.is_closed() && shutdown.is_drained() {
                    Ok(())
                } else {
                    Err(tool_cleanup_error())
                }
            })
        })
        .await;
        telemetry.emit_cleanup(
            tool_started.elapsed(),
            DurabilityAttemptKind::CleanupToolDrain,
            &tool_result,
        );
        let jobs_started = std::time::Instant::now();
        let jobs_result = if release_mode == super::super::JobReleaseMode::Terminal {
            let sessions = self.job_sessions.clone();
            let owner = self.agent_id.to_string();
            tokio::task::spawn_blocking(move || {
                plakous_tools::internal::stop_jobs_for_owner(
                    &sessions,
                    &owner,
                    plakous_tools::JobStopReason::SessionExit,
                    std::time::Duration::from_millis(200),
                )
            })
            .await
            .map_err(|_| tool_cleanup_error())
            .and_then(|result| result.map_err(|_| tool_cleanup_error()))
        } else {
            Ok(())
        };
        let job_records_result = contain_cleanup_panic(self.record_job_terminals())
            .await
            .map(|_| ());
        telemetry.emit_cleanup(
            jobs_started.elapsed(),
            DurabilityAttemptKind::CleanupJobTerminals,
            if jobs_result.is_err() {
                &jobs_result
            } else {
                &job_records_result
            },
        );
        let [heartbeat_result, store_result] = self
            .authority
            .retire(self.config.model_cleanup_timeout, Some(&telemetry))
            .await;
        let events_result = contain_cleanup_panic(async {
            let _ = self.events.close();
            Ok(())
        })
        .await;
        combine_cleanup_results([
            local_composition_result,
            tool_result,
            jobs_result,
            job_records_result,
            heartbeat_result,
            store_result,
            events_result,
        ])
    }

    pub(super) async fn apply_process_session_update(
        &mut self,
        turn_id: TurnId,
        update: plakous_tools::internal::ProcessSessionUpdate,
    ) -> Result<(), AgentError> {
        match update {
            plakous_tools::internal::ProcessSessionUpdate::Promoted { number, .. } => {
                if self
                    .authority
                    .checkpoint()
                    .manifest()
                    .jobs()
                    .rows()
                    .iter()
                    .any(|row| row.number == number)
                    || self
                        .pending_job_promotions
                        .iter()
                        .any(|row| row.number == number)
                {
                    return Ok(());
                }
                let view = self
                    .job_sessions
                    .jobs()
                    .into_iter()
                    .find(|job| job.number == number)
                    .ok_or_else(|| {
                        AgentError::new(
                            AgentErrorKind::OutcomeUnknown,
                            "promoted job lost its process owner",
                        )
                    })?;
                self.pending_job_promotions.push(crate::manifest::JobRow {
                    number,
                    terminal: None,
                    command: plakous_tools::internal::sanitize_job_command(&view.command),
                    started_at: view.started_at,
                    owner: view.owner,
                    session_id: ProcessSessionId::new(view.session_id)
                        .map_err(runtime_manifest_error)?,
                    owner_generation: ProcessSessionOwnerGeneration::new(view.owner_generation)
                        .map_err(runtime_manifest_error)?,
                });
                return Ok(());
            }
            plakous_tools::internal::ProcessSessionUpdate::JobTerminal { number } => {
                if !self.local_composition.is_in_flight()
                    && !self.pending_job_consumptions.contains(&number)
                {
                    self.pending_job_consumptions.push(number);
                }
                return Ok(());
            }
            plakous_tools::internal::ProcessSessionUpdate::JobTerminals { numbers } => {
                if !self.local_composition.is_in_flight() {
                    for number in numbers {
                        if !self.pending_job_consumptions.contains(&number) {
                            self.pending_job_consumptions.push(number);
                        }
                    }
                }
                return Ok(());
            }
            _ => {}
        }
        let (joined, session_bytes, generation) = match update {
            plakous_tools::internal::ProcessSessionUpdate::Active {
                session_id,
                owner_generation,
            } => (false, session_id, owner_generation.get()),
            plakous_tools::internal::ProcessSessionUpdate::Joined {
                session_id,
                owner_generation,
            } => (true, session_id, owner_generation.get()),
            plakous_tools::internal::ProcessSessionUpdate::Promoted { .. }
            | plakous_tools::internal::ProcessSessionUpdate::JobTerminal { .. }
            | plakous_tools::internal::ProcessSessionUpdate::JobTerminals { .. } => {
                unreachable!("handled above")
            }
        };
        let session_id = ProcessSessionId::new(session_bytes).map_err(runtime_manifest_error)?;
        let owner_generation =
            ProcessSessionOwnerGeneration::new(generation).map_err(runtime_manifest_error)?;
        let existing = {
            let authority = self.authority.checkpoint();
            authority
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .find_map(|record| match record.kind() {
                    DurableOperationKind::ProcessSession {
                        turn_id: record_turn,
                        session_id: record_session,
                        owner_generation: record_generation,
                    } if *record_session == session_id => Some((
                        OperationId(record.operation_id()),
                        *record_turn,
                        *record_generation,
                        record.state().clone(),
                    )),
                    _ => None,
                })
        };
        let Some((operation_id, record_turn, record_generation, state)) = existing else {
            if joined {
                // A foreground process may start and prove its join within one
                // executor call. Its enclosing Effect is the complete durable
                // operation; no spanning lifetime was ever exposed. Its join
                // can still retire an older exposed lifetime in the backend.
                return self.reconcile_joined_process_sessions(turn_id).await;
            }
            let generation_bytes = generation.to_be_bytes();
            let binding = Digest::sha256(
                DigestDomain::ProviderOperation,
                2,
                &[
                    b"process-session-lifetime",
                    turn_id.as_uuid().as_bytes(),
                    &session_bytes,
                    &generation_bytes,
                ],
            )
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "process session identity could not be digested",
                )
            })?;
            let operation_id = self
                .prepare_operation(
                    StoreAdmission::Settlement,
                    turn_id,
                    DurableOperationKind::ProcessSession {
                        turn_id,
                        session_id,
                        owner_generation,
                    },
                    binding,
                    None,
                    None,
                )
                .await?;
            // The process already started; recording its lifetime settles it.
            self.transition_operation_as(
                StoreAdmission::Settlement,
                turn_id,
                operation_id,
                DurableOperationState::Dispatched,
            )
            .await?;
            return self.reconcile_joined_process_sessions(turn_id).await;
        };
        if record_turn != turn_id || record_generation != owner_generation {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "process session update belongs to a foreign turn or owner generation",
            ));
        }
        if !joined {
            return match state {
                DurableOperationState::Dispatched => {
                    self.reconcile_joined_process_sessions(turn_id).await
                }
                DurableOperationState::Finalized(_) => {
                    // The backend may have joined an exited child while output
                    // remains paginated. The next bounded page still announces
                    // Active, but this exact durable lifetime already carries
                    // joined proof. A delayed page can also arrive after the
                    // backend evicts its bounded proof tombstone.
                    self.reconcile_joined_process_sessions(turn_id).await
                }
                _ => Err(AgentError::new(
                    AgentErrorKind::OutcomeUnknown,
                    "active process session contradicts its durable lifetime state",
                )),
            };
        }
        match state {
            DurableOperationState::Dispatched => {
                let evidence = runtime_component(
                    RuntimeComponentKind::Operation,
                    serde_json::to_vec(&(
                        "joined-process-session-v1",
                        turn_id,
                        session_bytes,
                        generation,
                    ))
                    .map_err(|_| {
                        AgentError::new(
                            AgentErrorKind::Internal,
                            "joined process proof could not be encoded",
                        )
                    })?,
                )?;
                self.transition_operation(
                    turn_id,
                    operation_id,
                    DurableOperationState::Completed(evidence),
                )
                .await?;
                self.finalize_operation(turn_id, operation_id).await?;
                Ok(())
            }
            DurableOperationState::Completed(_) => {
                self.finalize_operation(turn_id, operation_id).await?;
                Ok(())
            }
            DurableOperationState::Finalized(_) => Ok(()),
            _ => Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "joined process session contradicts its durable lifetime state",
            )),
        }
    }

    /// A later start may retire an earlier exited handle. Resolve its durable
    /// lifetime while the backend still holds joined proof, before its bounded
    /// tombstone can also be evicted by a long sequential tool program.
    async fn reconcile_joined_process_sessions(
        &mut self,
        turn_id: TurnId,
    ) -> Result<(), AgentError> {
        let Some(sessions) = self.tools.existing_joined_process_sessions() else {
            return Ok(());
        };
        let joined = plakous_tools::internal::joined_process_session_identities(&sessions)
            .into_iter()
            .map(|(session_id, generation)| (session_id, generation.get()))
            .collect::<BTreeSet<_>>();
        let unresolved = {
            let authority = self.authority.checkpoint();
            authority
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .filter_map(|record| match (record.kind(), record.state()) {
                    (
                        DurableOperationKind::ProcessSession {
                            turn_id: record_turn,
                            session_id,
                            owner_generation,
                        },
                        DurableOperationState::Dispatched,
                    ) if *record_turn == turn_id => {
                        let identity = (session_id.as_bytes(), owner_generation.get());
                        joined.contains(&identity).then_some(identity)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for (session_id, generation) in unresolved {
            let owner_generation = NonZeroU64::new(generation).ok_or_else(|| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "process session owner generation became zero",
                )
            })?;
            Box::pin(self.apply_process_session_update(
                turn_id,
                plakous_tools::internal::ProcessSessionUpdate::Joined {
                    session_id,
                    owner_generation,
                },
            ))
            .await?;
        }
        Ok(())
    }

    pub(super) async fn cleanup_process_sessions(
        &mut self,
        turn_id: TurnId,
    ) -> Result<(), AgentError> {
        let local_result = Box::pin(contain_cleanup_panic(
            self.cleanup_local_composition(turn_id),
        ))
        .await;
        let process_result = Box::pin(contain_cleanup_panic(
            self.cleanup_joined_process_sessions(turn_id),
        ))
        .await;
        combine_cleanup_results([local_result, process_result])
    }

    async fn cleanup_joined_process_sessions(&mut self, turn_id: TurnId) -> Result<(), AgentError> {
        if self.process_session_cleanup_uncertainty == Some(turn_id) {
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "process session cleanup already requires exact recovery",
            ));
        }
        self.reconcile_joined_process_sessions(turn_id).await?;
        let active = {
            let authority = self.authority.checkpoint();
            authority
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .filter_map(|record| match (record.kind(), record.state()) {
                    (
                        DurableOperationKind::ProcessSession {
                            turn_id: record_turn,
                            session_id,
                            owner_generation,
                        },
                        DurableOperationState::Dispatched,
                    ) if *record_turn == turn_id => Some((*session_id, *owner_generation)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let Some(sessions) = self.tools.existing_joined_process_sessions() else {
            if active.is_empty() {
                return Ok(());
            }
            self.process_session_cleanup_uncertainty = Some(turn_id);
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "durable process ownership exists without live cleanup authority",
            ));
        };
        let durable_identities = active
            .iter()
            .map(|(session_id, generation)| (session_id.as_bytes(), generation.get()))
            .collect::<BTreeSet<_>>();
        let live_before = plakous_tools::internal::active_process_session_identities(&sessions)
            .into_iter()
            .map(|(session_id, generation)| (session_id, generation.get()))
            .collect::<BTreeSet<_>>();
        let identity_mismatch = process_session_correlation_is_lost(
            &durable_identities,
            &live_before,
            sessions.is_drained(),
        );
        if identity_mismatch {
            self.process_session_cleanup_uncertainty = Some(turn_id);
        }
        let cleanup_sessions = sessions.clone();
        let cleanup_ids = cleanup_sessions
            .active_session_ids()
            .into_iter()
            .filter(|id| !cleanup_sessions.is_promoted(*id))
            .collect::<Vec<_>>();
        // `join_all` enforces the same backend cleanup deadline itself.  Do not
        // wrap its blocking owner in an async timeout: dropping a timed-out
        // `JoinHandle` detaches the cleanup, allowing it to retire children
        // after this driver has stopped correlating their durable lifetimes.
        let cleanup = tokio::task::spawn_blocking(move || {
            let mut failure = None;
            for id in cleanup_ids {
                if let Err(error) = cleanup_sessions.terminate(id) {
                    failure.get_or_insert(error);
                }
            }
            failure.map_or(Ok(()), Err)
        })
        .await;
        let live_after = plakous_tools::internal::active_process_session_identities(&sessions)
            .into_iter()
            .map(|(session_id, generation)| (session_id, generation.get()))
            .collect::<BTreeSet<_>>();
        let joined = live_before
            .difference(&live_after)
            .filter(|identity| durable_identities.contains(identity))
            .copied()
            .collect::<Vec<_>>();
        let mut update_error = None;
        for (session_id, owner_generation) in joined {
            let Some(owner_generation) = NonZeroU64::new(owner_generation) else {
                update_error.get_or_insert_with(|| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "process session owner generation became zero",
                    )
                });
                continue;
            };
            if let Err(error) = self
                .apply_process_session_update(
                    turn_id,
                    plakous_tools::internal::ProcessSessionUpdate::Joined {
                        session_id,
                        owner_generation,
                    },
                )
                .await
            {
                self.process_session_cleanup_uncertainty = Some(turn_id);
                update_error.get_or_insert(error);
            }
        }
        if !matches!(cleanup, Ok(Ok(()))) {
            self.process_session_cleanup_uncertainty = Some(turn_id);
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "process session cleanup did not prove every child joined",
            ));
        }
        if durable_identities
            .iter()
            .any(|identity| live_after.contains(identity))
        {
            self.process_session_cleanup_uncertainty = Some(turn_id);
            return Err(AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "process session cleanup retained an active owner",
            ));
        }
        if identity_mismatch || self.process_session_cleanup_uncertainty == Some(turn_id) {
            let correlation = AgentError::new(
                AgentErrorKind::OutcomeUnknown,
                "process session cleanup lost a child this turn is accountable for",
            );
            return Err(update_error.map_or(correlation.clone(), |error| {
                error_with_cleanup_disposition(error, &[correlation])
            }));
        }
        if let Some(error) = update_error {
            return Err(error);
        }
        Ok(())
    }

    async fn cleanup_local_composition(&mut self, turn_id: TurnId) -> Result<(), AgentError> {
        let shutdown_telemetry = self.local_composition_shutdown_telemetry_for_turn(turn_id);
        let Some(state) = self.local_composition.as_mut() else {
            return if self.local_composition.is_in_flight() {
                Err(local_composition_started_error(
                    "local composition cleanup owner was lost while an evaluation might be active",
                ))
            } else {
                Ok(())
            };
        };
        let evaluation = state.cleanup_at_root_boundary(shutdown_telemetry).await?;
        if let Some(evaluation) = evaluation {
            self.transition_operation(
                turn_id,
                evaluation,
                DurableOperationState::Completed(runtime_component(
                    RuntimeComponentKind::Operation,
                    b"local-composition-cell-terminated-shutdown-joined".to_vec(),
                )?),
            )
            .await?;
            self.finalize_operation(turn_id, evaluation).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::process_session_correlation_is_lost;
    use std::collections::BTreeSet;

    fn identities(ids: &[u8]) -> BTreeSet<([u8; 16], u64)> {
        ids.iter()
            .map(|id| {
                let mut bytes = [0_u8; 16];
                bytes[15] = *id;
                (bytes, 1)
            })
            .collect()
    }

    /// On the cleanup path, a session the product
    /// started, joined and is deliberately remembering was read as a lost
    /// correlation, and fenced the session.
    ///
    /// A finished session stays in the backend so its final output can still be
    /// drained and its exit status still read, and that retention outlives the
    /// turn that created it. Its durable record has already left `Dispatched`,
    /// so the live set is a superset of the durable one whenever a child has
    /// finished or an earlier turn owns one. That is the ordinary case, not a
    /// loss, and it happens on every turn that runs a command to completion.
    #[test]
    fn a_retained_or_foreign_session_is_not_a_lost_correlation() {
        // A child this turn dispatched that finished and was joined: the
        // durable set no longer names it, the backend still holds it.
        assert!(!process_session_correlation_is_lost(
            &identities(&[]),
            &identities(&[1]),
            true
        ));
        // An earlier turn's retained session alongside this turn's live one.
        assert!(!process_session_correlation_is_lost(
            &identities(&[2]),
            &identities(&[1, 2]),
            false
        ));
        // Nothing dispatched and nothing held is the quiet case.
        assert!(!process_session_correlation_is_lost(
            &identities(&[]),
            &identities(&[]),
            true
        ));
        // Exact agreement, the case that always worked.
        assert!(!process_session_correlation_is_lost(
            &identities(&[1, 2]),
            &identities(&[1, 2]),
            false
        ));
    }

    /// The other direction is still a loss, and still fences: a child this turn
    /// is accountable for that the backend no longer holds says nothing about
    /// whether that child was reaped.
    #[test]
    fn a_child_this_turn_owns_that_the_backend_dropped_is_still_a_loss() {
        assert!(process_session_correlation_is_lost(
            &identities(&[1]),
            &identities(&[]),
            true
        ));
        assert!(process_session_correlation_is_lost(
            &identities(&[1, 2]),
            &identities(&[2]),
            false
        ));
        // A generation that moved under a retained identity is a different
        // session wearing the same id, so the one this turn owns is gone.
        let durable = identities(&[1]);
        let live = durable
            .iter()
            .map(|(bytes, generation)| (*bytes, generation + 1))
            .collect();
        assert!(process_session_correlation_is_lost(&durable, &live, true));
        // A backend that reports itself undrained while holding nothing live
        // contradicts itself.
        assert!(process_session_correlation_is_lost(
            &identities(&[]),
            &identities(&[]),
            false
        ));
    }
}
