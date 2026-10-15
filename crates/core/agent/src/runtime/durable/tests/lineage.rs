use super::*;

/// Exact local-composition marker family under lineage test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LineagePtcClass {
    Native,
    JavaScript,
    Python,
}

impl LineagePtcClass {
    pub(super) const fn execute_name(self) -> &'static str {
        match self {
            Self::Native => "exec",
            Self::JavaScript => "exec_js",
            Self::Python => "exec_py",
        }
    }

    pub(super) const fn wait_name(self) -> &'static str {
        match self {
            Self::Native => "wait",
            Self::JavaScript => "wait_js",
            Self::Python => "wait_py",
        }
    }

    pub(super) fn configured_tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Native => &["exec", "wait"],
            Self::JavaScript => &["exec_js", "wait_js"],
            Self::Python => &["exec_py", "wait_py"],
        }
    }

    pub(super) const fn marker_executor_id(self) -> &'static str {
        match self {
            Self::Native => "plakous.codex-code-mode.driver-marker.v1",
            Self::JavaScript => "plakous.product-javascript-composition.driver-marker.v1",
            Self::Python => "plakous.python-ptc.driver-marker.v1",
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Native => "native-ptc",
            Self::JavaScript => "js-ptc",
            Self::Python => "py-ptc",
        }
    }

    /// Exact model-facing arguments each marker's own input contract admits.
    ///
    /// Codex native `exec` is a raw UTF-8 source marker; `exec_js` and
    /// `exec_py` are structured functions with different source keys. Only
    /// the arguments differ; the lineage projection does not.
    pub(super) fn execute_arguments(self) -> TestResult<String> {
        Ok(match self {
            Self::Native => serde_json::to_string("lineage proof")?,
            Self::JavaScript => serde_json::to_string(&json!({"source": "lineage proof"}))?,
            Self::Python => serde_json::to_string(&json!({"code": "lineage proof"}))?,
        })
    }
}

pub(super) const LINEAGE_CELL_ID: &str = "lineage-cell";

const LINEAGE_NESTED_TOOL: &str = "read_file";

const LINEAGE_NESTED_CALLS: usize = 2;

#[derive(Clone, Copy)]
pub(super) enum LineageScenario {
    Read,
    Patch,
    PatchAfterRejectedObject,
    PatchAfterRejectedEmpty,
}

impl LineageScenario {
    pub(super) const fn nested_name(self) -> &'static str {
        match self {
            Self::Read => LINEAGE_NESTED_TOOL,
            Self::Patch | Self::PatchAfterRejectedObject | Self::PatchAfterRejectedEmpty => {
                "apply_patch"
            }
        }
    }

    pub(super) const fn nested_calls(self) -> usize {
        match self {
            Self::Read => LINEAGE_NESTED_CALLS,
            Self::Patch => 1,
            Self::PatchAfterRejectedObject | Self::PatchAfterRejectedEmpty => 2,
        }
    }

    pub(super) fn nested_input(self, index: usize) -> serde_json::Value {
        match self {
            Self::Read => json!({"path": "lineage.txt"}),
            Self::PatchAfterRejectedObject if index == 0 => json!({"patch": "wrong shape"}),
            Self::PatchAfterRejectedEmpty if index == 0 => {
                serde_json::Value::String(String::new())
            }
            Self::Patch | Self::PatchAfterRejectedObject | Self::PatchAfterRejectedEmpty => serde_json::Value::String(
                "*** Begin Patch\n*** Add File: projected.txt\n+λ \"quote\" \\ slash\n*** End Patch\n"
                    .to_owned(),
            ),
        }
    }
}

#[derive(Default)]
pub(super) struct LineageEvidence {
    pub(super) counters: CompositionCounters,
    pub(super) invalid_read: bool,
    /// Set once a cell is running, after its marker's dispatch was recorded.
    pub(super) cell_running: Arc<AtomicBool>,
    pub(super) delivered: AtomicUsize,
    pub(super) rejected: AtomicUsize,
    pub(super) reject_reasons: Mutex<Vec<String>>,
}

pub(super) struct LineageResponder {
    pub(super) evidence: Arc<LineageEvidence>,
}

impl crate::local_composition::LocalCompositionNestedResponder for LineageResponder {
    fn resolve(
        self: Box<Self>,
        _value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            self.evidence.delivered.fetch_add(1, Ordering::AcqRel);
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, crate::local_composition::LocalCompositionNestedDelivery>
    {
        Box::pin(async move {
            self.evidence.delivered.fetch_add(1, Ordering::AcqRel);
            self.evidence.rejected.fetch_add(1, Ordering::AcqRel);
            lock_mutex(&self.evidence.reject_reasons).push(reason);
            Ok(crate::local_composition::LocalCompositionNestedDelivery::Accepted)
        })
    }
}

pub(super) struct LineageEvents {
    pub(super) next: usize,
    pub(super) evidence: Arc<LineageEvidence>,
    pub(super) scenario: LineageScenario,
}

pub(super) struct LineageSession {
    pub(super) evidence: Arc<LineageEvidence>,
    pub(super) scenario: LineageScenario,
}

pub(super) struct LineageIntegration {
    pub(super) class: LineagePtcClass,
    pub(super) descriptor: LocalCompositionDescriptor,
    pub(super) evidence: Arc<LineageEvidence>,
    pub(super) scenario: LineageScenario,
}

pub(super) fn lineage_registry() -> TestResult<FrozenToolRegistry> {
    let mut builder = ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut builder,
    )?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut builder)?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut builder)?;
    PythonPtcV1::register_markers_into(&mut builder)?;
    Ok(builder.freeze())
}

fn lineage_nested_definitions(scenario: LineageScenario) -> Vec<LocalCompositionToolDefinition> {
    vec![LocalCompositionToolDefinition {
        name: scenario.nested_name().to_owned(),
        description: "Lineage nested probe.".to_owned(),
        input_schema: match scenario {
            LineageScenario::Read => json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
            LineageScenario::Patch
            | LineageScenario::PatchAfterRejectedObject
            | LineageScenario::PatchAfterRejectedEmpty => {
                json!({"type": "string"})
            }
        },
    }]
}

pub(super) fn lineage_composition_descriptor(
    class: LineagePtcClass,
    scenario: LineageScenario,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
) -> TestResult<LocalCompositionDescriptor> {
    let nested = [ToolName::new(scenario.nested_name())?];
    let (outer_projection, nested_projection, identity) = match class {
        LineagePtcClass::Native => {
            let exact = CodexNativeLocalCodeModeV1::compile(
                plakous_tools::JobConfig::default(),
                registry,
                target,
            )?;
            (
                exact.outer_projection().clone(),
                exact.nested_projection().clone(),
                exact.identity(),
            )
        }
        LineagePtcClass::JavaScript => {
            let exact = match scenario {
                LineageScenario::Read => {
                    ProductJavaScriptCompositionV1::compile(registry, target, nested)?
                }
                LineageScenario::Patch
                | LineageScenario::PatchAfterRejectedObject
                | LineageScenario::PatchAfterRejectedEmpty => {
                    ProductJavaScriptCompositionV1::compile_for_kind(
                        plakous_tools::JobConfig::default(),
                        registry,
                        target,
                        CodingPresentation::Codex,
                        nested,
                    )?
                }
            };
            (
                exact.outer_projection().clone(),
                exact.nested_projection().clone(),
                exact.identity(),
            )
        }
        LineagePtcClass::Python => {
            let exact = match scenario {
                LineageScenario::Read => PythonPtcV1::compile(
                    registry,
                    target,
                    PythonPtcPresentationV1::StructuredFunctions,
                    nested,
                )?,
                LineageScenario::Patch
                | LineageScenario::PatchAfterRejectedObject
                | LineageScenario::PatchAfterRejectedEmpty => PythonPtcV1::compile_for_kind(
                    plakous_tools::JobConfig::default(),
                    registry,
                    target,
                    PythonPtcPresentationV1::StructuredFunctions,
                    CodingPresentation::Codex,
                    nested,
                )?,
            };
            (
                exact.outer_projection().clone(),
                exact.nested_projection().clone(),
                exact.identity(),
            )
        }
    };
    Ok(LocalCompositionDescriptor {
        outer_projection,
        nested_projection,
        nested_tools: lineage_nested_definitions(scenario).into(),
        identity,
        execute_name: class.execute_name().into(),
        wait_name: class.wait_name().into(),
        marker_executor: ToolExecutorId::new(class.marker_executor_id())?,
    })
}

/// One projected tool lifecycle observation, normalized for cross-class comparison.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct LineageObservation {
    pub(super) name: String,
    pub(super) phase: &'static str,
    pub(super) class: ToolCallerClass,
    pub(super) cell_id: Option<String>,
    pub(super) parent_is_outer_marker: Option<bool>,
    pub(super) nested_ordinal: Option<u32>,
    pub(super) depth: Option<u16>,
}

async fn observe_lineage_for_class(class: LineagePtcClass) -> TestResult<Vec<LineageObservation>> {
    observe_lineage_for_scenario(class, LineageScenario::Read).await
}

async fn observe_lineage_for_scenario(
    class: LineagePtcClass,
    scenario: LineageScenario,
) -> TestResult<Vec<LineageObservation>> {
    observe_lineage(class, scenario, false).await
}

/// Runs one lineage cell. Under `pressure` the store reports ordinary work
/// exhausted from the moment the cell runs: its nested calls must be refused
/// definitely, and the turn must still reach a definite terminal.
async fn observe_lineage(
    class: LineagePtcClass,
    scenario: LineageScenario,
    pressure: bool,
) -> TestResult<Vec<LineageObservation>> {
    observe_lineage_effect_fixture(class, scenario, pressure, None).await
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum LineageEffectFixture {
    Mutating,
    Undispatched,
    Failed,
}

struct CancellableLineageRead;

impl plakous_tools::ToolExecutor for CancellableLineageRead {
    fn execute(
        self: Arc<Self>,
        _invocation: ToolInvocation,
        context: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            context.cancellation().cancelled().await;
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "offline cancellation probe",
            ))
        })
    }
}

async fn observe_lineage_effect_fixture(
    class: LineagePtcClass,
    scenario: LineageScenario,
    pressure: bool,
    fixture: Option<LineageEffectFixture>,
) -> TestResult<Vec<LineageObservation>> {
    Ok(run_lineage_fixture(class, scenario, pressure, fixture)
        .await?
        .0)
}

/// The real composition driver run behind the lineage pins, with the store
/// that recorded its commits.
#[allow(
    clippy::too_many_lines,
    reason = "real composition driver and retained evidence proof"
)]
pub(super) async fn run_lineage_fixture(
    class: LineagePtcClass,
    scenario: LineageScenario,
    pressure: bool,
    fixture: Option<LineageEffectFixture>,
) -> TestResult<(Vec<LineageObservation>, Arc<TestCreateStore>)> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = if fixture == Some(LineageEffectFixture::Failed) {
        let mut registry = ToolRegistryBuilder::default();
        registry.register(ToolRegistration::new(
            RegisteredToolId::new("lineage.cancel.read")?, ToolExecutorId::new("lineage.cancel.executor")?,
            plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"lineage.cancel.executor"])?,
            ToolDefinition::new(ToolName::new("read_file")?, "1.0.0", "Offline cancellation probe.",
                ToolInputContract::structured(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}))?,
                ToolOutputContract::model_content(128, 1_048_576)?, ToolConcurrency::Parallel, ToolEffect::ReadOnly)?,
            Arc::new(CancellableLineageRead),
        ).nested_eligible())?;
        CodexNativeLocalCodeModeV1::register_markers_into(&mut registry)?;
        ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
        registry.freeze()
    } else {
        lineage_registry()?
    };
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_7000, false)
            .with_scripted_tool_calls([(0, class.execute_name(), class.execute_arguments()?)]),
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
        Some(class.configured_tool_names()),
        ToolPresentationMode::LocalCompositionOnly,
        false,
    )?;
    let target = composition_projection_target(&configured, &descriptor)?;
    let composition_descriptor =
        lineage_composition_descriptor(class, scenario, registry.clone(), target)?;
    let name = ToolName::new(scenario.nested_name())?;
    if matches!(
        scenario,
        LineageScenario::Patch
            | LineageScenario::PatchAfterRejectedObject
            | LineageScenario::PatchAfterRejectedEmpty
    ) {
        for invalid in [Some(json!({"patch": "not raw text"})), None] {
            let error =
                crate::runtime::composition::projection::local_composition_nested_call_arguments(
                    &composition_descriptor.nested_projection,
                    &name,
                    invalid.as_ref(),
                )
                .err()
                .ok_or("custom nested input accepted an object or absent value")?;
            assert_eq!(error.kind(), AgentErrorKind::InvalidInput);
        }
    } else {
        assert_eq!(
            crate::runtime::composition::projection::local_composition_nested_call_arguments(
                &composition_descriptor.nested_projection,
                &name,
                None,
            )?,
            "{}"
        );
    }
    let evidence = Arc::new(LineageEvidence {
        invalid_read: fixture == Some(LineageEffectFixture::Undispatched),
        ..LineageEvidence::default()
    });
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(FakeComposition::Lineage(LineageIntegration {
            class,
            descriptor: composition_descriptor.clone(),
            evidence: Arc::clone(&evidence),
            scenario,
        }));
    let builder = code_cell_agent_builder(
        model.as_ref(),
        configured,
        registry,
        workspace,
        "test.tool-caller-lineage",
        composition_descriptor,
        integration,
    )?;
    let test_store = Arc::new(TestCreateStore::default());
    if pressure {
        *lock_mutex(&test_store.ordinary_exhausted) = Some(Arc::clone(&evidence.cell_running));
    }
    let store: Arc<dyn SessionStore> = test_store.clone();
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create(&format!("create:lineage-{}", class.label()), builder)
        .await?;
    let mut turn = agent
        .start(
            CommandId::new(format!("lineage-{}:root", class.label()))?,
            "prove tool caller lineage",
        )
        .await?
        .into_turn()
        .ok_or("lineage root was not admitted")?;
    let mut events = turn.take_events()?;

    let mut outer_call_id: Option<CallId> = None;
    let mut observed = Vec::new();
    let mut cancel_requested = false;
    loop {
        let event = events.recv().await?;
        let phase = match event.kind() {
            AgentEventKind::ToolStarted { .. } => "started",
            AgentEventKind::ToolProgress { .. } => "progress",
            AgentEventKind::ToolCompleted { .. } => "completed",
            AgentEventKind::ToolAuthorized { .. } => "authorized",
            AgentEventKind::ToolPreparationRejected { .. } => "prepared_rejected",
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. }
                if pressure || fixture == Some(LineageEffectFixture::Failed) =>
            {
                break;
            }
            AgentEventKind::TurnFailed { kind, .. } => {
                return Err(format!(
                    "lineage turn emitted failure: {kind:?}; delivered={} rejected={} reasons={:?}",
                    evidence.delivered.load(Ordering::Acquire),
                    evidence.rejected.load(Ordering::Acquire),
                    lock_mutex(&evidence.reject_reasons),
                )
                .into());
            }
            _ => continue,
        };
        let lineage = event
            .kind()
            .tool_caller_lineage()
            .ok_or("a tool lifecycle observation omitted caller lineage")?;
        let name = event.kind().tool_audit_identity().map_or_else(
            || "<rejected>".to_owned(),
            |identity| identity.canonical_name().as_str().to_owned(),
        );
        if name == class.execute_name() && phase == "started" {
            outer_call_id = event.call_id();
        }
        if fixture == Some(LineageEffectFixture::Failed)
            && name == "read_file"
            && phase == "started"
            && !cancel_requested
        {
            turn.control().cancel().await?;
            cancel_requested = true;
        }
        observed.push(LineageObservation {
            name: if name == class.execute_name() {
                "<marker>".to_owned()
            } else {
                name
            },
            phase,
            class: lineage.class(),
            cell_id: lineage.cell_id().map(str::to_owned),
            parent_is_outer_marker: lineage
                .parent_call_id()
                .map(|parent| Some(parent) == outer_call_id),
            nested_ordinal: lineage.nested_ordinal(),
            depth: lineage.depth(),
        });
    }
    let result = turn.result().await?;
    if fixture != Some(LineageEffectFixture::Failed) {
        assert_eq!(
            evidence.delivered.load(Ordering::Acquire),
            scenario.nested_calls()
        );
    }
    if pressure {
        // Every nested call was refused before its edge was prepared, the
        // cell ran to its own result, and the model call after it was refused
        // at its first fence: a definite failure, never recovery.
        assert_eq!(
            evidence.rejected.load(Ordering::Acquire),
            scenario.nested_calls()
        );
        assert!(
            lock_mutex(&evidence.reject_reasons)
                .iter()
                .all(|reason| reason.contains("storage pressure")),
            "{:?}",
            lock_mutex(&evidence.reject_reasons)
        );
        let TurnResultOutcome::Failed(error) = result.outcome() else {
            return Err(format!("pressured cell turn ended {:?}", result.outcome()).into());
        };
        assert_eq!(error.kind(), AgentErrorKind::Session);
        agent.shutdown().await?;
        let head = lock_mutex(&test_store.state)
            .stored
            .clone()
            .ok_or("the pressured session has no durable head")?;
        assert!(
            matches!(
                head.checkpoint().manifest().lifecycle(),
                RuntimeLifecycle::Quiescent
            ),
            "the pressured turn left its head needing recovery"
        );
        return Ok((observed, test_store));
    }
    if !matches!(scenario, LineageScenario::Read) {
        assert_eq!(
            evidence.rejected.load(Ordering::Acquire),
            usize::from(matches!(
                scenario,
                LineageScenario::PatchAfterRejectedObject
                    | LineageScenario::PatchAfterRejectedEmpty
            ))
        );
    }
    if matches!(
        scenario,
        LineageScenario::Patch
            | LineageScenario::PatchAfterRejectedObject
            | LineageScenario::PatchAfterRejectedEmpty
    ) {
        assert_eq!(
            std::fs::read(directory.path().join("projected.txt"))?,
            "λ \"quote\" \\ slash\n".as_bytes()
        );
    }
    let checkpoint = agent.checkpoint();
    let manifest = checkpoint.manifest();
    let operations = manifest.ledger().operations();
    let mut canonical_children = 0;
    for record in &operations {
        if !matches!(record.kind(), DurableOperationKind::Effect { .. }) {
            continue;
        }
        let raw: serde_json::Value = serde_json::from_slice(record.request().bytes())?;
        let raw_call = raw
            .get("call")
            .or_else(|| {
                raw.as_array()
                    .and_then(|parts| parts.get(if parts.len() == 5 { 2 } else { 1 }))
            })
            .ok_or("effect has no call")?;
        let call: ToolCallBlock = serde_json::from_value(raw_call.clone())?;
        if let Some(fixture) = fixture {
            match fixture {
                LineageEffectFixture::Mutating => assert!(raw.is_object()),
                LineageEffectFixture::Undispatched => {
                    assert_eq!(raw[0], "proven-not-dispatched-tool-v1");
                }
                LineageEffectFixture::Failed => {
                    let (DurableOperationState::Completed(outcome)
                    | DurableOperationState::Finalized(outcome)) = record.state()
                    else {
                        return Err("failed probe has no definitive evidence".into());
                    };
                    let raw: serde_json::Value = serde_json::from_slice(outcome.bytes())?;
                    assert_eq!(raw[0], "failed-tool-outcome-v1");
                }
            }
        }
        let plakous_model::ToolCallerLineage::LocalProgrammatic {
            evaluation_operation_id,
            child_call_id,
            interpreter_generation,
            cell_id,
            request_sequence,
            ..
        } = call.correlation().caller_lineage()
        else {
            continue;
        };
        let evaluation = manifest
            .ledger()
            .operation(*evaluation_operation_id)
            .ok_or("canonical evaluation id is not a ledger key")?;
        let DurableOperationKind::CompositionEvaluation { call_id: owner, .. } = evaluation.kind()
        else {
            return Err("canonical evaluation id names another operation kind".into());
        };
        assert_eq!(Some(*owner), outer_call_id);
        assert_eq!(*child_call_id, call.call_id());
        assert!(operations.iter().any(|record| matches!(record.kind(),
            DurableOperationKind::CompositionChild {call_id, evaluation_operation_id: bound_evaluation, interpreter_generation: bound_generation, cell_id: bound_cell, request_sequence: bound_sequence, ..}
                if call_id == child_call_id && bound_evaluation == evaluation_operation_id && bound_generation == interpreter_generation && bound_cell == cell_id && bound_sequence == request_sequence
        )), "canonical child has no exact ledger edge");
        canonical_children += 1;
    }
    assert!(canonical_children > 0);
    if fixture.is_some() {
        let owners = std::collections::HashSet::from([outer_call_id.ok_or("outer marker")?]);
        let projected = crate::facade_support::history_nested_tools(
            manifest.ledger(),
            &owners,
            2048,
            8 * 1_048_576,
        );
        assert_eq!(
            projected.len(),
            canonical_children,
            "real effect rows were silently dropped"
        );
        assert!(projected.iter().all(
            |call| call.result.succeeded() == (fixture == Some(LineageEffectFixture::Mutating))
        ));
    }
    agent.shutdown().await?;
    Ok((observed, test_store))
}

#[tokio::test]
async fn real_driver_retained_mutating_undispatched_and_failed_effects_project() -> TestResult {
    for fixture in [
        LineageEffectFixture::Mutating,
        LineageEffectFixture::Undispatched,
        LineageEffectFixture::Failed,
    ] {
        observe_lineage_effect_fixture(
            LineagePtcClass::JavaScript,
            if fixture == LineageEffectFixture::Mutating {
                LineageScenario::Patch
            } else {
                LineageScenario::Read
            },
            false,
            Some(fixture),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn driver_canonical_nested_lineage_matches_evaluation_and_child_records() -> TestResult {
    for class in [LineagePtcClass::Native, LineagePtcClass::JavaScript] {
        observe_lineage_for_class(class).await?;
    }
    Ok(())
}

/// With ordinary work exhausted while a Code Cell runs, each nested call is
/// refused definitely before its edge is prepared, the cell runs to its own
/// result, and the turn ends with a definite failure on a quiescent head.
#[tokio::test]
async fn a_cell_nested_call_under_storage_pressure_is_refused_and_the_turn_settles() -> TestResult {
    observe_lineage(LineagePtcClass::Native, LineageScenario::Read, true).await?;
    Ok(())
}

#[tokio::test]
async fn every_ptc_class_dispatches_raw_nested_patch_through_the_file_executor() -> TestResult {
    for class in [
        LineagePtcClass::Native,
        LineagePtcClass::JavaScript,
        LineagePtcClass::Python,
    ] {
        let observed = observe_lineage_for_scenario(class, LineageScenario::Patch).await?;
        assert!(
            observed
                .iter()
                .any(|event| { event.name == "patch_envelope" && event.phase == "completed" })
        );
    }
    Ok(())
}

async fn direct_custom_patch_decodes_canonical_model_string_before_executor(
    projected: bool,
) -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let registry = if projected {
        lineage_registry()?
    } else {
        let mut builder = ToolRegistryBuilder::default();
        for pack in plakous_compose::standard_coding_packs_in_memory()? {
            for registration in pack.registrations()? {
                if registration.name().as_str() == "patch_envelope" {
                    builder.register(registration)?;
                }
            }
        }
        builder.freeze()
    };
    let descriptor = neutral_composition_test_descriptor()?;
    let authority = artifact_authority(&descriptor, 1)?;
    // Responses stores custom input as a JSON string in the canonical call.
    // Its model-visible patch can omit the final newline.
    let patch = "*** Begin Patch\n*** Add File: direct.txt\n+λ \"quote\" \\ slash\n*** End Patch";
    let canonical_arguments = serde_json::to_string(patch)?;
    let model = Arc::new(
        ArtifactModel::new(descriptor.clone(), authority.clone(), 0, 0xc0de_7100, false)
            .with_scripted_tool_calls([(
                0,
                if projected {
                    "apply_patch"
                } else {
                    "patch_envelope"
                },
                canonical_arguments,
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
        None,
        ToolPresentationMode::Direct,
        false,
    )?;
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new(if projected {
            "test.direct-custom-patch-v2"
        } else {
            "test.direct-custom-patch-v1"
        })?)
        .semantic_version(ProfileSemanticVersion::new(1, 0, 0))
        .provenance(HarnessProfileProvenance::Synthetic)
        .base_instructions("")
        .instruction_strategy(InstructionCompositionStrategy::BaseOnly)
        .tool_exposure_strategy(if projected {
            ToolExposureStrategy::None
        } else {
            ToolExposureStrategy::CanonicalAll
        })
        .reasoning_strategy(ReasoningPreferenceStrategy::Disabled)
        .continuation_strategy(ContinuationPreferenceStrategy::CanonicalReplayOnly)
        .cache_strategy(CachePreferenceStrategy::Disabled)
        .build()?;
    let plan = compile_explicit_runtime_profile(
        &profile,
        model.as_ref(),
        configured.configuration(),
        &registry,
        workspace.as_ref(),
        Vec::new(),
    )?;
    let mut builder = AgentBuilder::new(configured.clone(), registry.clone(), workspace);
    if projected {
        let target = composition_projection_target(&configured, &descriptor)?;
        let projection = CodingPresentation::Codex.projection_map(
            plakous_tools::JobConfig::default(),
            registry,
            target,
        )?;
        let admission: Arc<dyn ProjectionCandidateAdmission> = FakeProjectionAdmission::new(true);
        builder = builder
            .tool_projection(projection)
            .projection_admission(admission);
    }
    let builder = builder
        .harness_plan(plan)
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        );
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let agent = sessions
        .create("create:direct-custom-patch", builder)
        .await
        .map_err(|error| format!("direct agent construction failed: {}", error.message()))?;
    let mut turn = agent
        .start(CommandId::new("direct-custom-patch")?, "apply the patch")
        .await
        .map_err(|error| format!("direct turn start failed: {}", error.message()))?
        .into_turn()
        .ok_or("direct patch turn not admitted")?;
    let mut events = turn.take_events()?;
    let mut completed_patch = false;
    loop {
        let event = events.recv().await?;
        match event.kind() {
            AgentEventKind::ToolCompleted {
                identity,
                observation,
                ..
            } if identity.canonical_name().as_str() == "patch_envelope" => {
                completed_patch = observation
                    .as_ref()
                    .is_some_and(crate::ToolResultObservation::succeeded);
            }
            AgentEventKind::TurnCompleted { .. } => break,
            AgentEventKind::TurnFailed { .. } => return Err("direct patch turn failed".into()),
            _ => {}
        }
    }
    assert!(completed_patch, "canonical patch call did not succeed");
    let result = turn.result().await?;
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        std::fs::read(directory.path().join("direct.txt"))?,
        b"\xce\xbb \"quote\" \\ slash\n"
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn v2_direct_custom_patch_decodes_canonical_model_string() -> TestResult {
    direct_custom_patch_decodes_canonical_model_string_before_executor(true).await
}

#[tokio::test]
async fn v1_direct_utf8_patch_decodes_canonical_model_string() -> TestResult {
    direct_custom_patch_decodes_canonical_model_string_before_executor(false).await
}

#[tokio::test]
async fn a_rejected_nested_patch_object_does_not_fence_the_next_raw_patch() -> TestResult {
    let observed = observe_lineage_for_scenario(
        LineagePtcClass::Native,
        LineageScenario::PatchAfterRejectedObject,
    )
    .await?;
    assert!(
        observed
            .iter()
            .any(|event| { event.name == "patch_envelope" && event.phase == "completed" })
    );
    Ok(())
}

#[tokio::test]
async fn a_rejected_empty_nested_patch_does_not_leave_a_prepared_child() -> TestResult {
    let observed = observe_lineage_for_scenario(
        LineagePtcClass::Native,
        LineageScenario::PatchAfterRejectedEmpty,
    )
    .await?;
    assert!(
        observed
            .iter()
            .any(|event| { event.name == "patch_envelope" && event.phase == "completed" })
    );
    Ok(())
}

/// Every PTC class projects the same caller lineage onto its cell's tools.
///
/// Codex native `exec`, `exec_js`, and `exec_py` are all Plakous local
/// composition markers driven by the one private driver, so the lineage contract
/// requires the projection to be identical modulo the marker's name: the
/// outer marker owns the cell (`LocalProgrammatic`, no parent, no ordinal),
/// and every nested call names the owning marker call, the cell, a
/// one-based acceptance ordinal, and depth 1.
#[tokio::test]
async fn every_ptc_class_publishes_identical_tool_caller_lineage() -> TestResult {
    let test = async {
        let mut per_class = Vec::new();
        for class in [
            LineagePtcClass::Native,
            LineagePtcClass::JavaScript,
            LineagePtcClass::Python,
        ] {
            let observed = observe_lineage_for_class(class).await?;

            let outer_started = observed
                .iter()
                .find(|item| item.name == "<marker>" && item.phase == "started")
                .ok_or("the outer marker published no started observation")?;
            assert_eq!(
                outer_started.class,
                ToolCallerClass::LocalProgrammatic,
                "{} outer marker reported a non-composition class",
                class.label()
            );
            assert_eq!(outer_started.parent_is_outer_marker, None);
            assert_eq!(outer_started.nested_ordinal, None);
            assert_eq!(outer_started.depth, None);

            let nested = observed
                .iter()
                .filter(|item| item.name == LINEAGE_NESTED_TOOL && item.phase == "started")
                .collect::<Vec<_>>();
            assert_eq!(
                nested.len(),
                LINEAGE_NESTED_CALLS,
                "{} published {} nested started observations",
                class.label(),
                nested.len()
            );
            for (index, item) in nested.iter().enumerate() {
                assert_eq!(item.class, ToolCallerClass::LocalProgrammatic);
                assert_eq!(item.cell_id.as_deref(), Some(LINEAGE_CELL_ID));
                assert_eq!(item.parent_is_outer_marker, Some(true));
                assert_eq!(
                    item.nested_ordinal,
                    u32::try_from(index + 1).ok(),
                    "{} nested ordinals are not a one-based acceptance sequence",
                    class.label()
                );
                assert_eq!(item.depth, Some(1));
            }
            assert!(
                observed
                    .iter()
                    .filter(|item| item.name == LINEAGE_NESTED_TOOL && item.phase == "progress")
                    .all(|item| item.parent_is_outer_marker == Some(true)
                        && item.cell_id.as_deref() == Some(LINEAGE_CELL_ID)
                        && item.depth == Some(1)),
                "{} nested progress lost its caller lineage",
                class.label()
            );
            // The marker's own terminal observation stays attributable: it
            // is the row a consumer folds children into.
            let outer_completed = observed
                .iter()
                .find(|item| item.name == "<marker>" && item.phase == "completed")
                .ok_or("the outer marker published no completed observation")?;
            assert_eq!(outer_completed.class, ToolCallerClass::LocalProgrammatic);
            assert_eq!(outer_completed.parent_is_outer_marker, None);
            assert_eq!(outer_completed.nested_ordinal, None);
            // the marker owns the cell, so its terminal names
            // the cell it closed even though the driver has already retired
            // it. Without this a `wait*` continuation's terminal — the only
            // event that says which cell finished — carries no cell at all.
            assert_eq!(
                outer_completed.cell_id.as_deref(),
                Some(LINEAGE_CELL_ID),
                "{} outer marker terminal lost its cell",
                class.label()
            );
            // Nested read-only children bypass the group scheduler; without
            // their own terminal observation a consumer sees started
            // children that never end, and any per-cell tally (
            // Batch counts) is silently short.
            let nested_completed = observed
                .iter()
                .filter(|item| item.name == LINEAGE_NESTED_TOOL && item.phase == "completed")
                .collect::<Vec<_>>();
            assert_eq!(
                nested_completed.len(),
                LINEAGE_NESTED_CALLS,
                "{} published {} nested completed observations",
                class.label(),
                nested_completed.len()
            );
            for (index, item) in nested_completed.iter().enumerate() {
                assert_eq!(item.class, ToolCallerClass::LocalProgrammatic);
                assert_eq!(item.cell_id.as_deref(), Some(LINEAGE_CELL_ID));
                assert_eq!(item.parent_is_outer_marker, Some(true));
                assert_eq!(item.nested_ordinal, u32::try_from(index + 1).ok());
                assert_eq!(item.depth, Some(1));
            }
            per_class.push((class, observed));
        }
        let (first_class, first) = &per_class[0];
        for (class, observed) in per_class.iter().skip(1) {
            assert_eq!(
                observed,
                first,
                "{} lineage differs from {} beyond the marker name",
                class.label(),
                first_class.label()
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    Box::pin(tokio::time::timeout(Duration::from_secs(60), test)).await??;
    Ok(())
}
