//! Long closed-cell turns exercise retirement through the real interpreter.

use super::*;
use plakous_agent::internal::SessionStore;
use plakous_agent::{AgentConfig, CommandAckDisposition, CommandId, WeakAgent};
use plakous_tools::{ToolRuntimeError, ToolRuntimeErrorKind};

const CELLS: usize = 100;
const LEAVES: usize = 50;

#[derive(Clone, Copy)]
struct Sample {
    operations: usize,
    /// Local tool/evaluator outcomes, excluding request-bearing baton frames.
    closed_local_bytes: usize,
}

struct SamplingProbe {
    agent: Mutex<Option<WeakAgent>>,
    calls: AtomicUsize,
    samples: Mutex<Vec<Sample>>,
}

impl plakous_tools::ToolExecutor for SamplingProbe {
    fn execute(
        self: Arc<Self>,
        _invocation: plakous_tools::ToolInvocation,
        _context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let failure = || {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "retirement fixture could not inspect its checkpoint",
                )
            };
            let agent = test_lock(&self.agent)
                .as_ref()
                .and_then(WeakAgent::upgrade)
                .ok_or_else(failure)?;
            let checkpoint = agent_checkpoint(&agent);
            let ledger = checkpoint.manifest().ledger();
            let records = ledger.operations();
            let closed_local_bytes = records
                .iter()
                .filter(|record| {
                    matches!(
                        record.kind(),
                        DurableOperationKind::Effect { .. }
                            | DurableOperationKind::CompositionEvaluation { .. }
                            | DurableOperationKind::CompositionContinuation { .. }
                            | DurableOperationKind::CompositionChild { .. }
                    ) && matches!(record.state(), DurableOperationState::Finalized(_))
                })
                .map(|record| match record.state() {
                    DurableOperationState::Finalized(evidence) => evidence.bytes().len(),
                    _ => 0,
                })
                .sum();
            test_lock(&self.samples).push(Sample {
                operations: records.len(),
                closed_local_bytes,
            });
            ToolOutcome::success_text("ok")
        })
    }
}

struct ClosedCellsModel {
    scripted: Arc<ScriptedModel>,
    cells: AtomicUsize,
    calls: AtomicUsize,
    cell_limit: usize,
    refusal_fixture: bool,
    outer_calls: AtomicUsize,
    boundary_sent: AtomicUsize,
}

impl LanguageModel for ClosedCellsModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        self.scripted.clone().revalidate_runtime_authority(at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let inert = self.scripted.clone().prepare_call(plan.clone(), context)?;
        Ok(PreparedModelCall::new(inert.record().clone(), move |_| {
            Box::pin(async move {
                let response = self.response(&plan).map_err(|error| {
                    ModelError::new(ModelErrorKind::InvalidEvent, error.to_string())
                })?;
                let item = &response.output()[0];
                let events = vec![
                    Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                        None,
                        plan.digest(),
                    ))),
                    Ok(ModelEvent::BlockStarted(BlockStarted::new(
                        item.id(),
                        OutputLocation::new(0, 0),
                        BlockStart::Complete(item.blocks()[0].clone()),
                    ))),
                    Ok(ModelEvent::ResponseCompleted(response)),
                ];
                Ok(Box::pin(FixtureStream {
                    events: events.into(),
                }) as ModelStream)
            })
        }))
    }
}

impl ClosedCellsModel {
    fn response(&self, plan: &ValidatedRequestPlan) -> TestResult<ModelResponse> {
        let round = self.calls.fetch_add(1, Ordering::AcqRel);
        let seed = u128::try_from(round)? * 2 + 0x8000;
        let latest_output = plan
            .request()
            .input()
            .items()
            .iter()
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| {
                let ContentBlock::ToolResult(result) = block else {
                    return None;
                };
                Some(
                    result
                        .parts()
                        .iter()
                        .filter_map(|part| match part {
                            plakous_model::ToolResultPart::Text(text) => Some(text.text()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            })
            .last();
        let latest = latest_output.as_deref();
        let boundary = self.refusal_fixture
            && self.cells.load(Ordering::Acquire) == 1
            && latest.is_some_and(|text| text.starts_with("Script completed"))
            && self
                .boundary_sent
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        let call = if boundary {
            None
        } else if let Some(cell) = latest
            .and_then(|text| text.strip_prefix("Script running with cell ID "))
            .and_then(|text| text.lines().next())
        {
            Some((
                "wait_js",
                serde_json::json!({"cell_id":cell,"yield_time_ms":600_000}),
            ))
        } else if self.cells.load(Ordering::Acquire) < self.cell_limit {
            if let Some(text) = latest {
                assert!(
                    text.starts_with("Script completed"),
                    "cell did not close: {text}"
                );
                if self.refusal_fixture {
                    assert!(text.contains("headroom refusals:"), "{text}");
                }
            }
            self.cells.fetch_add(1, Ordering::AcqRel);
            if self.cells.load(Ordering::Acquire).is_multiple_of(10) {
                eprintln!(
                    "closed-cell fixture: opened {} of {} cells",
                    self.cells.load(Ordering::Acquire),
                    self.cell_limit
                );
            }
            Some((
                "exec_js",
                serde_json::json!({
                    "source": closed_cell_source(self.refusal_fixture, self.cells.load(Ordering::Acquire)),
                    "yield_time_ms":10_000,
                }),
            ))
        } else {
            assert!(latest.is_some_and(|text| text.starts_with("Script completed")));
            if self.refusal_fixture {
                assert!(latest.is_some_and(|text| text.contains("later cell succeeded")));
            }
            None
        };
        let (block, finish) = if let Some((name, arguments)) = call {
            self.outer_calls.fetch_add(1, Ordering::AcqRel);
            (
                ContentBlock::ToolCall(ToolCallBlock::new(
                    CallId::try_from(Uuid::from_u128(seed))?,
                    ToolName::new(name)?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    ToolCorrelation::default(),
                    serde_json::to_string(&arguments)?,
                )?),
                FinishReason::ToolCalls,
            )
        } else if boundary {
            (
                ContentBlock::Text(TextBlock::new("continue", None)?),
                FinishReason::AssistantContinuation,
            )
        } else {
            (
                ContentBlock::Text(TextBlock::new("done", None)?),
                FinishReason::Stop,
            )
        };
        let item = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(seed + 1))?,
            Actor::Assistant,
            vec![block],
            Provenance::ModelOutput,
        )?;
        Ok(ModelResponse::new(
            vec![item],
            FinishDetails::new(finish, None),
            None,
            None,
            plan.digest(),
        ))
    }
}

fn closed_cell_source(refusal_fixture: bool, cell: usize) -> String {
    if refusal_fixture {
        return if cell == 1 {
            "let refused = 0; for (let i = 0; i < 100; i++) { try { await tools.probe({}); } catch (error) { if (!JSON.stringify(error).includes('nested call not admitted: runtime operation headroom is low; finish this cell and continue in a new call')) throw error; refused++; } } if (refused === 0) throw Error('headroom did not refuse'); text('headroom refusals: ' + refused);".into()
        } else {
            "await tools.probe({}); text('later cell succeeded');".into()
        };
    }
    format!(
        "for (let i = 0; i < {LEAVES}; i += 16) {{ await Promise.all(Array.from({{length: Math.min(16, {LEAVES} - i)}}, () => tools.probe({{}}))); }} text('closed');"
    )
}

async fn closed_cells_case(sqlite: bool, cells: usize) -> TestResult {
    let directory = tempfile::tempdir()?;
    let worker = std::env::var_os("PLAK_JS_HOST")
        .map(PathBuf::from)
        .ok_or("set PLAK_JS_HOST to the test build's plakous-js-host")?;
    let store: Arc<dyn SessionStore> = if sqlite {
        Arc::new(SqliteSessionStore::open(
            directory.path().join("session.sqlite3"),
        )?)
    } else {
        Arc::new(MemorySessionStore::with_clock(
            MemoryStoreLimits::default(),
            Arc::new(FixedLeaseClock::new(timestamp()?)),
        )?)
    };
    let probe = Arc::new(SamplingProbe {
        agent: Mutex::new(None),
        calls: AtomicUsize::new(0),
        samples: Mutex::new(Vec::new()),
    });
    let registry = registry(Engine::JavaScript, probe.clone())?;
    let config = AgentConfig::default().with_max_model_rounds(1_024);
    let durability = if sqlite {
        LocalDurabilityDisposition::DurableRequired
    } else {
        LocalDurabilityDisposition::Ephemeral
    };
    let authority = RuntimeModelAuthority::new(
        &descriptor()?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"real-cell-crash"])?,
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"real-cell-crash-credential"],
        )?,
        1,
        timestamp()?,
        None,
    )?;
    let model = Arc::new(ClosedCellsModel {
        scripted: Arc::new(
            ScriptedModel::new(descriptor()?, [])
                .with_runtime_authority(authority.clone())
                .with_context_observation(crate::test_model::context_observation()?),
        ),
        cells: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        cell_limit: cells,
        refusal_fixture: std::env::var_os("PLAK_TEST_LEDGER_REFUSAL_CHILD").is_some(),
        outer_calls: AtomicUsize::new(0),
        boundary_sent: AtomicUsize::new(0),
    });
    let runtime = binding_model(
        directory.path(),
        &worker,
        Engine::JavaScript,
        registry.clone(),
        config,
        durability,
        (model.clone(), authority),
    )?;
    let sessions = AgentSessions::new(SessionBackend::from_store(
        store,
        if sqlite {
            RuntimeDurability::Required
        } else {
            RuntimeDurability::Disabled
        },
    ))?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("retirement:create")?,
            runtime,
        ))
        .await?;
    *test_lock(&probe.agent) = Some(session.agent().downgrade());
    let command = CommandId::new("retirement:root")?;
    let started = session
        .agent()
        .start(command.clone(), "close every cell")
        .await?;
    let result = started.into_turn().ok_or("turn")?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    let leaves = probe.calls.load(Ordering::Acquire);
    if model.refusal_fixture {
        assert!(leaves > 1 && leaves < 100);
    } else {
        assert_eq!(leaves, cells * LEAVES);
    }
    eprintln!(
        "closed-cell fixture: {cells} evaluations, {} waits, {} leaves; {} local attempts",
        model.outer_calls.load(Ordering::Acquire) - cells,
        leaves,
        result.usage().local_tool_attempts(),
    );
    assert_eq!(
        result.usage().local_tool_attempts(),
        u64::try_from(leaves + model.outer_calls.load(Ordering::Acquire))?
    );
    assert_eq!(
        result.usage().model_operations(),
        u64::try_from(model.calls.load(Ordering::Acquire))?
    );
    assert!(model.calls.load(Ordering::Acquire) <= 1_024);
    assert_eq!(model.cells.load(Ordering::Acquire), cells);
    if model.refusal_fixture {
        assert!(
            agent_checkpoint(session.agent())
                .manifest()
                .ledger()
                .operations()
                .len()
                < leaves
        );
    } else {
        assert_samples(&probe, session.agent(), cells)?;
    }
    let duplicate = session
        .agent()
        .start(command.clone(), "close every cell")
        .await?;
    assert_eq!(
        duplicate.acknowledgement().disposition(),
        CommandAckDisposition::Duplicate
    );
    assert_eq!(
        duplicate
            .into_result()
            .ok_or("duplicate result")?
            .result()
            .await?
            .usage(),
        result.usage()
    );
    let locator = session.release().await?;
    sessions.prune_history(locator).await?;
    let (runtime, reopened_model) = binding_scenarios(
        directory.path(),
        &worker,
        Engine::JavaScript,
        registry,
        Vec::new(),
        config,
        durability,
    )?;
    let OpenSessionOutcome::Opened { session, .. } = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("retirement:reopen")?,
            locator,
            runtime,
        ))
        .await?
    else {
        return Err("reopen".into());
    };
    let duplicate = session.agent().start(command, "close every cell").await?;
    assert_eq!(
        duplicate
            .into_result()
            .ok_or("reopened result")?
            .result()
            .await?
            .usage(),
        result.usage()
    );
    assert_eq!(reopened_model.captured_plans().len(), 0);
    session.release().await?;
    Ok(())
}

fn assert_samples(probe: &SamplingProbe, agent: &Agent, cells: usize) -> TestResult {
    let samples = test_lock(&probe.samples);
    assert_eq!(samples.len(), cells * LEAVES);
    let high_water = std::env::var("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2_048);
    let frontier_bound = high_water.max(cells + 2 * LEAVES + 4) + 2 * LEAVES + 4;
    assert!(
        samples
            .iter()
            .all(|sample| sample.operations <= frontier_bound)
    );
    if cells >= CELLS {
        let early = samples[1_000..2_000]
            .iter()
            .map(|sample| sample.closed_local_bytes)
            .max()
            .ok_or("early samples")?;
        let late = samples[4_000..]
            .iter()
            .map(|sample| sample.closed_local_bytes)
            .max()
            .ok_or("late samples")?;
        assert!(
            late <= early + early / 4 + 4_096,
            "closed debt grew: {early} -> {late}"
        );
    }
    let checkpoint = agent_checkpoint(agent);
    assert!(checkpoint.manifest().ledger().operations().len() < frontier_bound);
    assert!(checkpoint.manifest().ledger().operations().len() < cells * LEAVES);
    assert_eq!(checkpoint.manifest().ledger().commands().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "long-turn evidence: just soak (also run by just full)"]
async fn ledger_retirement_5000_real_leaf_calls_in_closed_cells_memory() -> TestResult {
    closed_cells_case(false, CELLS).await
}

#[tokio::test]
#[ignore = "long-turn evidence: just soak (also run by just full)"]
async fn ledger_retirement_5000_real_leaf_calls_in_closed_cells_sqlite() -> TestResult {
    closed_cells_case(true, CELLS).await
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn ledger_retirement_low_threshold_closed_cells_reopen_both_stores() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_LOW_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::ledger_retirement_low_threshold_closed_cells_reopen_both_stores"
        )
        .strip_prefix("plakous::")
        .ok_or("test name")?;
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--nocapture"])
            .env("PLAK_TEST_LEDGER_LOW_CHILD", "1")
            .env("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER", "16")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    closed_cells_case(false, 4).await?;
    closed_cells_case(true, 4).await
}

#[cfg(feature = "test-hooks")]
#[tokio::test]
async fn ledger_retirement_nested_refusal_cell_finishes_and_next_cell_succeeds() -> TestResult {
    if std::env::var_os("PLAK_TEST_LEDGER_REFUSAL_CHILD").is_none() {
        let name = concat!(
            module_path!(),
            "::ledger_retirement_nested_refusal_cell_finishes_and_next_cell_succeeds"
        )
        .strip_prefix("plakous::")
        .ok_or("test name")?;
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--nocapture"])
            .env("PLAK_TEST_LEDGER_REFUSAL_CHILD", "1")
            .env("PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER", "8")
            .env("PLAK_TEST_LEDGER_HEADROOM_LIMIT", "80")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    }
    closed_cells_case(false, 2).await?;
    closed_cells_case(true, 2).await
}
