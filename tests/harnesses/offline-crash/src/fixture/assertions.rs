use super::{
    AgentSessions, AgentSessionsError, AgentSessionsErrorKind, Arc, BlockedWorkingWorkflow,
    BoundaryController, BufReader, ChildReply, CompactReason, CompactSessionOutcome,
    CompactSessionRequest, ContentBlock, ContextQuery, ContextStatusRequest, CrashBoundary,
    CreateSessionOutcome, CreateSessionRequest, EvidenceKind, FINAL_TEXT, ForkSessionOutcome,
    ForkSessionRequest, FsyncEvidenceLog, HarnessResult, LocatorState, ModelResponse,
    OfflineReopenInspection, OfflineReopenOutcome, OpenSessionOutcome, OpenSessionRequest, Opened,
    PROTOCOL_VERSION, Path, ProbeReport, RebindSessionOutcome, RebindSessionRequest,
    RecoverSessionOutcome, RecoverSessionRequest, RecoveryAction, RecoveryFact, RecoveryPhase,
    RecoveryRequiredReport, SessionBackend, SessionContextStatusKind, SessionLocator,
    SessionOperationId, SessionRecoveryAction, SessionRecoveryPhase, SessionWorkflowUnavailable,
    TurnResultOutcome, Write, io, pause_if, runtime_binding, write_fsynced_json,
};

pub(crate) async fn run_fixture(
    database: &Path,
    workspace: &Path,
    evidence_path: &Path,
    state_path: &Path,
    cut: CrashBoundary,
) -> HarnessResult<String> {
    std::fs::create_dir_all(workspace)?;
    let evidence = Arc::new(FsyncEvidenceLog::open(evidence_path)?);
    let controller = BoundaryController::new(cut, evidence);
    let backend = SessionBackend::sqlite(database)
        .map_err(|error| session_workflow_error("backend-open", &error))?;
    let sessions = AgentSessions::new(backend)
        .map_err(|error| session_workflow_error("service-create", &error))?;
    let created = Box::pin(sessions.create(CreateSessionRequest::new(
        SessionOperationId::new("crash-harness:create")?,
        runtime_binding(workspace, controller.clone())?,
    )))
    .await
    .map_err(|error| session_workflow_error("session-create", &error))?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("session create returned an unknown outcome".into());
    };
    let old_locator = session
        .release()
        .await
        .map_err(|error| session_workflow_error("genesis-release", &error))?;
    write_locators(state_path, old_locator, old_locator)?;
    let pause = controller.announce(
        CrashBoundary::GenesisReleased,
        EvidenceKind::OldCheckpointReleased,
        None,
    )?;
    pause_if(pause).await;

    let opened = Box::pin(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("crash-harness:open-for-turn")?,
        old_locator,
        runtime_binding(workspace, controller.clone())?,
    )))
    .await
    .map_err(|error| session_workflow_error("turn-open", &error))?;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err("released genesis did not reopen as quiescent".into());
    };
    let turn = session
        .agent()
        .prompt("perform the loopback effect")
        .await?;
    let result = turn.result().await?;
    if !matches!(result.outcome(), TurnResultOutcome::Completed(_))
        || result.response().map(ModelResponse::output_text).as_deref() != Some(FINAL_TEXT)
    {
        return Err(format!(
            "loopback turn did not produce the exact completed response: outcome={:?}, text={:?}",
            result.outcome(),
            result.response().map(ModelResponse::output_text)
        )
        .into());
    }
    let new_locator = session
        .release()
        .await
        .map_err(|error| session_workflow_error("turn-release", &error))?;
    write_locators(state_path, old_locator, new_locator)?;
    let pause = controller.announce(
        CrashBoundary::CheckpointReleased,
        EvidenceKind::NewCheckpointReleased,
        None,
    )?;
    pause_if(pause).await;
    Ok(new_locator.to_string())
}

pub(crate) async fn probe_fixture(
    database: &Path,
    workspace: &Path,
    evidence_path: &Path,
    state_path: &Path,
) -> HarnessResult<ProbeReport> {
    let state: LocatorState =
        serde_json::from_reader(BufReader::new(std::fs::File::open(state_path)?))?;
    if state.version != PROTOCOL_VERSION {
        return Err("unsupported locator-state version".into());
    }
    let source = state.source.parse::<SessionLocator>()?;
    let mut locator = state.latest.parse::<SessionLocator>()?;
    let evidence = Arc::new(FsyncEvidenceLog::open(evidence_path)?);
    let controller = BoundaryController::new(CrashBoundary::None, evidence);
    let backend = SessionBackend::sqlite(database)
        .map_err(|error| session_workflow_error("probe-backend-open", &error))?;
    let sessions = AgentSessions::new(backend)
        .map_err(|error| session_workflow_error("probe-service-create", &error))?;
    for attempt in 1_u8..=2 {
        let predicted = match predicted_reopen(database, locator) {
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                locator = error
                    .current_locator()
                    .ok_or("stale locator did not expose the current exact head")?;
                continue;
            }
            other => other?,
        };
        let opened = Box::pin(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new(format!("crash-harness:probe-open-{attempt}"))?,
            locator,
            runtime_binding(workspace, controller.clone())?,
        )))
        .await;
        match opened {
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                locator = error
                    .current_locator()
                    .ok_or("stale locator did not expose the current exact head")?;
            }
            Err(error) => {
                check_prediction(&predicted, Opened::Failed)?;
                return Err(error.into());
            }
            Ok(OpenSessionOutcome::RecoveryRequired { recovery, .. }) => {
                check_prediction(&predicted, Opened::RecoveryRequired(&recovery))?;
                Box::pin(assert_recovery_remains_non_dispatching(
                    &sessions,
                    source,
                    &recovery,
                    workspace,
                    controller.clone(),
                ))
                .await?;
                return recovery_report(source, &recovery);
            }
            Ok(OpenSessionOutcome::Opened { session, .. }) => {
                check_prediction(
                    &predicted,
                    Opened::Ordinary {
                        abandoned: session.agent().abandoned_turn().is_some(),
                    },
                )?;
                let view = session.agent().conversation();
                // A turn a lost process left is closed by this very open, which
                // abandons its unfinished model request. The session is
                // ordinary again, so it is neither of the two exact states: the
                // prompt is in the history and no answer ever will be.
                let report = match session.agent().abandoned_turn() {
                    Some(abandoned) => abandoned_report(
                        source,
                        session.locator(),
                        &view,
                        abandoned.operations(),
                        abandoned.nothing_started() || abandoned.tool_round_completed(),
                    )?,
                    None => exact_report(source, session.locator(), &view)?,
                };
                session.release().await?;
                return Ok(report);
            }
            Ok(_) => return Err("session open returned an unknown outcome".into()),
        }
    }
    Err("stale locator retry did not reach the current exact head".into())
}

/// The open did what the read-only loaders predicted for the same head.
fn check_prediction(
    predicted: &(OfflineReopenInspection, Option<RecoveryRequiredReport>),
    opened: Opened<'_>,
) -> HarnessResult {
    let (inspection, report) = predicted;
    let agrees = match opened {
        Opened::Failed => inspection.outcome == OfflineReopenOutcome::RefusesRecoveryScope,
        Opened::RecoveryRequired(recovery) => {
            matches!(
                inspection.outcome,
                OfflineReopenOutcome::RefusesRecoveryScope
                    | OfflineReopenOutcome::RestoresEffectDecision
                    | OfflineReopenOutcome::RestoresHostedContinuation
            ) && report.as_ref() == Some(recovery)
        }
        Opened::Ordinary { abandoned } => {
            let expected = if abandoned {
                &[OfflineReopenOutcome::RecoversAbandonedTurn][..]
            } else {
                &[
                    OfflineReopenOutcome::OpensQuiescent,
                    OfflineReopenOutcome::RecoversSavedTerminal,
                    OfflineReopenOutcome::ResolvesCompactionWorkflow,
                ][..]
            };
            expected.contains(&inspection.outcome)
                && (inspection.outcome == OfflineReopenOutcome::RecoversAbandonedTurn) == abandoned
                && report.is_none() == (inspection.outcome == OfflineReopenOutcome::OpensQuiescent)
        }
    };
    if agrees {
        Ok(())
    } else {
        Err(format!("the open disagreed with the predicate's prediction {predicted:?}").into())
    }
}

/// What the read-only loaders of the one recovery predicate (the rehearsal
/// and the `SQLite` recovery inspection) predict an open of this exact head
/// does, over the same bytes the open loads (spec 01 §5.5).
fn predicted_reopen(
    database: &Path,
    locator: SessionLocator,
) -> Result<(OfflineReopenInspection, Option<RecoveryRequiredReport>), AgentSessionsError> {
    let inspection = plakous::sessions::rehearsal::inspect_sqlite(database, locator)?;
    let report = AgentSessions::inspect_recovery_sqlite(database, locator)?;
    Ok((inspection, report))
}

#[allow(
    clippy::too_many_lines,
    reason = "one ordered negative proof keeps all public Working bypass workflows visibly bound to one report"
)]
async fn assert_recovery_remains_non_dispatching(
    sessions: &AgentSessions,
    source: SessionLocator,
    recovery: &RecoveryRequiredReport,
    workspace: &Path,
    controller: BoundaryController,
) -> HarnessResult {
    let recover_operation = SessionOperationId::new("crash-harness:probe-recover")?;
    let outcome = Box::pin(sessions.recover(RecoverSessionRequest::new(
        recover_operation.clone(),
        recovery.locator(),
        runtime_binding(workspace, controller.clone())?,
    )))
    .await?;
    match &outcome {
        RecoverSessionOutcome::Unavailable {
            operation_id,
            recovery: still_required,
            reason: SessionWorkflowUnavailable::WorkingRecovery,
        } if operation_id == &recover_operation && still_required == recovery => {}
        other => {
            return Err(format!(
                "working recovery guessed an outcome or changed authority: {other:?}"
            )
            .into());
        }
    }

    let context_operation = SessionOperationId::new("crash-harness:probe-context")?;
    let context = sessions
        .context_status(ContextStatusRequest::new(
            context_operation.clone(),
            recovery.locator(),
            ContextQuery::Current,
        ))
        .await?;
    if context.operation_id() != &context_operation
        || context.locator() != recovery.locator()
        || !matches!(
            context.kind(),
            SessionContextStatusKind::RecoveryRequired(still_required)
                if still_required == recovery
        )
    {
        return Err("Working context status did not preserve exact recovery authority".into());
    }

    let compact_operation = SessionOperationId::new("crash-harness:probe-compact")?;
    let compact = sessions
        .compact(CompactSessionRequest::new(
            compact_operation.clone(),
            recovery.locator(),
            CompactReason::Manual,
            runtime_binding(workspace, controller.clone())?,
        ))
        .await?;
    if !matches!(
        compact,
        CompactSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
            ..
        } if operation_id == compact_operation && still_required == *recovery
    ) {
        return Err("Working compaction escaped exact recovery authority".into());
    }

    let fork_operation = SessionOperationId::new("crash-harness:probe-fork")?;
    let fork = sessions
        .fork(ForkSessionRequest::new(
            fork_operation.clone(),
            recovery.locator(),
        ))
        .await?;
    if !matches!(
        fork,
        ForkSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
        } if operation_id == fork_operation && still_required == *recovery
    ) {
        return Err("Working fork escaped exact recovery authority".into());
    }

    let runtime = runtime_binding(workspace, controller.clone())?;
    let target = runtime.target_binding();
    let rebind_operation = SessionOperationId::new("crash-harness:probe-rebind")?;
    let rebound = Box::pin(sessions.rebind(RebindSessionRequest::new(
        rebind_operation.clone(),
        recovery.locator(),
        target,
        runtime,
    )))
    .await?;
    if !matches!(
        rebound,
        RebindSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
        } if operation_id == rebind_operation && still_required == *recovery
    ) {
        return Err("Working rebind escaped exact recovery authority".into());
    }

    let reopened = Box::pin(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("crash-harness:probe-reopen-working")?,
        recovery.locator(),
        runtime_binding(workspace, controller)?,
    )))
    .await?;
    if !matches!(
        reopened,
        OpenSessionOutcome::RecoveryRequired {
            recovery: still_required,
            ..
        } if still_required == *recovery
    ) {
        return Err("Working checkpoint changed after blocked bypass operations".into());
    }
    if source.session_id() != recovery.locator().session_id()
        || source.branch_id() != recovery.locator().branch_id()
        || source.checkpoint_id() == recovery.locator().checkpoint_id()
    {
        return Err("Working checkpoint does not preserve exact source lineage".into());
    }
    Ok(())
}

/// Classifies the quiescent state a reopen reached by abandoning a turn.
fn abandoned_report(
    source: SessionLocator,
    locator: SessionLocator,
    view: &plakous::model::ConversationView,
    operations: &[u64],
    nothing_open: bool,
) -> HarnessResult<ProbeReport> {
    // A turn cut at a round boundary, or before anything it asked for
    // started, closes with no open operation of its own.
    if operations.is_empty() && !nothing_open {
        return Err("an abandoned turn reported no abandoned operation".into());
    }
    let note = view
        .items()
        .into_iter()
        .filter_map(|item| {
            item.blocks().first().and_then(|block| match block {
                ContentBlock::Text(text) => Some(text.text().to_owned()),
                _ => None,
            })
        })
        // Three claims, not two. An abandonment always writes a note; only the
        // wording varies, and the strongest applicable claim wins: a running
        // tool may have changed something outside the session, an unobserved
        // request may have been billed, and exact evidence that nothing
        // started says neither.
        //
        // This detector is deliberately permissive, because its job is to find
        // *a* note so the report can be built. It cannot catch a *wrong* note,
        // and that is the expectation arms' job: each asserts the wording its
        // own cut requires. A detector that accepted anything and arms that
        // asserted nothing is how a borrowed warning ships.
        .find(|text| {
            text.contains("abandoned after its outcome became uncertain")
                || text.contains("abandoned after the process was lost while a tool was running")
                || text.contains("never started")
                || text.contains("this turn was abandoned after its tool round completed")
                || text.contains("this turn was abandoned after its model round completed")
                || text.starts_with("this turn ")
                || text.starts_with("the model request for this turn ")
        })
        .ok_or("an abandoned turn left no note in its canonical history")?;
    Ok(ProbeReport::RecoveredByAbandonment {
        source: source.to_string(),
        locator: locator.to_string(),
        canonical_items: view.items().into_iter().count(),
        abandoned_operations: operations.to_vec(),
        note,
    })
}

fn exact_report(
    source: SessionLocator,
    locator: SessionLocator,
    view: &plakous::model::ConversationView,
) -> HarnessResult<ProbeReport> {
    let items = view.items();
    if items.is_empty() {
        return Ok(ProbeReport::OldExact {
            source: source.to_string(),
            locator: locator.to_string(),
            canonical_items: 0,
        });
    }
    if items.len() != 4 {
        return Err(format!(
            "quiescent state has an impossible canonical length: {}",
            items.len()
        )
        .into());
    }
    let final_text = items
        .into_iter()
        .last()
        .and_then(|item| item.blocks().first())
        .and_then(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .ok_or("new state lacks the final assistant text")?;
    if final_text != FINAL_TEXT {
        return Err("new state contains an unexpected final assistant text".into());
    }
    Ok(ProbeReport::NewExact {
        source: source.to_string(),
        locator: locator.to_string(),
        canonical_items: items.len(),
        final_text: final_text.to_owned(),
    })
}

fn recovery_report(
    source: SessionLocator,
    recovery: &RecoveryRequiredReport,
) -> HarnessResult<ProbeReport> {
    let phase = match recovery.phase() {
        SessionRecoveryPhase::DispatchEvidence => RecoveryPhase::DispatchEvidence,
        SessionRecoveryPhase::EffectReconciliation => RecoveryPhase::EffectReconciliation,
        SessionRecoveryPhase::CommitRecovery => RecoveryPhase::CommitRecovery,
        SessionRecoveryPhase::Finalization => RecoveryPhase::Finalization,
        _ => return Err("unknown public recovery phase".into()),
    };
    let obligations = recovery
        .obligations()
        .iter()
        .map(|obligation| {
            let action = match obligation.action() {
                SessionRecoveryAction::CommitRecovery => RecoveryAction::CommitRecovery,
                SessionRecoveryAction::ProviderEvidenceRequired => {
                    RecoveryAction::ProviderEvidenceRequired
                }
                SessionRecoveryAction::EffectEvidenceRequired => {
                    RecoveryAction::EffectEvidenceRequired
                }
                SessionRecoveryAction::ProvenNotDispatched => RecoveryAction::ProvenNotDispatched,
                SessionRecoveryAction::FinalizationRequired => RecoveryAction::FinalizationRequired,
                _ => return Err("unknown public recovery action"),
            };
            Ok(RecoveryFact {
                operation_id: obligation.operation_id(),
                action,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ProbeReport::RecoveryRequired {
        source: source.to_string(),
        locator: recovery.locator().to_string(),
        phase,
        obligations,
        blocked: vec![
            BlockedWorkingWorkflow::Recover,
            BlockedWorkingWorkflow::ContextStatus,
            BlockedWorkingWorkflow::Compact,
            BlockedWorkingWorkflow::Fork,
            BlockedWorkingWorkflow::Rebind,
        ],
    })
}

fn write_locators(path: &Path, source: SessionLocator, latest: SessionLocator) -> io::Result<()> {
    write_fsynced_json(
        path,
        &LocatorState {
            version: PROTOCOL_VERSION,
            source: source.to_string(),
            latest: latest.to_string(),
        },
    )
}

fn session_workflow_error(stage: &'static str, error: &AgentSessionsError) -> io::Error {
    io::Error::other(format!("{stage} failed with {:?}", error.kind()))
}

pub(crate) fn write_reply(reply: &ChildReply) -> io::Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, reply).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}
