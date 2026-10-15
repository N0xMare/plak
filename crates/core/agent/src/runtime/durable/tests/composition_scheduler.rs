use super::*;

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(super) enum DriverSchedulerScenario {
    #[default]
    Ordinary,
    UnknownMutation,
    EdgePublicationFailure,
    DeliveryHostLost,
    DeliveryRefused,
    DeliveryPanics,
    DeliveryPanicsAfterIntegrationLoss,
    PanicBeforeSourceAdmission,
    PanicAfterSourceAdmission,
    PanicAfterCellAcknowledgement,
    ReadOnlyLeaseExpiry,
    MutatingLeaseExpiry,
}

impl DriverSchedulerScenario {
    pub(super) fn panics_during_delivery(self) -> bool {
        matches!(
            self,
            Self::DeliveryPanics | Self::DeliveryPanicsAfterIntegrationLoss
        )
    }
}

#[derive(Default)]
pub(super) struct DriverSchedulerEvidence {
    pub(super) counters: CompositionCounters,
    pub(super) scenario: DriverSchedulerScenario,
    pub(super) release_read_a: AtomicBool,
    pub(super) read_a_entries: AtomicUsize,
    pub(super) source_starts: AtomicUsize,
    pub(super) session_drops: AtomicUsize,
    pub(super) receiver_drops: AtomicUsize,
    pub(super) responder_drops: Mutex<Vec<(String, usize)>>,
    pub(super) delivery_panics: Mutex<Vec<(String, usize)>>,
    pub(super) read_a_started: AtomicBool,
    pub(super) read_b_started: AtomicBool,
    pub(super) read_b_finished: AtomicBool,
    pub(super) cap_requested: AtomicBool,
    pub(super) cap_rejected: AtomicBool,
    pub(super) invalid_rejected: AtomicBool,
    pub(super) serial_invalid_delivered: AtomicBool,
    pub(super) read_b_delivered: AtomicBool,
    pub(super) mutations_done: AtomicUsize,
    pub(super) cancel_started: AtomicBool,
    pub(super) cancel_joined: AtomicBool,
    pub(super) active_reads: AtomicUsize,
    pub(super) max_active_reads: AtomicUsize,
    pub(super) active_mutations: AtomicUsize,
    pub(super) mutation_overlap: AtomicBool,
    pub(super) deliveries: Mutex<Vec<(String, bool, String)>>,
    pub(super) read_finish_order: Mutex<Vec<String>>,
    pub(super) mutation_order: Mutex<Vec<String>>,
}

pub(super) async fn wait_for_scheduler_fact(fact: &AtomicBool) {
    while !fact.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
}

pub(super) async fn wait_for_scheduler_count(count: &AtomicUsize, expected: usize) {
    while count.load(Ordering::Acquire) < expected {
        tokio::task::yield_now().await;
    }
}

struct DriverSchedulerTool {
    evidence: Arc<DriverSchedulerEvidence>,
}

struct ActiveSchedulerRead(Arc<DriverSchedulerEvidence>);

impl Drop for ActiveSchedulerRead {
    fn drop(&mut self) {
        self.0.active_reads.fetch_sub(1, Ordering::AcqRel);
    }
}

impl plakous_tools::ToolExecutor for DriverSchedulerTool {
    fn execute(
        self: Arc<Self>,
        invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            match invocation.identity().name().as_str() {
                "read_a" => {
                    let active = self.evidence.active_reads.fetch_add(1, Ordering::AcqRel) + 1;
                    self.evidence
                        .max_active_reads
                        .fetch_max(active, Ordering::AcqRel);
                    let _active = ActiveSchedulerRead(Arc::clone(&self.evidence));
                    self.evidence.read_a_started.store(true, Ordering::Release);
                    self.evidence.read_a_entries.fetch_add(1, Ordering::AcqRel);
                    if self.evidence.scenario == DriverSchedulerScenario::EdgePublicationFailure
                        || self.evidence.scenario == DriverSchedulerScenario::DeliveryHostLost
                        || self.evidence.scenario.panics_during_delivery()
                    {
                        wait_for_scheduler_fact(&self.evidence.release_read_a).await;
                        return ToolOutcome::success_text("read-a");
                    }
                    wait_for_scheduler_fact(&self.evidence.cap_requested).await;
                    wait_for_scheduler_fact(&self.evidence.read_b_finished).await;
                    lock_mutex(&self.evidence.read_finish_order).push("A".to_owned());
                    ToolOutcome::success_text("read-a")
                }
                "read_b" => {
                    let active = self.evidence.active_reads.fetch_add(1, Ordering::AcqRel) + 1;
                    self.evidence
                        .max_active_reads
                        .fetch_max(active, Ordering::AcqRel);
                    let _active = ActiveSchedulerRead(Arc::clone(&self.evidence));
                    self.evidence.read_b_started.store(true, Ordering::Release);
                    wait_for_scheduler_fact(&self.evidence.cap_requested).await;
                    lock_mutex(&self.evidence.read_finish_order).push("B".to_owned());
                    self.evidence.read_b_finished.store(true, Ordering::Release);
                    ToolOutcome::failure_text("read-b failed")
                }
                "read_cancel" => {
                    let active = self.evidence.active_reads.fetch_add(1, Ordering::AcqRel) + 1;
                    self.evidence
                        .max_active_reads
                        .fetch_max(active, Ordering::AcqRel);
                    let _active = ActiveSchedulerRead(Arc::clone(&self.evidence));
                    self.evidence.cancel_started.store(true, Ordering::Release);
                    context.cancellation().cancelled().await;
                    self.evidence.cancel_joined.store(true, Ordering::Release);
                    Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Cancelled,
                        "scheduler cancellation probe joined",
                    ))
                }
                "mutate_one" | "mutate_two" => {
                    if self
                        .evidence
                        .active_mutations
                        .fetch_add(1, Ordering::AcqRel)
                        != 0
                    {
                        self.evidence
                            .mutation_overlap
                            .store(true, Ordering::Release);
                    }
                    lock_mutex(&self.evidence.mutation_order)
                        .push(invocation.identity().name().as_str().to_owned());
                    tokio::task::yield_now().await;
                    self.evidence
                        .active_mutations
                        .fetch_sub(1, Ordering::AcqRel);
                    self.evidence.mutations_done.fetch_add(1, Ordering::AcqRel);
                    if invocation.identity().name().as_str() == "mutate_one" {
                        if self.evidence.scenario == DriverSchedulerScenario::UnknownMutation {
                            return ToolOutcome::outcome_unknown_text("exact unknown mutation");
                        }
                        return ToolOutcome::failure_text("mutation failed after partial effects");
                    }
                    ToolOutcome::success_text(invocation.identity().name().as_str())
                }
                other => Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    format!("unexpected scheduler tool {other}"),
                )),
            }
        })
    }
}

pub(super) struct DriverSchedulerResponder {
    pub(super) identity: String,
    pub(super) evidence: Arc<DriverSchedulerEvidence>,
}

impl Drop for DriverSchedulerResponder {
    fn drop(&mut self) {
        lock_mutex(&self.evidence.responder_drops).push((
            self.identity.clone(),
            self.evidence
                .counters
                .session_shutdowns
                .load(Ordering::Acquire),
        ));
    }
}

impl crate::local_composition::LocalCompositionNestedResponder for DriverSchedulerResponder {
    fn resolve(
        self: Box<Self>,
        value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            if self.evidence.scenario.panics_during_delivery() {
                if self.identity == "A_DUPLICATE" {
                    wait_for_scheduler_count(&self.evidence.counters.integration_shutdowns, 1)
                        .await;
                }
                lock_mutex(&self.evidence.delivery_panics).push((
                    self.identity.clone(),
                    self.evidence
                        .counters
                        .integration_shutdowns
                        .load(Ordering::Acquire),
                ));
                assert!(
                    !self.evidence.scenario.panics_during_delivery(),
                    "injected nested delivery panic"
                );
            }
            lock_mutex(&self.evidence.deliveries).push((
                self.identity.clone(),
                true,
                value.to_string(),
            ));
            if self.evidence.scenario == DriverSchedulerScenario::DeliveryHostLost {
                return Ok(crate::local_composition::LocalCompositionNestedDelivery::HostLost);
            }
            if self.identity == "B" {
                self.evidence
                    .read_b_delivered
                    .store(true, Ordering::Release);
            }
            if self.identity == "INVALID_MUTATION" {
                self.evidence
                    .serial_invalid_delivered
                    .store(true, Ordering::Release);
            }
            // A batch consumer does not acknowledge A until B has also been
            // delivered. The driver must keep committing and polling B.
            if self.identity == "A" {
                wait_for_scheduler_fact(&self.evidence.read_b_delivered).await;
            }
            if self.evidence.scenario == DriverSchedulerScenario::DeliveryRefused {
                return Ok(
                    crate::local_composition::LocalCompositionNestedDelivery::Refused(
                        "cell no longer owns this request".to_owned(),
                    ),
                );
            }
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            if self
                .evidence
                .counters
                .session_shutdowns
                .load(Ordering::Acquire)
                != 0
            {
                return Ok(crate::local_composition::LocalCompositionNestedDelivery::HostLost);
            }
            if self.identity == "M1"
                && self.evidence.scenario == DriverSchedulerScenario::UnknownMutation
            {
                return std::future::pending().await;
            }
            lock_mutex(&self.evidence.deliveries).push((
                self.identity.clone(),
                false,
                reason.clone(),
            ));
            if self.identity == "B" {
                self.evidence
                    .read_b_delivered
                    .store(true, Ordering::Release);
            }
            if self.identity == "CAP" && reason.contains("limit of 2") {
                self.evidence.cap_rejected.store(true, Ordering::Release);
            }
            if self.identity == "INVALID" {
                self.evidence
                    .invalid_rejected
                    .store(true, Ordering::Release);
            }
            if self.identity == "INVALID_MUTATION" {
                self.evidence
                    .serial_invalid_delivered
                    .store(true, Ordering::Release);
            }
            // A batch consumer does not acknowledge A until B has also been
            // delivered. The driver must keep committing and polling B.
            if self.identity == "A" {
                wait_for_scheduler_fact(&self.evidence.read_b_delivered).await;
            }
            if self.evidence.scenario == DriverSchedulerScenario::DeliveryRefused {
                return Ok(
                    crate::local_composition::LocalCompositionNestedDelivery::Refused(
                        "cell no longer owns this request".to_owned(),
                    ),
                );
            }
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }
}

pub(super) struct DriverSchedulerEvents {
    pub(super) next: usize,
    pub(super) evidence: Arc<DriverSchedulerEvidence>,
}

impl DriverSchedulerEvents {
    pub(super) fn nested(&self, identity: &str, tool_name: &str) -> LocalCompositionEvent {
        self.nested_with_input(identity, tool_name, json!({}))
    }

    pub(super) fn nested_with_input(
        &self,
        identity: &str,
        tool_name: &str,
        input: serde_json::Value,
    ) -> LocalCompositionEvent {
        LocalCompositionEvent::NestedCall {
            call: crate::local_composition::LocalCompositionNestedCall {
                cell_id: "scheduler-cell".to_owned(),
                runtime_call_id: identity.to_owned(),
                tool_name: tool_name.to_owned(),
                input: Some(input),
            },
            responder: Box::new(DriverSchedulerResponder {
                identity: identity.to_owned(),
                evidence: Arc::clone(&self.evidence),
            }),
        }
    }
}

pub(super) struct DriverSchedulerSession {
    pub(super) evidence: Arc<DriverSchedulerEvidence>,
}

impl Drop for DriverSchedulerSession {
    fn drop(&mut self) {
        self.evidence.session_drops.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for DriverSchedulerEvents {
    fn drop(&mut self) {
        self.evidence.receiver_drops.fetch_add(1, Ordering::AcqRel);
    }
}

pub(super) struct DriverSchedulerIntegration {
    pub(super) descriptor: LocalCompositionDescriptor,
    pub(super) evidence: Arc<DriverSchedulerEvidence>,
}

fn driver_scheduler_registry(
    evidence: Arc<DriverSchedulerEvidence>,
) -> TestResult<FrozenToolRegistry> {
    let executor: Arc<dyn plakous_tools::ToolExecutor> = Arc::new(DriverSchedulerTool { evidence });
    let mut registry = ToolRegistryBuilder::default();
    for (name, effect, concurrency) in [
        ("read_a", ToolEffect::ReadOnly, ToolConcurrency::Parallel),
        ("read_b", ToolEffect::ReadOnly, ToolConcurrency::Parallel),
        (
            "read_cancel",
            ToolEffect::ReadOnly,
            ToolConcurrency::Parallel,
        ),
        ("mutate_one", ToolEffect::Mutating, ToolConcurrency::Serial),
        ("mutate_two", ToolEffect::Mutating, ToolConcurrency::Serial),
    ] {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "1.0.0",
            format!("Scheduler probe {name}."),
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::model_content(128, 1_048_576)?,
            concurrency,
            effect,
        )?;
        registry.register(
            ToolRegistration::new(
                RegisteredToolId::new(format!("scheduler.{name}.registration"))?,
                ToolExecutorId::new(format!("scheduler.{name}.executor"))?,
                plakous_tools::ToolExecutorBindingIdentity::new(
                    1,
                    &[format!("scheduler.{name}.executor").as_bytes()],
                )?,
                definition,
                Arc::clone(&executor),
            )
            .nested_eligible(),
        )?;
    }
    ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    Ok(registry.freeze())
}

#[test]
fn private_driver_composition_scheduler_is_concurrent_bounded_correlated_and_joined() -> TestResult
{
    run_driver_scheduler_scenario(DriverSchedulerScenario::Ordinary)
}

#[test]
fn recovery_required_nested_result_fences_sibling_mutation_before_receipt() -> TestResult {
    run_driver_scheduler_scenario(DriverSchedulerScenario::UnknownMutation)
}

#[test]
fn extracted_composition_resource_panics_remain_explicit_recovery_obligations() -> TestResult {
    for scenario in [
        DriverSchedulerScenario::PanicBeforeSourceAdmission,
        DriverSchedulerScenario::PanicAfterSourceAdmission,
        DriverSchedulerScenario::PanicAfterCellAcknowledgement,
    ] {
        run_driver_scheduler_scenario(scenario)?;
    }
    Ok(())
}

#[test]
fn nested_read_only_and_mutating_final_admission_expiry_has_zero_entries() -> TestResult {
    for scenario in [
        DriverSchedulerScenario::ReadOnlyLeaseExpiry,
        DriverSchedulerScenario::MutatingLeaseExpiry,
    ] {
        run_driver_scheduler_scenario(scenario)?;
    }
    Ok(())
}

pub(super) fn run_driver_scheduler_scenario(scenario: DriverSchedulerScenario) -> TestResult {
    run_cancel_cell_scenario(move || async move {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let evidence = Arc::new(DriverSchedulerEvidence {
            scenario,
            ..Default::default()
        });
        let registry = driver_scheduler_registry(Arc::clone(&evidence))?;
        let descriptor = neutral_composition_test_descriptor()?;
        let authority = artifact_authority(&descriptor, 1)?;
        let model = Arc::new(
            ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_6000, false)
                .with_scripted_tool_calls([(
                    0,
                    "exec_js",
                    serde_json::to_string(&json!({"source": "scheduler proof"}))?,
                )]),
        );
        let configured = configured_test_model_for_operation_with_tools(
            model.clone(),
            descriptor.clone(),
            authority,
            &registry,
            LocalDurabilityDisposition::DurableRequired,
            ContextAdmissionRequirement::AnyBounded,
            1,
            ReasoningEffortDisposition::Disabled,
            Some(&["exec_js", "wait_js"]),
            ToolPresentationMode::LocalCompositionOnly,
            false,
        )?;
        let target = composition_projection_target(&configured, &descriptor)?;
        let exact = ProductJavaScriptCompositionV1::compile(
            registry.clone(),
            target,
            [
                "read_a",
                "read_b",
                "read_cancel",
                "mutate_one",
                "mutate_two",
            ]
            .into_iter()
            .map(ToolName::new)
            .collect::<Result<Vec<_>, _>>()?,
        )?;
        let composition_descriptor = LocalCompositionDescriptor {
            outer_projection: exact.outer_projection().clone(),
            nested_projection: exact.nested_projection().clone(),
            nested_tools: exact
                .nested_tools()
                .iter()
                .map(|tool| LocalCompositionToolDefinition {
                    name: tool.exposed_name().as_str().to_owned(),
                    description: tool.description().to_owned(),
                    input_schema: tool.composition_input_schema_value(),
                })
                .collect::<Vec<_>>()
                .into(),
            identity: exact.identity(),
            execute_name: "exec_js".into(),
            wait_name: "wait_js".into(),
            marker_executor: ToolExecutorId::new(
                "plakous.product-javascript-composition.driver-marker.v1",
            )?,
        };
        let integration: Arc<dyn LocalCompositionIntegration> =
            Arc::new(FakeComposition::Scheduler(DriverSchedulerIntegration {
                descriptor: composition_descriptor.clone(),
                evidence: Arc::clone(&evidence),
            }));
        let builder = code_cell_agent_builder(
            model.as_ref(),
            configured,
            registry,
            workspace,
            "test.private-driver-composition-scheduler",
            composition_descriptor,
            integration,
        )?;
        let expiry = matches!(
            scenario,
            DriverSchedulerScenario::ReadOnlyLeaseExpiry
                | DriverSchedulerScenario::MutatingLeaseExpiry
        );
        let mut holder = None;
        let mut scheduler = None;
        let builder = if expiry {
            let shared_scheduler = ToolScheduler::new(1)?;
            let tool = Arc::new(LeasePermitTool::default());
            let holder_agent = lease_permit_builder(
                Arc::new(WorkspacePolicy::host(directory.path(), ".")?),
                Arc::clone(&tool),
                shared_scheduler.clone(),
                false,
                ToolEffect::ReadOnly,
            )?
            .build()?;
            let held = holder_agent.prompt("hold nested admission permit").await?;
            tool.entered.notified().await;
            holder = Some((holder_agent, held, tool));
            scheduler = Some(shared_scheduler.clone());
            builder.tool_scheduler(shared_scheduler, ToolRuntimeConfig::new(1)?)
        } else {
            builder
        };
        let store = Arc::new(TestCreateStore::default());
        if scenario == DriverSchedulerScenario::EdgePublicationFailure {
            store.fail_nested_edge.store(true, Ordering::Release);
        }
        let sessions = DurableSessionRuntime::current(
            store.clone(),
            RuntimeDurability::Required,
            NonZeroU64::new(30).ok_or("nonzero lease")?,
        )?;
        let agent = sessions
            .create("create:private-driver-composition-scheduler", builder)
            .await?;
        let mut shutdown_events = if scenario.panics_during_delivery() {
            Some(agent.attach(ViewRequest::metadata()).await?.into_parts().1)
        } else {
            None
        };
        let command_id = CommandId::new("private-driver-composition-scheduler:root")?;
        let turn = agent
            .start(command_id.clone(), "exercise the private scheduler")
            .await?
            .into_turn()
            .ok_or("scheduler root was not admitted")?;
        let control = turn.control();
        let shared = Arc::clone(&control.shared);
        if let (Some(scheduler), Some((holder_agent, held, holder_tool))) = (scheduler, holder) {
            while scheduler.status().waiting() != 1 {
                tokio::task::yield_now().await;
            }
            let gate = agent
                .inner
                .authority
                .pause_lease()
                .await
                .ok_or("nested lease gate")?;
            let lease = store.active_lease().ok_or("nested active lease")?;
            // Move only the store's authority clock; no wall timer or heartbeat
            // advances while the exact nested permit is blocked at final admission.
            *lock_mutex(&store.publication_clock) =
                Some((tokio::time::Instant::now(), lease.expires_at().seconds()));
            holder_tool.release.notify_one();
            while scheduler.status().waiting() != 0 {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
            assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 0);
            assert_eq!(evidence.mutations_done.load(Ordering::Acquire), 0);
            drop(gate);
            let result = turn.result().await;
            assert!(
                result.is_err()
                    || result.as_ref().is_ok_and(|result| matches!(
                        result.outcome(),
                        TurnResultOutcome::RecoveryRequired(_)
                    ))
            );
            let _ = agent.shutdown().await;
            held.result().await?;
            holder_agent.shutdown().await?;
            assert_eq!(evidence.source_starts.load(Ordering::Acquire), 1);
            assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 0);
            assert_eq!(evidence.mutations_done.load(Ordering::Acquire), 0);
            assert!(
                lock_mutex(&evidence.deliveries)
                    .iter()
                    .all(|(_, accepted, _)| !accepted)
            );
            let drops = lock_mutex(&evidence.responder_drops);
            assert_eq!(drops.iter().filter(|(id, _)| id == "EXPIRY").count(), 1);
            assert_eq!(
                drops
                    .iter()
                    .filter(|(id, _)| id == "EXPIRY_DUPLICATE")
                    .count(),
                1
            );
            assert!(evidence.counters.session_shutdowns.load(Ordering::Acquire) >= 1);
            assert_eq!(scheduler.status().active(), 0);
            return Ok(());
        }
        if matches!(
            scenario,
            DriverSchedulerScenario::PanicBeforeSourceAdmission
                | DriverSchedulerScenario::PanicAfterSourceAdmission
                | DriverSchedulerScenario::PanicAfterCellAcknowledgement
        ) {
            let result = turn.result().await?;
            assert!(
                matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                "{:?}",
                result.outcome()
            );
            assert!(result.recovery_operation().is_some());
            assert_eq!(shared.published_result_count(), 1);
            assert_eq!(
                evidence.source_starts.load(Ordering::Acquire),
                usize::from(scenario != DriverSchedulerScenario::PanicBeforeSourceAdmission)
            );
            assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 0);
            assert_eq!(evidence.mutations_done.load(Ordering::Acquire), 0);
            assert!(lock_mutex(&evidence.deliveries).is_empty());
            // Unwind drops the actual pair. The restored owner's possible-start
            // obligation must never be mistaken for an absent integration.
            assert_eq!(evidence.session_drops.load(Ordering::Acquire), 1);
            assert_eq!(evidence.receiver_drops.load(Ordering::Acquire), 1);
            assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 0);
            assert_eq!(
                evidence.counters.session_shutdowns.load(Ordering::Acquire),
                0
            );
            let _ = agent.shutdown().await;
            assert_eq!(
                evidence
                    .counters
                    .integration_shutdowns
                    .load(Ordering::Acquire),
                1
            );
            return Ok(());
        }
        if scenario == DriverSchedulerScenario::DeliveryHostLost {
            let result = turn.result().await?;
            let TurnResultOutcome::RecoveryRequired(error) = result.outcome() else {
                return Err("missing receipt did not require recovery".into());
            };
            assert_eq!(error.kind(), AgentErrorKind::OutcomeUnknown);
            assert!(result.recovery_operation().is_some());
            assert_exact_terminal_result(&result, &command_id)?;
            assert_eq!(shared.published_result_count(), 1);
            let operations = result.checkpoint().manifest().ledger().operations();
            let edge = operations
                .iter()
                .find(|operation| {
                    matches!(
                        operation.kind(),
                        DurableOperationKind::CompositionChild {
                            request_sequence: 1,
                            ..
                        }
                    )
                })
                .ok_or("committed child edge")?;
            assert!(matches!(edge.state(), DurableOperationState::Completed(_)));
            let DurableOperationKind::CompositionChild { call_id, .. } = edge.kind() else {
                unreachable!()
            };
            let tool = operations.iter().find(|operation| matches!(
                operation.kind(),
                DurableOperationKind::Effect { call_id: candidate, .. } if candidate == call_id
            )).ok_or("committed tool result")?;
            let DurableOperationState::Completed(terminal) = tool.state() else {
                return Err("nested tool outcome was not committed before receipt loss".into());
            };
            let (tag, _, completed): (String, EntryId, ToolExecutionResult) =
                serde_json::from_slice(terminal.bytes())?;
            assert_eq!(tag, "completed-tool-outcome-v1");
            assert!(completed.dispatched);
            assert!(
                matches!(completed.result.parts(), [ToolResultPart::Text(text)] if text.text() == "read-a")
            );
            let _ = agent.shutdown().await;
            assert_eq!(evidence.source_starts.load(Ordering::Acquire), 1);
            assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 1);
            assert_eq!(evidence.mutations_done.load(Ordering::Acquire), 0);
            assert_eq!(
                lock_mutex(&evidence.deliveries)
                    .iter()
                    .filter(|(identity, _, _)| identity == "A")
                    .count(),
                1
            );
            assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
            assert_eq!(
                evidence.counters.session_shutdowns.load(Ordering::Acquire),
                1
            );
            return Ok(());
        }
        if scenario.panics_during_delivery() {
            let result = turn.result().await?;
            assert!(matches!(
                result.outcome(),
                TurnResultOutcome::RecoveryRequired(_)
            ));
            assert!(result.recovery_operation().is_some());
            assert_eq!(shared.published_result_count(), 1);
            assert_eq!(evidence.source_starts.load(Ordering::Acquire), 1);
            assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 1);
            assert_eq!(evidence.mutations_done.load(Ordering::Acquire), 0);
            assert_eq!(evidence.session_drops.load(Ordering::Acquire), 1);
            assert_eq!(evidence.receiver_drops.load(Ordering::Acquire), 1);
            assert_eq!(
                lock_mutex(&evidence.delivery_panics).as_slice(),
                [("A".to_owned(), 0)]
            );
            let releases = store.releases.load(Ordering::Acquire);
            let flushes = store.flushes.load(Ordering::Acquire);
            let shutdown = agent
                .shutdown()
                .await
                .expect_err("delivery panic must remain a cleanup failure");
            assert_eq!(
                evidence
                    .counters
                    .integration_shutdowns
                    .load(Ordering::Acquire),
                1
            );
            assert_eq!(
                lock_mutex(&evidence.delivery_panics).as_slice(),
                [("A".to_owned(), 0), ("A_DUPLICATE".to_owned(), 1),]
            );
            assert!(lock_mutex(&evidence.deliveries).is_empty());
            assert_eq!(store.releases.load(Ordering::Acquire), releases + 1);
            assert_eq!(store.flushes.load(Ordering::Acquire), flushes + 1);
            assert!(lock_mutex(&store.state).active_lease.is_none());
            if scenario == DriverSchedulerScenario::DeliveryPanicsAfterIntegrationLoss {
                assert!(
                    shutdown
                        .message()
                        .contains("injected earlier integration shutdown loss")
                );
            }
            assert!(
                shutdown
                    .message()
                    .contains("agent actor dependency panicked during owned work")
            );
            let events = shutdown_events
                .as_mut()
                .ok_or("shutdown event subscription")?;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match events.recv().await {
                        Ok(_) => {}
                        Err(crate::SubscriptionEnd::AgentClosed { .. }) => {
                            return Ok::<(), Box<dyn std::error::Error>>(());
                        }
                        Err(error) => {
                            return Err(format!("unexpected subscription end: {error:?}").into());
                        }
                    }
                }
            })
            .await??;
            assert_eq!(evidence.source_starts.load(Ordering::Acquire), 1);
            assert_eq!(evidence.read_a_entries.load(Ordering::Acquire), 1);
            return Ok(());
        }
        if scenario == DriverSchedulerScenario::EdgePublicationFailure {
            let result = turn.result().await?;
            assert!(
                matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                "{:?}",
                result.outcome()
            );
            assert!(result.recovery_operation().is_some());
            assert_eq!(shared.published_result_count(), 1);
            let _ = agent.shutdown().await;
            assert_publication_failure_witness(&store, &evidence, &result)?;
            return Ok(());
        }
        if scenario == DriverSchedulerScenario::UnknownMutation {
            let result = turn.result().await?;
            assert!(
                matches!(result.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                "{:?}",
                result.outcome()
            );
            assert_exact_terminal_result(&result, &command_id)?;
            assert_eq!(shared.published_result_count(), 1);
            assert_eq!(
                lock_mutex(&evidence.mutation_order).as_slice(),
                ["mutate_one"]
            );
            assert_eq!(evidence.mutations_done.load(Ordering::Acquire), 1);
            assert!(
                result
                    .checkpoint()
                    .manifest()
                    .ledger()
                    .operations()
                    .into_iter()
                    .any(|operation| matches!(
                        operation.state(),
                        DurableOperationState::OutcomeUnknown(_)
                    ))
            );
            let _ = agent.shutdown().await;
            return Ok(());
        }
        wait_for_scheduler_fact(&evidence.cancel_started).await;
        assert_eq!(control.cancel().await?, CancellationStatus::Requested);
        let result = turn.result().await?;

        assert!(
            matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
            "unexpected composition scheduler outcome: {:?}",
            result.outcome()
        );
        assert_exact_terminal_result(&result, &command_id)?;
        assert_eq!(shared.published_result_count(), 1);
        assert_eq!(evidence.max_active_reads.load(Ordering::Acquire), 2);
        assert!(evidence.cap_rejected.load(Ordering::Acquire));
        assert!(evidence.invalid_rejected.load(Ordering::Acquire));
        assert!(evidence.serial_invalid_delivered.load(Ordering::Acquire));
        assert!(evidence.cancel_joined.load(Ordering::Acquire));
        assert_eq!(evidence.active_reads.load(Ordering::Acquire), 0);
        assert!(!evidence.mutation_overlap.load(Ordering::Acquire));
        assert_eq!(
            lock_mutex(&evidence.read_finish_order).as_slice(),
            ["B", "A"]
        );
        assert_eq!(
            lock_mutex(&evidence.mutation_order).as_slice(),
            ["mutate_one", "mutate_two"]
        );
        {
            let deliveries = lock_mutex(&evidence.deliveries);
            let positions = |identity: &str| {
                deliveries
                    .iter()
                    .position(|(candidate, _, _)| candidate == identity)
                    .ok_or_else(|| format!("missing correlated delivery {identity}"))
            };
            let cap = positions("CAP")?;
            let b = positions("B")?;
            let a = positions("A")?;
            assert!(
                cap < a && a < b,
                "accepted deliveries did not follow request sequence: {deliveries:?}"
            );
            assert!(deliveries[cap].2.contains("limit of 2"));
            assert!(!deliveries[b].1 && deliveries[b].2.contains("read-b failed"));
            assert!(deliveries[a].1 && deliveries[a].2.contains("read-a"));
            let mutation = positions("M1")?;
            assert!(!deliveries[mutation].1);
            assert!(
                deliveries[mutation]
                    .2
                    .contains("mutation failed after partial effects")
            );
            assert!(deliveries[positions("M2")?].1);
            let invalid_mutation = positions("INVALID_MUTATION")?;
            assert!(
                !deliveries[invalid_mutation].1,
                "ordinary tool rejection must reject the nested promise"
            );
        }
        if scenario == DriverSchedulerScenario::DeliveryRefused {
            // A refusal is the cell's own answer that it no longer owns the
            // request: the driver neither replays it nor repeats its effect.
            let deliveries = lock_mutex(&evidence.deliveries);
            let mut identities = deliveries
                .iter()
                .map(|(identity, _, _)| identity.clone())
                .collect::<Vec<_>>();
            let delivered = identities.len();
            identities.sort();
            identities.dedup();
            assert_eq!(
                identities.len(),
                delivered,
                "a refused delivery must not be replayed: {deliveries:?}"
            );
            drop(deliveries);
            // The committed child outcome stands although the cell refused
            // every acknowledgement: each dispatched child is still finalized
            // in the ledger, and the driver carried the turn to its terminal.
            let dispatched_children = result
                .checkpoint()
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .filter(|record| {
                    matches!(record.kind(), DurableOperationKind::CompositionChild { .. })
                        && record.transitions().iter().any(|transition| {
                            matches!(transition.state(), DurableOperationState::Dispatched)
                        })
                })
                .collect::<Vec<_>>();
            assert!(
                !dispatched_children.is_empty(),
                "the refusal scenario must dispatch at least one child"
            );
            assert!(
                dispatched_children
                    .iter()
                    .all(|record| matches!(record.state(), DurableOperationState::Finalized(_))),
                "a refused acknowledgement must leave the committed child outcome finalized"
            );
        }
        let undispatched_children = result
            .checkpoint()
            .manifest()
            .ledger()
            .operations()
            .into_iter()
            .filter(|record| {
                matches!(record.kind(), DurableOperationKind::CompositionChild { .. })
                    && record.transitions().iter().any(|transition| {
                        matches!(
                            transition.state(),
                            DurableOperationState::ProvenNotDispatched(_)
                        )
                    })
            })
            .collect::<Vec<_>>();
        assert_eq!(undispatched_children.len(), 2);
        assert!(undispatched_children.iter().all(|record| {
            matches!(record.state(), DurableOperationState::Finalized(_))
                && !record.transitions().iter().any(|transition| {
                    matches!(transition.state(), DurableOperationState::Dispatched)
                })
        }));
        assert_eq!(evidence.counters.terminates.load(Ordering::Acquire), 1);
        assert_eq!(
            evidence.counters.session_shutdowns.load(Ordering::Acquire),
            1
        );

        agent.shutdown().await?;
        assert_eq!(
            evidence
                .counters
                .integration_shutdowns
                .load(Ordering::Acquire),
            1
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}

#[test]
fn accepted_root_and_completed_evidence_remain_working_until_terminal_fold() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(120))?;
    let binding = Digest::sha256(DigestDomain::ProviderOperation, 4, &[b"root"])?;
    let bytes = serde_json::to_vec(&("root:1", 1_u64, turn_id, binding, "prompt", false))?;
    let command = DurableCommandRecord::new(
        "root:1",
        1,
        Some(turn_id),
        binding,
        runtime_component(RuntimeComponentKind::Command, bytes.clone())?,
        DurableCommandDisposition::Accepted,
        None,
    )?;
    let checkpoint = |value| CheckpointId::try_from(uuid::Uuid::from_u128(value));
    let operation = DurableOperationRecord::new(
        1,
        DurableOperationKind::Command { turn_id: None },
        runtime_component(RuntimeComponentKind::Operation, bytes)?,
        vec![DurableOperationTransition::new(
            checkpoint(121)?,
            DurableOperationState::Prepared,
        )],
    )?;
    let ledger = RuntimeOperationLedger::new(vec![command], vec![operation])?;
    assert!(matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id: bound, .. },
            ..
        } if bound == turn_id
    ));
    let ledger = ledger.transition_operation(
        1,
        DurableOperationTransition::new(checkpoint(122)?, DurableOperationState::Dispatched),
    )?;
    assert!(matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Working { .. }
    ));
    let terminal = runtime_component(RuntimeComponentKind::Operation, b"terminal".to_vec())?;
    let ledger = ledger.transition_operation(
        1,
        DurableOperationTransition::new(
            checkpoint(123)?,
            DurableOperationState::Completed(terminal.clone()),
        ),
    )?;
    assert!(matches!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Working {
            recovery_phase: WorkingRecoveryPhase::Finalization,
            ..
        }
    ));
    let ledger = ledger.transition_operation(
        1,
        DurableOperationTransition::new(
            checkpoint(124)?,
            DurableOperationState::Finalized(terminal),
        ),
    )?;
    assert_eq!(
        working_lifecycle(turn_id, &ledger)?,
        RuntimeLifecycle::Quiescent
    );
    Ok(())
}

#[test]
fn crashed_code_cell_requires_abandonment_and_never_predispatch_replay() -> TestResult {
    let turn_id = TurnId::try_from(uuid::Uuid::from_u128(130))?;
    let checkpoint = |value| CheckpointId::try_from(uuid::Uuid::from_u128(value));
    let operation = DurableOperationRecord::new(
        7,
        DurableOperationKind::CompositionEvaluation {
            turn_id,
            attempt_id: AttemptId::try_from(uuid::Uuid::from_u128(131))?,
            call_id: CallId::try_from(uuid::Uuid::from_u128(132))?,
            source_index: 0,
        },
        runtime_component(
            RuntimeComponentKind::Operation,
            b"source-digest-and-cell-start-fence".to_vec(),
        )?,
        vec![
            DurableOperationTransition::new(checkpoint(133)?, DurableOperationState::Prepared),
            DurableOperationTransition::new(checkpoint(134)?, DurableOperationState::Dispatched),
        ],
    )?;
    let ledger = RuntimeOperationLedger::new(Vec::new(), vec![operation])?;
    let RuntimeLifecycle::Working {
        recovery_phase,
        obligations,
        ..
    } = working_lifecycle(turn_id, &ledger)?
    else {
        return Err("crashed code cell unexpectedly quiescent".into());
    };
    assert_eq!(recovery_phase, WorkingRecoveryPhase::EffectReconciliation);
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0].kind(), RecoveryObligationKind::CodeCell);
    assert_eq!(
        obligations[0].action(),
        RecoveryAction::EvaluationAbandonmentRequired
    );
    Ok(())
}

#[test]
fn product_composition_recovery_boundary_matrix_is_provider_neutral_and_non_replaying() -> TestResult
{
    #[derive(Clone, Copy, Debug)]
    enum OperationBoundary {
        Evaluation,
        Child,
        Continuation,
    }

    #[derive(Clone, Copy, Debug)]
    enum CrashBoundary {
        Prepared,
        Dispatched,
        Completed,
        Finalized,
    }

    let checkpoint =
        |profile_index: u128, operation_index: u128, state_index: u128, transition_index: u128| {
            CheckpointId::try_from(uuid::Uuid::from_u128(
                0xc0de_7000
                    + profile_index * 0x1000
                    + operation_index * 0x100
                    + state_index * 0x10
                    + transition_index,
            ))
        };
    let evidence =
        |profile: &str, operation: &str, state: &str| -> TestResult<ReconstructiveComponent> {
            Ok(runtime_component(
                RuntimeComponentKind::Operation,
                serde_json::to_vec(&(
                    "product-composition-recovery-boundary-v1",
                    profile,
                    operation,
                    state,
                ))?,
            )?)
        };

    for (profile_index, (profile, execute_name)) in [
        ("ProductJavaScriptCompositionV1", "exec_js"),
        ("PythonPtcV1", "exec_py"),
    ]
    .into_iter()
    .enumerate()
    {
        for (operation_index, operation) in [
            OperationBoundary::Evaluation,
            OperationBoundary::Child,
            OperationBoundary::Continuation,
        ]
        .into_iter()
        .enumerate()
        {
            for (state_index, boundary) in [
                CrashBoundary::Prepared,
                CrashBoundary::Dispatched,
                CrashBoundary::Completed,
                CrashBoundary::Finalized,
            ]
            .into_iter()
            .enumerate()
            {
                let profile_index = profile_index as u128;
                let operation_index = operation_index as u128;
                let state_index = state_index as u128;
                let turn_id = TurnId::try_from(uuid::Uuid::from_u128(
                    0xc0de_8000 + profile_index * 0x1000 + operation_index * 0x100 + state_index,
                ))?;
                let attempt_id = AttemptId::try_from(uuid::Uuid::from_u128(
                    0xc0de_9000 + profile_index * 0x1000 + operation_index * 0x100 + state_index,
                ))?;
                let call_id = CallId::try_from(uuid::Uuid::from_u128(
                    0xc0de_a000 + profile_index * 0x1000 + operation_index * 0x100 + state_index,
                ))?;
                let operation_name = match operation {
                    OperationBoundary::Evaluation => "evaluation",
                    OperationBoundary::Child => "child",
                    OperationBoundary::Continuation => "continuation",
                };
                let state_name = match boundary {
                    CrashBoundary::Prepared => "prepared",
                    CrashBoundary::Dispatched => "dispatched",
                    CrashBoundary::Completed => "completed",
                    CrashBoundary::Finalized => "finalized",
                };
                let terminal_evidence = evidence(profile, operation_name, state_name)?;
                let mut transitions = vec![DurableOperationTransition::new(
                    checkpoint(profile_index, operation_index, state_index, 1)?,
                    DurableOperationState::Prepared,
                )];
                if !matches!(boundary, CrashBoundary::Prepared) {
                    transitions.push(DurableOperationTransition::new(
                        checkpoint(profile_index, operation_index, state_index, 2)?,
                        DurableOperationState::Dispatched,
                    ));
                }
                if matches!(
                    boundary,
                    CrashBoundary::Completed | CrashBoundary::Finalized
                ) {
                    transitions.push(DurableOperationTransition::new(
                        checkpoint(profile_index, operation_index, state_index, 3)?,
                        DurableOperationState::Completed(terminal_evidence.clone()),
                    ));
                }
                if matches!(boundary, CrashBoundary::Finalized) {
                    transitions.push(DurableOperationTransition::new(
                        checkpoint(profile_index, operation_index, state_index, 4)?,
                        DurableOperationState::Finalized(terminal_evidence),
                    ));
                }
                let kind = match operation {
                    OperationBoundary::Evaluation => DurableOperationKind::CompositionEvaluation {
                        turn_id,
                        attempt_id,
                        call_id,
                        source_index: 0,
                    },
                    OperationBoundary::Child => DurableOperationKind::CompositionChild {
                        turn_id,
                        attempt_id,
                        call_id,
                        evaluation_operation_id: 1,
                        interpreter_generation: 1,
                        cell_id: format!("{profile}-cell"),
                        request_sequence: 1,
                    },
                    OperationBoundary::Continuation => {
                        DurableOperationKind::CompositionContinuation {
                            turn_id,
                            attempt_id,
                            call_id,
                            source_index: 0,
                            evaluation_operation_id: 1,
                        }
                    }
                };
                let record = DurableOperationRecord::new(
                    1,
                    kind,
                    runtime_component(
                        RuntimeComponentKind::Operation,
                        serde_json::to_vec(&(
                            "product-composition-request-v1",
                            profile,
                            execute_name,
                            operation_name,
                        ))?,
                    )?,
                    transitions,
                )?;
                assert!(
                    record
                        .transitions()
                        .iter()
                        .filter(|transition| matches!(
                            transition.state(),
                            DurableOperationState::Dispatched
                        ))
                        .count()
                        <= 1,
                    "{profile} {operation_name} {state_name} recorded duplicate dispatch"
                );
                let ledger = RuntimeOperationLedger::new(Vec::new(), vec![record])?;
                match (boundary, working_lifecycle(turn_id, &ledger)?) {
                    (CrashBoundary::Finalized, RuntimeLifecycle::Quiescent) => {}
                    (boundary, RuntimeLifecycle::Working { obligations, .. }) => {
                        assert_eq!(
                            obligations.len(),
                            1,
                            "{profile} {operation_name} {state_name}"
                        );
                        assert_eq!(obligations[0].kind(), RecoveryObligationKind::CodeCell);
                        let expected = match boundary {
                            CrashBoundary::Prepared => RecoveryAction::ProvenNotDispatched,
                            CrashBoundary::Dispatched => {
                                RecoveryAction::EvaluationAbandonmentRequired
                            }
                            CrashBoundary::Completed => RecoveryAction::FinalizationRequired,
                            CrashBoundary::Finalized => unreachable!(),
                        };
                        assert_eq!(
                            obligations[0].action(),
                            expected,
                            "{profile} {operation_name} {state_name}"
                        );
                    }
                    (boundary, lifecycle) => {
                        return Err(format!(
                            "{profile} {operation_name} {boundary:?} produced {lifecycle:?}"
                        )
                        .into());
                    }
                }
            }
        }
    }
    Ok(())
}
