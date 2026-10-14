//! Every runtime transition is one commit, except a provider-ended turn,
//! which is exactly its decision commit and one consequence commit (spec 01
//! §3.5). Each accepted commit is a crash cut: a host lost right after it
//! leaves exactly that head, so no head may hold half a decision.

use super::*;
use crate::feed::{FeedFact, FeedOperationState, FeedTurnOutcome};
use crate::runtime::operation_record::{RoundDecision, operation_turn_id, provider_outcome};
use crate::runtime::terminal::read_terminal_result_record;
use plakous_model::{ResponseStarted, UsageUpdate};
use plakous_test_support::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep};

/// Why `checkpoint` holds half a transition, if it does.
fn half_transition(checkpoint: &RuntimeCheckpoint) -> Option<String> {
    let operations = checkpoint.manifest().ledger().operations();
    let finalized_roots = operations
        .iter()
        .filter_map(|record| match (record.kind(), record.state()) {
            (
                DurableOperationKind::Command {
                    turn_id: Some(turn_id),
                },
                DurableOperationState::Finalized(evidence),
            ) if read_terminal_result_record(evidence.bytes()).is_ok() => Some(*turn_id),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for record in &operations {
        let id = record.operation_id();
        let settled_unfinalized = matches!(
            record.state(),
            DurableOperationState::Completed(_) | DurableOperationState::ProvenNotDispatched(_)
        );
        match record.kind() {
            DurableOperationKind::Command { turn_id: Some(_) } if settled_unfinalized => {
                return Some(format!("command {id} settled without its finalization"));
            }
            DurableOperationKind::EffectDecision { .. } if settled_unfinalized => {
                return Some(format!(
                    "effect decision {id} settled without its finalization"
                ));
            }
            DurableOperationKind::Provider {
                root: ModelOperationRoot::Turn(turn_id),
                ..
            } if settled_unfinalized => {
                let decision = provider_outcome(record)
                    .ok()
                    .flatten()
                    .map(|outcome| outcome.decision);
                match decision {
                    Some(
                        RoundDecision::Continue
                        | RoundDecision::RetryOutputLimit
                        | RoundDecision::RetryContentFilter,
                    ) => {
                        return Some(format!(
                            "provider {id} decision {decision:?} without its consequence"
                        ));
                    }
                    Some(RoundDecision::Terminal | RoundDecision::FailTurn(_))
                        if finalized_roots.contains(turn_id) =>
                    {
                        return Some(format!(
                            "provider {id} left unfinalized by its turn's consequence"
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if settled_unfinalized
            && operation_turn_id(record).is_some_and(|turn_id| finalized_roots.contains(&turn_id))
        {
            return Some(format!(
                "operation {id} left unfinalized by its turn's consequence"
            ));
        }
    }
    None
}

fn accepted(store: &TestCreateStore) -> Vec<(Arc<RuntimeCheckpoint>, Vec<FeedFact>)> {
    lock_mutex(&store.accepted_commits).clone()
}

fn assert_whole(commits: &[(Arc<RuntimeCheckpoint>, Vec<FeedFact>)]) -> TestResult {
    for (index, (checkpoint, _)) in commits.iter().enumerate() {
        if let Some(violation) = half_transition(checkpoint) {
            return Err(format!("accepted commit {index}: {violation}").into());
        }
    }
    Ok(())
}

/// The states `record` reached in the commit of `checkpoint_id`.
fn states_at(record: &DurableOperationRecord, checkpoint_id: CheckpointId) -> Vec<&'static str> {
    record
        .transitions()
        .iter()
        .filter(|transition| transition.checkpoint_id() == checkpoint_id)
        .map(|transition| match transition.state() {
            DurableOperationState::Completed(_) => "completed",
            DurableOperationState::ProvenNotDispatched(_) => "proven-not-dispatched",
            DurableOperationState::Finalized(_) => "finalized",
            DurableOperationState::Dispatched => "dispatched",
            _ => "other",
        })
        .collect()
}

fn round_providers(checkpoint: &RuntimeCheckpoint, turn_id: TurnId) -> Vec<DurableOperationRecord> {
    let mut records = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(
                record.kind(),
                DurableOperationKind::Provider {
                    root: ModelOperationRoot::Turn(turn),
                    ..
                } if *turn == turn_id
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    records.sort_by_key(DurableOperationRecord::operation_id);
    records
}

fn failed_attempt(error: ModelError) -> TestResult<ModelScenario> {
    Ok(ModelScenario::new(
        ExpectedRequest::Any,
        ScenarioStart::Immediate,
        vec![
            ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                None,
                Digest::sha256(DigestDomain::RequestPlan, 1, &[b"rebound"])?,
            ))),
            ScenarioStep::Emit(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                3, 1, 0, 0, 0,
            )))),
            ScenarioStep::Error(error),
        ],
    ))
}

fn text_answer(entry: u128) -> TestResult<ModelScenario> {
    let response = ModelResponse::new(
        vec![ConversationItem::new(
            EntryId::try_from(uuid::Uuid::from_u128(entry))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("answer", None)?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        Some(Usage::new(5, 2, 0, 0, 0)),
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"rebound"])?,
    );
    Ok(ModelScenario::new(
        ExpectedRequest::Any,
        ScenarioStart::Immediate,
        vec![ScenarioStep::Emit(ModelEvent::ResponseCompleted(response))],
    ))
}

fn content_filtered() -> ModelError {
    ModelError::new(
        ModelErrorKind::ContentFilteredIncomplete,
        "stream disconnected before completion: Incomplete response returned, reason: content_filter",
    )
}

struct Scripted {
    model: Arc<plakous_test_support::ScriptedModel>,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    _directory: tempfile::TempDir,
    workspace: Arc<WorkspacePolicy>,
}

impl Scripted {
    fn new(scenarios: Vec<ModelScenario>) -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let descriptor = durable_test_descriptor("test.route")?;
        let authority = artifact_authority(&descriptor, 1)?;
        let model = Arc::new(
            plakous_test_support::ScriptedModel::new(descriptor.clone(), scenarios)
                .with_runtime_authority(authority.clone())
                .with_context_observation(CountObservation::new(
                    "agent-scripted-counter",
                    "agent-scripted-exact-v1",
                    64,
                )?)
                .with_content_filter_instruction("Offer a permitted alternative.".to_owned()),
        );
        Ok(Self {
            model,
            descriptor,
            authority,
            _directory: directory,
            workspace,
        })
    }

    fn builder(&self) -> TestResult<AgentBuilder> {
        let registry = ToolRegistryBuilder::default().freeze();
        let configured = configured_test_model(
            self.model.clone(),
            self.descriptor.clone(),
            self.authority.clone(),
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
        )?;
        Ok(
            AgentBuilder::new(configured, registry, Arc::clone(&self.workspace))
                .request_policy(AgentRequestPolicy::new(
                    GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
                    ReasoningPolicy::Disabled,
                ))
                .identity_sources(
                    Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
                    Arc::new(SystemIdGenerator),
                ),
        )
    }
}

fn sessions(store: &Arc<TestCreateStore>) -> TestResult<DurableSessionRuntime> {
    Ok(DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?)
}

/// The two commits a provider-ended turn makes, as indices into
/// `commits`: the decision commit and the consequence commit after it.
fn decision_and_consequence(
    commits: &[(Arc<RuntimeCheckpoint>, Vec<FeedFact>)],
    turn_id: TurnId,
) -> TestResult<(usize, usize)> {
    let terminal = commits
        .iter()
        .position(|(checkpoint, _)| {
            matches!(
                checkpoint.manifest().lifecycle(),
                RuntimeLifecycle::Quiescent
            ) && round_providers(checkpoint, turn_id)
                .iter()
                .all(|record| matches!(record.state(), DurableOperationState::Finalized(_)))
                && !round_providers(checkpoint, turn_id).is_empty()
        })
        .ok_or("no consequence commit closed the turn")?;
    let decision = terminal
        .checked_sub(1)
        .ok_or("consequence without a decision")?;
    Ok((decision, terminal))
}

/// A retried-then-answered turn: each retry is one commit finalizing the
/// failed attempt with its guidance or note, the answer is its decision
/// commit plus exactly one consequence commit, the head ends quiescent with
/// every same-turn record finalized, and the next prompt on the same session
/// completes (it used to end `RecoveryRequired` because the failed attempts
/// stayed `Completed` and the head stayed working).
#[tokio::test]
async fn a_retried_then_answered_turn_commits_whole_transitions_and_admits_the_next_prompt()
-> TestResult {
    let scripted = Scripted::new(vec![
        failed_attempt(content_filtered())?,
        failed_attempt(ModelError::new(
            ModelErrorKind::OutputLimit,
            "truncated mid call",
        ))?,
        text_answer(0xA3_0001)?,
        text_answer(0xA3_0002)?,
    ])?;
    let store = Arc::new(TestCreateStore::default());
    let agent = sessions(&store)?
        .create("create:a3-retried", scripted.builder()?)
        .await?;
    let base = accepted(&store).len();
    let turn = agent.prompt("retried").await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let head = result.checkpoint();
    assert!(matches!(
        head.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
    assert!(
        head.manifest()
            .ledger()
            .operations()
            .into_iter()
            .all(|record| matches!(record.state(), DurableOperationState::Finalized(_))),
        "the consequence commit finalizes every same-turn record"
    );
    let commits = accepted(&store)[base..].to_vec();
    assert_whole(&commits)?;
    let providers = round_providers(head, turn_id);
    assert_eq!(providers.len(), 3);
    // Each retry finalized its attempt in the commit that recorded it.
    for retry in &providers[..2] {
        let (completed_at, _) = commits
            .iter()
            .find(|(checkpoint, _)| {
                !states_at(retry, checkpoint.id()).is_empty()
                    && states_at(retry, checkpoint.id()).contains(&"completed")
            })
            .ok_or("retry decision commit")?;
        assert_eq!(
            states_at(retry, completed_at.id()),
            ["completed", "finalized"],
            "a retry is one commit"
        );
    }
    let (decision, consequence) = decision_and_consequence(&commits, turn_id)?;
    let answer = &providers[2];
    assert_eq!(states_at(answer, commits[decision].0.id()), ["completed"]);
    assert_eq!(
        states_at(answer, commits[consequence].0.id()),
        ["finalized"]
    );
    assert_eq!(
        consequence,
        commits.len() - 1,
        "the consequence is the turn's last commit"
    );
    // Feed: the decision commit yields only the provider's advance; the
    // consequence commit yields the turn terminal.
    assert_eq!(
        commits[decision].1,
        [FeedFact::OperationAdvanced {
            operation_id: answer.operation_id(),
            turn_id: Some(turn_id),
            kind: crate::feed::FeedOperationKind::ProviderAttempt {
                attempt_id: match answer.kind() {
                    DurableOperationKind::Provider { attempt_id, .. } => *attempt_id,
                    _ => return Err("provider kind".into()),
                },
            },
            state: FeedOperationState::Completed,
        }]
    );
    let facts = &commits[consequence].1;
    assert!(facts.iter().any(|fact| matches!(
        fact,
        FeedFact::TurnTerminal { turn_id: terminal, outcome: FeedTurnOutcome::Completed { .. }, .. }
            if *terminal == turn_id
    )));
    assert!(
        facts
            .iter()
            .any(|fact| matches!(fact, FeedFact::HistoryAppended { .. }))
    );
    assert!(matches!(facts.last(), Some(FeedFact::TurnTerminal { .. })));

    let next = agent.prompt("again").await?;
    let next = next.result().await?;
    assert!(
        matches!(next.outcome(), TurnResultOutcome::Completed(_)),
        "the next prompt after a retried turn completes: {:?}",
        next.outcome()
    );
    assert_eq!(scripted.model.remaining_scenarios(), 0);
    assert_whole(&accepted(&store)[base..])?;
    agent.shutdown().await?;
    Ok(())
}

/// A failed turn is its `FailTurn` decision commit and one consequence
/// commit holding the root's terminal, the failure note and every
/// finalization; its feed yields the failed turn terminal from that commit.
#[tokio::test]
async fn a_failed_turn_is_its_decision_then_one_consequence_commit() -> TestResult {
    let scripted = Scripted::new(vec![failed_attempt(ModelError::new(
        ModelErrorKind::Protocol,
        "stream broke",
    ))?])?;
    let store = Arc::new(TestCreateStore::default());
    let agent = sessions(&store)?
        .create("create:a3-failed", scripted.builder()?)
        .await?;
    let base = accepted(&store).len();
    let turn = agent.prompt("fails").await?;
    let turn_id = turn.id();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Failed(_)));
    let commits = accepted(&store)[base..].to_vec();
    assert_whole(&commits)?;
    let (decision, consequence) = decision_and_consequence(&commits, turn_id)?;
    assert_eq!(consequence, commits.len() - 1);
    let provider = &round_providers(result.checkpoint(), turn_id)[0];
    assert_eq!(states_at(provider, commits[decision].0.id()), ["completed"]);
    let consequence_head = &commits[consequence].0;
    assert_eq!(states_at(provider, consequence_head.id()), ["finalized"]);
    assert_eq!(
        consequence_head
            .manifest()
            .canonical()
            .history()
            .segments()
            .len(),
        commits[decision]
            .0
            .manifest()
            .canonical()
            .history()
            .segments()
            .len()
            + 1,
        "the failure note commits with the terminal"
    );
    assert!(commits[consequence].1.iter().any(|fact| matches!(
        fact,
        FeedFact::TurnTerminal {
            outcome: FeedTurnOutcome::Failed { .. },
            ..
        }
    )));
    agent.shutdown().await?;
    Ok(())
}

/// A host lost right after any commit of a retried-then-answered turn leaves
/// a whole transition, and the cut between the decision commit and the
/// consequence commit leaves exactly the recorded decision with no
/// consequence. Reopening never redispatches the answered attempt.
#[tokio::test]
async fn a_cut_after_each_commit_of_a_turn_leaves_a_whole_transition() -> TestResult {
    let scenarios = || -> TestResult<Vec<ModelScenario>> {
        Ok(vec![
            failed_attempt(content_filtered())?,
            failed_attempt(ModelError::new(
                ModelErrorKind::OutputLimit,
                "truncated mid call",
            ))?,
            text_answer(0xA3_0101)?,
        ])
    };
    let uncut = {
        let scripted = Scripted::new(scenarios()?)?;
        let store = Arc::new(TestCreateStore::default());
        let agent = sessions(&store)?
            .create("create:a3-cut", scripted.builder()?)
            .await?;
        let base = accepted(&store).len();
        let turn = agent.prompt("cut").await?;
        let turn_id = turn.id();
        turn.result().await?;
        agent.shutdown().await?;
        let commits = accepted(&store)[base..].to_vec();
        let (decision, _) = decision_and_consequence(&commits, turn_id)?;
        (base, commits.len(), decision)
    };
    let (base, count, decision) = uncut;
    for cut in 1..=count {
        let scripted = Scripted::new(scenarios()?)?;
        let store = Arc::new(TestCreateStore::default());
        store
            .halt_after_accepted_commit
            .store(base + cut, Ordering::Release);
        let sessions = sessions(&store)?;
        let original = sessions
            .create("create:a3-cut", scripted.builder()?)
            .await?;
        let turn = original.prompt("cut").await?;
        let turn_id = turn.id();
        let _ = tokio::time::timeout(Duration::from_secs(5), turn.result()).await?;
        let commits = accepted(&store);
        assert_eq!(commits.len(), base + cut, "cut {cut} stopped at its commit");
        let (head, _) = commits.last().ok_or("cut head")?;
        if let Some(violation) = half_transition(head) {
            return Err(format!("cut {cut}: {violation}").into());
        }
        if cut == decision + 1 {
            // Decision recorded, consequence absent.
            let providers = round_providers(head, turn_id);
            let answer = providers.last().ok_or("answer provider")?;
            assert!(matches!(
                provider_outcome(answer)?.map(|outcome| outcome.decision),
                Some(RoundDecision::Terminal)
            ));
            assert!(matches!(
                answer.state(),
                DurableOperationState::Completed(_)
            ));
            assert!(matches!(
                head.manifest().lifecycle(),
                RuntimeLifecycle::Working { .. }
            ));
        }
        let address =
            DurableSessionAddress::new(original.session_id(), original.branch_id(), head.id());
        original.abort_driver_for_test();
        tokio::task::yield_now().await;
        store.resume_commits();
        store.expire_active_lease();
        let dispatched = scripted.model.captured_plans().len();
        let DurableRuntimeOpen::Opened(reopened) = sessions
            .open("open:a3-cut", address, scripted.builder()?)
            .await?
        else {
            return Err(format!("cut {cut} did not reopen").into());
        };
        assert_eq!(
            scripted.model.captured_plans().len(),
            dispatched,
            "cut {cut}: reopen must not redispatch"
        );
        let after = accepted(&store);
        let terminals = after[base..]
            .iter()
            .flat_map(|(_, facts)| facts)
            .filter_map(|fact| match fact {
                FeedFact::TurnTerminal {
                    turn_id: bound,
                    usage,
                    ..
                } if *bound == turn_id => Some(usage),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1, "cut {cut}: exactly one terminal");
        let records = reopened.checkpoint();
        let terminal = crate::runtime::feed_views::checkpoint_turn_terminal(&records, turn_id)?
            .ok_or("recovered terminal")?;
        assert_eq!(
            terminal.1.as_ref(),
            reopened.abandoned_turn(),
            "cut {cut}: report read without a parent"
        );
        let providers = round_providers(&records, turn_id);
        let mut expected = TurnUsage::default();
        for provider in &providers {
            let outcome = provider_outcome(provider)?;
            if outcome.is_some()
                || provider.transitions().iter().any(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
            {
                expected.record_model_operation(outcome.and_then(|outcome| outcome.usage))?;
            }
        }
        assert_eq!(
            *terminals[0], expected,
            "cut {cut}: terminal usage is the ledger sum"
        );
        assert_whole(&after[base + cut..])?;
        reopened.shutdown().await?;
    }
    Ok(())
}

/// Steering applied at a safe boundary is one commit: each steer's
/// finalization with its `Steering` segment.
#[tokio::test]
async fn applied_steering_is_one_commit_with_its_segment() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            0xA3_0200,
            false,
        )
        .with_tool_first()
        .with_second_direct_tool()?,
    );
    let release = Arc::new(Notify::new());
    *lock_mutex(&model.held_call_release) = Some(release.clone());
    model.held_call.store(0, Ordering::Release);
    let (registry, _tool) = session_update_registry(SessionUpdateFixtureMode::Active)?;
    let store = Arc::new(TestCreateStore::default());
    let agent = sessions(&store)?
        .create(
            "create:a3-steer",
            session_update_builder(workspace, model.clone(), registry)?,
        )
        .await?;
    let base = accepted(&store).len();
    let turn = agent.prompt("start processes").await?;
    tokio::time::timeout(Duration::from_secs(2), model.held_call_started.notified()).await?;
    turn.control().steer("new instruction").await?;
    release.notify_one();
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    let commits = accepted(&store)[base..].to_vec();
    assert_whole(&commits)?;
    let steering_commits = commits
        .iter()
        .filter(|(_, facts)| {
            facts.iter().any(|fact| {
                matches!(
                    fact,
                    FeedFact::HistoryAppended {
                        segment: HistorySegmentKind::Steering,
                        ..
                    }
                )
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(steering_commits.len(), 1);
    let (checkpoint, _) = steering_commits[0];
    let finalized_steers = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|record| {
            matches!(record.kind(), DurableOperationKind::Command { .. })
                && states_at(record, checkpoint.id()) == ["completed", "finalized"]
        })
        .count();
    assert_eq!(
        finalized_steers, 1,
        "the steer is applied and finalized with its segment"
    );
    agent.shutdown().await?;
    Ok(())
}

async fn dispatch_rejected_fail_turn_reopens(refused: bool) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let model = artifact_test_model(0, 0xAF_0001)?;
    model.reject_round_dispatch.store(true, Ordering::Release);
    let builder = || durable_artifact_test_builder(workspace.clone(), model.clone());
    let cut_count = {
        let store = Arc::new(TestCreateStore::default());
        let agent = sessions(&store)?
            .create("create:dispatch-rejected-uncut", builder())
            .await?;
        let base = accepted(&store).len();
        let turn = agent.prompt("rejected before dispatch").await?;
        let turn_id = turn.id();
        turn.result().await?;
        let commits = accepted(&store);
        let (decision, _) = decision_and_consequence(&commits[base..], turn_id)?;
        agent.shutdown().await?;
        base + decision + 1
    };
    let store = Arc::new(TestCreateStore::default());
    let sessions = sessions(&store)?;
    let original = sessions
        .create("create:dispatch-rejected", builder())
        .await?;
    if refused {
        store.fail_quiescent_terminal();
    } else {
        store
            .halt_after_accepted_commit
            .store(cut_count, Ordering::Release);
    }
    let turn = original.prompt("rejected before dispatch").await?;
    let turn_id = turn.id();
    if refused {
        let result = turn.result().await?;
        assert!(matches!(
            result.outcome(),
            TurnResultOutcome::RecoveryRequired(_)
        ));
    } else {
        let _ = tokio::time::timeout(Duration::from_secs(5), turn.result()).await?;
    }
    let head = original.checkpoint();
    let providers = round_providers(&head, turn_id);
    let provider = providers.last().ok_or("provider")?;
    assert!(matches!(
        provider.state(),
        DurableOperationState::ProvenNotDispatched(_)
    ));
    assert!(matches!(
        provider_outcome(provider)?.map(|outcome| outcome.decision),
        Some(RoundDecision::FailTurn(_))
    ));
    let address =
        DurableSessionAddress::new(original.session_id(), original.branch_id(), head.id());
    original.abort_driver_for_test();
    tokio::task::yield_now().await;
    store.resume_commits();
    store.expire_active_lease();
    let starts = model.calls.load(Ordering::Acquire);
    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:dispatch-rejected", address, builder())
        .await?
    else {
        return Err("dispatch-rejected decision did not reopen".into());
    };
    assert!(reopened.abandoned_turn().is_none());
    assert_eq!(model.calls.load(Ordering::Acquire), starts);
    let terminals = accepted(&store).into_iter().flat_map(|(_, facts)| facts).filter(|fact| matches!(fact,
        FeedFact::TurnTerminal { turn_id: bound, outcome: FeedTurnOutcome::Failed { .. }, .. } if *bound == turn_id
    )).count();
    assert_eq!(terminals, 1);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn dispatch_rejected_fail_turn_cut_reopens_without_redispatch() -> TestResult {
    dispatch_rejected_fail_turn_reopens(false).await
}

#[tokio::test]
async fn dispatch_rejected_fail_turn_refused_consequence_reopens_without_redispatch() -> TestResult
{
    dispatch_rejected_fail_turn_reopens(true).await
}
