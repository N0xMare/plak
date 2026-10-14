use super::*;
use crate::runtime::durable::turn_recovery::local_terminal_recovery_fold;

struct ArtifactFixture {
    _directory: tempfile::TempDir,
    workspace: Arc<WorkspacePolicy>,
    model: Arc<ArtifactModel>,
    store: Arc<TestCreateStore>,
    sessions: DurableSessionRuntime,
    result: Arc<TurnResult>,
    saved: CreateStoreState,
}

impl ArtifactFixture {
    fn builder_parts(
        workspace: &Arc<WorkspacePolicy>,
        model: &Arc<ArtifactModel>,
    ) -> TestResult<AgentBuilder> {
        let (registry, _) = session_update_registry(SessionUpdateFixtureMode::Active)?;
        session_update_builder(workspace.clone(), model.clone(), registry)
    }
    fn cuts(&self) -> Vec<StoredCheckpoint> {
        self.saved
            .stored_commits
            .values()
            .filter(|stored| {
                local_terminal_recovery_fold(stored.checkpoint())
                    .is_ok_and(|fold| !fold.output.is_empty())
            })
            .cloned()
            .collect()
    }
    async fn new(family: &'static str, candidate_calls: usize) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = durable_test_descriptor("test.route")?;
        let mut model = ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            candidate_calls,
            0xE8C_000,
            false,
        )
        .with_tool_first()
        .with_report_usage();
        model.native_final_family = Some(family);
        model.response_provenance = true;
        let model = Arc::new(model);
        let store = Arc::new(TestCreateStore::default());
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("lease")?,
        )?;
        let agent = sessions
            .create(
                "create:artifact-finalization",
                Self::builder_parts(&workspace, &model)?,
            )
            .await?;
        let command = CommandId::new("start:artifact-finalization")?;
        let result = agent
            .start(command.clone(), "complete with native thinking")
            .await?
            .into_result()
            .ok_or("result")?
            .result()
            .await?;
        agent.shutdown().await?;
        let saved = lock_mutex(&store.state).clone();
        Ok(Self {
            _directory: directory,
            workspace,
            model,
            store,
            sessions,
            result,
            saved,
        })
    }
}

async fn artifact_finalization_parity(family: &'static str, candidate_calls: usize) -> TestResult {
    let fixture = ArtifactFixture::new(family, candidate_calls).await?;
    let ArtifactFixture {
        workspace,
        model,
        store,
        sessions,
        result,
        saved,
        ..
    } = &fixture;
    let command = CommandId::new("start:artifact-finalization")?;
    let builder = || ArtifactFixture::builder_parts(workspace, model);
    let response = result.response().ok_or("live response")?;
    assert!(
        response
            .output()
            .iter()
            .flat_map(ConversationItem::blocks)
            .any(|block| matches!(block, ContentBlock::Opaque(_)))
    );
    let live = result.checkpoint();
    let live_artifact = live.manifest().family_artifact();
    assert!(super::super::turn_recovery::root_turn_closure_for_test(live)?.is_none());
    assert_eq!(live_artifact.is_some(), candidate_calls == 2);
    if let Some(artifact) = live_artifact {
        assert_eq!(artifact.family(), family);
    }
    let cuts = fixture.cuts();
    // The decision commit, and the two later heads a live process session
    // leaves before the consequence commit: every one is the finish row.
    assert_eq!(
        cuts.len(),
        3,
        "each cut between the decision and the consequence commit must close"
    );
    for stored in cuts {
        let checkpoint = stored.checkpoint();
        let closure = super::super::turn_recovery::root_turn_closure_for_test(checkpoint)?
            .ok_or("root closure")?;
        let fold = local_terminal_recovery_fold(checkpoint)?;
        assert_eq!(
            matches!(
                closure,
                super::super::turn_recovery::RootClosure::Complete(_)
            ),
            fold.synthesized.is_some()
        );
        assert_eq!(fold.family_artifact.as_ref(), live_artifact);
        if let Some((_, evidence)) = &fold.synthesized {
            let pending = decode_pending_terminal_record(evidence.bytes()).ok_or("terminal")?;
            assert_eq!(pending.usage, result.usage());
            assert!(
                matches!(pending.outcome, DurableTerminalOutcome::Completed(ref value) if value == response)
            );
            let live_pending = saved
                .stored_commits
                .values()
                .flat_map(|item| item.checkpoint().manifest().ledger().operations())
                .find_map(live_pending_terminal)
                .ok_or("live pending")?;
            assert_eq!(evidence.bytes(), live_pending.as_slice());
        }
        let topology = checkpoint.manifest().topology();
        let source =
            DurableSessionAddress::new(topology.session_id, topology.branch_id, checkpoint.id());
        let head = BranchHead::from_store(
            source.session(),
            BranchRevision::from_store(topology.branch_revision),
            stored.reference(),
        );
        {
            let mut state = lock_mutex(&store.state);
            *state = (*saved).clone();
            state.loaded = Some(crate::LoadedBranch::from_store(
                saved.loaded.as_ref().ok_or("saved branch")?.session(),
                head,
                stored.clone(),
            ));
            state.stored = Some(stored.clone());
            state.active_lease = None;
            state.released_lease = None;
        }
        let DurableRuntimeOpen::Opened(reopened) = sessions
            .open("open:artifact-finalization", source, builder()?)
            .await?
        else {
            return Err("artifact finalization refused".into());
        };
        let recovered = reopened.checkpoint();
        assert_eq!(
            recovered.manifest().lifecycle(),
            &RuntimeLifecycle::Quiescent
        );
        assert_eq!(
            recovered.manifest().canonical().history(),
            live.manifest().canonical().history()
        );
        assert_eq!(recovered.manifest().family_artifact(), live_artifact);
        assert_eq!(
            recovered
                .manifest()
                .family_artifact()
                .map(crate::FamilyArtifact::bytes),
            live_artifact.map(crate::FamilyArtifact::bytes)
        );
        let terminal = crate::runtime::checkpoint_root_terminal_result(&recovered, &command)?
            .ok_or("recovered terminal")?;
        assert_eq!(terminal.response(), Some(response));
        assert_eq!(
            serde_json::to_vec(&terminal.response())?,
            serde_json::to_vec(&Some(response))?
        );
        assert_eq!(
            recovered
                .manifest()
                .family_artifact()
                .map(|value| value.binding_component().bytes()),
            live_artifact.map(|value| value.binding_component().bytes())
        );
        assert_eq!(terminal.usage(), result.usage());
        reopened.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn finalization_artifact_grok_encrypted_reasoning_matches_live_bytes() -> TestResult {
    artifact_finalization_parity("responses", 2).await
}

#[tokio::test]
async fn finalization_artifact_anthropic_signed_thinking_matches_live_bytes() -> TestResult {
    artifact_finalization_parity("messages", 2).await
}

fn assert_foreign_artifact_refusals(
    checkpoint: &RuntimeCheckpoint,
    model: &ArtifactModel,
) -> TestResult {
    let provider = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .rfind(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }))
        .ok_or("provider")?;
    let DurableOperationState::Completed(evidence) = provider.state() else {
        return Err("provider not completed".into());
    };
    let (_, response) =
        crate::runtime::operation_record::test_support::recorded_response_parts(evidence)?;
    let candidate = response.family_artifact_candidate().ok_or("candidate")?;
    let facts = candidate.producer();
    let foreign = |domain, label: &'static [u8]| Digest::sha256(domain, 1, &[label]);
    let plan = lock_mutex(&model.observed_provenance)
        .last()
        .ok_or("plan")?
        .0
        .clone();
    let metadata = plan.request().metadata();
    let foreign_run = RunId::try_from(uuid::Uuid::from_u128(0xE8C_FFFF))?;
    let foreign_request = plan.request().for_round(RequestMetadata::for_turn(
        foreign_run,
        metadata.turn_id().ok_or("turn")?,
        metadata.attempt_id(),
        metadata.created_at(),
    ))?;
    assert_ne!(foreign_request.identity(), plan.request().identity());
    assert_eq!(
        foreign_request.model_input_identity(),
        plan.request().model_input_identity()
    );
    for mismatch in ["plan", "cache", "coverage", "run", "run_only"] {
        let changed = FamilyArtifactProducerFacts::new(
            if mismatch == "run" {
                foreign_request.identity()
            } else {
                facts.request()
            },
            facts.model_input(),
            if mismatch == "plan" {
                foreign(DigestDomain::RequestPlan, b"foreign plan")?
            } else {
                facts.plan()
            },
            if mismatch == "cache" {
                Some(foreign(DigestDomain::CacheAffinity, b"foreign cache")?)
            } else {
                facts.cache_affinity()
            },
            if matches!(mismatch, "run" | "run_only") {
                foreign_run
            } else {
                facts.run()
            },
            facts.root(),
            facts.attempt(),
            facts.source_projection(),
            facts.source_projection_items(),
            facts.output_items(),
            if mismatch == "coverage" {
                foreign(DigestDomain::CanonicalItemPrefix, b"foreign coverage")?
            } else {
                facts.covered_projection()
            },
            facts.covered_projection_items(),
            facts.physical_operation(),
            facts.terminal_operation(),
        )?;
        let altered = ModelResponse::new(
            response.output().to_vec(),
            response.finish().clone(),
            response.usage(),
            response.response_id().map(str::to_owned),
            changed.plan(),
        )
        .with_metadata(response.metadata().clone())
        .with_family_artifact_candidate(SealedFamilyArtifactCandidate::new(
            candidate.family(),
            candidate.schema_revision(),
            candidate.codec_revision(),
            candidate.binding(),
            changed,
            candidate.bytes().to_vec(),
        )?)?;
        if mismatch == "run_only" {
            let covered = plan
                .request()
                .input()
                .staged_canonical_item_prefix_identity(altered.output())?;
            assert_eq!(
                altered
                    .family_artifact_candidate()
                    .ok_or("candidate")?
                    .producer()
                    .request(),
                facts.request()
            );
            assert!(
                altered
                    .family_artifact_candidate()
                    .ok_or("candidate")?
                    .validate_for_commit(&plan, covered, altered.output().len())
                    .is_err(),
                "live accepted run-only drift"
            );
            continue;
        }
        let records = checkpoint
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .map(|record| -> TestResult<_> {
                let mut transitions = record.transitions().to_vec();
                if record.operation_id() == provider.operation_id() {
                    let last = transitions.last_mut().ok_or("transition")?;
                    *last = DurableOperationTransition::new(
                        last.checkpoint_id(),
                        DurableOperationState::Completed(
                            crate::runtime::operation_record::test_support::outcome_component(Ok(
                                &altered,
                            ))?,
                        ),
                    );
                } else if matches!(record.kind(), DurableOperationKind::Command { .. })
                    && let DurableOperationState::Completed(value) = record.state()
                {
                    let pending = decode_pending_terminal_record(value.bytes()).ok_or("pending")?;
                    let evidence = crate::runtime::terminal::completed_terminal_evidence(
                        &CommandId::new(&pending.command_id)?,
                        pending.turn_id,
                        pending.operation_id,
                        &altered,
                        pending.usage,
                    )?;
                    let last = transitions.last_mut().ok_or("transition")?;
                    *last = DurableOperationTransition::new(
                        last.checkpoint_id(),
                        DurableOperationState::Completed(evidence),
                    );
                }
                Ok(DurableOperationRecord::new(
                    record.operation_id(),
                    record.kind().clone(),
                    record.request().clone(),
                    transitions,
                )?)
            })
            .collect::<TestResult<Vec<_>>>()?;
        let poisoned = checkpoint_with_code_cell_records(
            checkpoint,
            metadata.turn_id().ok_or("turn")?,
            records,
        )?;
        let error = local_terminal_recovery_fold(&poisoned)
            .err()
            .ok_or("foreign artifact accepted")?;
        assert_eq!(error.kind(), AgentErrorKind::Session, "foreign {mismatch}");
        assert_eq!(error.store_error(), Some(SessionStoreError::corrupt()));
    }
    Ok(())
}

#[tokio::test]
async fn finalization_artifact_refuses_foreign_plan_cache_coverage_and_run() -> TestResult {
    let fixture = ArtifactFixture::new("responses", 2).await?;
    let cuts = fixture.cuts();
    assert_eq!(cuts.len(), 3);
    for cut in cuts {
        assert_foreign_artifact_refusals(cut.checkpoint(), &fixture.model)?;
    }
    Ok(())
}

#[tokio::test]
async fn finalization_artifact_sealed_prior_unsealed_final_clears_like_live() -> TestResult {
    artifact_finalization_parity("responses", 1).await
}

#[tokio::test]
async fn finalization_artifact_live_driver_rejects_run_only_drift() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let mut model = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        1,
        0xE8D_000,
        false,
    );
    model.tamper_run = true;
    let model = Arc::new(model);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:run-only-drift",
            durable_artifact_test_builder(workspace, model),
        )
        .await?;
    let result = agent.prompt("reject foreign run").await?.result().await?;
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return Err("live accepted run-only drift".into());
    };
    assert_eq!(error.kind(), AgentErrorKind::Model);
    assert!(agent.checkpoint().manifest().family_artifact().is_none());
    agent.shutdown().await?;
    Ok(())
}
