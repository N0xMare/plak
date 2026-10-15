use super::*;

pub(super) type PrepareContextObservation = (
    AttemptId,
    Option<ModelCacheAffinity>,
    Option<ActiveFamilyArtifact>,
);

pub(super) struct RestoreDecisionPolicy(ToolPolicyRevision);

impl RestoreDecisionPolicy {
    pub(super) fn new() -> Result<Self, ToolRuntimeError> {
        ToolPolicyRevision::new("test.restore-decision", 1).map(Self)
    }
}

impl ToolPolicy for RestoreDecisionPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        self.0.clone()
    }

    fn evaluate(&self, _request: ToolPolicyRequest) -> ToolPolicyFuture {
        Box::pin(async {
            Ok(ToolPolicyDecision::DecisionRequired(
                EffectDecisionPrompt::new("restore probe decision"),
            ))
        })
    }
}

pub(super) fn assert_exact_terminal_result(
    result: &TurnResult,
    command_id: &CommandId,
) -> Result<(), Box<dyn std::error::Error>> {
    let terminal = terminal_record_at_checkpoint(result.checkpoint(), command_id)?;
    assert_eq!(terminal.turn_id, result.turn_id());
    assert_eq!(terminal.checkpoint_id, result.checkpoint_id());
    assert_eq!(
        terminal.conversation_identity,
        result
            .checkpoint()
            .manifest()
            .canonical()
            .history()
            .identity()
    );
    assert!(matches!(
        (&terminal.outcome, result.outcome()),
        (
            DurableTerminalOutcome::Completed(_),
            TurnResultOutcome::Completed(_)
        ) | (
            DurableTerminalOutcome::Cancelled(_),
            TurnResultOutcome::Cancelled(_)
        ) | (
            DurableTerminalOutcome::Failed(_),
            TurnResultOutcome::Failed(_)
        ) | (
            DurableTerminalOutcome::RecoveryRequired(_),
            TurnResultOutcome::RecoveryRequired(_)
        )
    ));
    Ok(())
}

fn native_fixture_error(error: impl fmt::Display) -> ModelError {
    ModelError::new(ModelErrorKind::InvalidEvent, error.to_string())
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "the private poison fixture exposes independent orthogonal failure switches to prove Driver fences"
)]
pub(super) struct ArtifactModel {
    pub(super) descriptor: ResolvedModelDescriptor,
    pub(super) authority: RuntimeModelAuthority,
    pub(super) dispatch_authority: Option<RuntimeModelAuthority>,
    pub(super) authority_checks: AtomicUsize,
    pub(super) candidate_calls: usize,
    pub(super) native_final_family: Option<&'static str>,
    pub(super) refuse_scheduling_measurement: AtomicBool,
    pub(super) operation_roots: Mutex<Vec<ModelOperationRoot>>,
    pub(super) prepares: AtomicUsize,
    pub(super) prepared_attempts: Mutex<BTreeSet<AttemptId>>,
    pub(super) prepared_records: Mutex<Vec<PreparedCallRecord>>,
    pub(super) calls: AtomicUsize,
    pub(super) observed: Mutex<Vec<Option<ActiveFamilyArtifact>>>,
    pub(super) observed_affinities: Mutex<Vec<Option<ModelCacheAffinity>>>,
    pub(super) observed_prepare_contexts: Mutex<Vec<PrepareContextObservation>>,
    pub(super) observed_request_texts: Mutex<Vec<Vec<String>>>,
    pub(super) produced_affinities: Mutex<Vec<Option<Digest>>>,
    pub(super) dispatch_probe: Option<Arc<AtomicBool>>,
    pub(super) entry_base: u128,
    pub(super) tamper_coverage: bool,
    pub(super) tamper_affinity: bool,
    pub(super) tamper_run: bool,
    pub(super) tool_call_on: Option<usize>,
    pub(super) tool_owner: ToolExecutionOwner,
    pub(super) tool_correlation: ToolCorrelation,
    pub(super) secondary_tool_correlation: Option<ToolCorrelation>,
    pub(super) secondary_tool_name: Option<String>,
    pub(super) secondary_tool_on: Option<usize>,
    pub(super) scripted_tool_calls: BTreeMap<usize, (String, String)>,
    pub(super) script_tools_by_ordinary_call: AtomicBool,
    pub(super) ordinary_calls: AtomicUsize,
    pub(super) reject_round_dispatch: AtomicBool,
    pub(super) provider_pause_calls: BTreeSet<usize>,
    pub(super) assistant_continuation_calls: Mutex<BTreeSet<usize>>,
    pub(super) report_usage: bool,
    /// Limits `report_usage` to these calls; `None` reports on every call.
    pub(super) usage_calls: Option<BTreeSet<usize>>,
    /// Estimates of every prepared request, in order, flagged when the
    /// request was a compaction's own.
    pub(super) prepared_estimates: Mutex<Vec<(bool, Option<u64>)>>,
    pub(super) compaction_wire_estimates: Mutex<Vec<u64>>,
    pub(super) terminal_only: bool,
    pub(super) duplicate_terminal: bool,
    pub(super) panic_call: bool,
    pub(super) join_cancellation: Option<Arc<AtomicBool>>,
    pub(super) block_call_after: Option<usize>,
    pub(super) held_call: AtomicUsize,
    pub(super) held_call_as_stream: AtomicBool,
    pub(super) held_call_stream_polled: AtomicBool,
    pub(super) held_call_joined: AtomicBool,
    pub(super) held_call_started: Notify,
    pub(super) held_call_release: Mutex<Option<Arc<Notify>>>,
    pub(super) context_tokens_from_prepare: Option<(usize, u64)>,
    pub(super) context_growth_ends_at_fold: AtomicBool,
    /// Counts the request's own history bytes instead of a fixed 64.
    ///
    /// The placeholder rung shrinks canonical history, so a fixture whose count
    /// ignores content cannot observe it working. `ceil(bytes/4)` is the same
    /// upper bound the runtime's own meters use.
    pub(super) byte_derived_context_tokens: bool,
    pub(super) context_token_multiplier: AtomicUsize,
    /// Exercises a summary-only envelope tighter than ordinary round input.
    pub(super) summary_context_overhead: AtomicUsize,
    pub(super) ordinary_context_overhead: AtomicUsize,
    pub(super) tool_successor_capacity_loss: AtomicUsize,
    pub(super) capacity_loss_after_fold: AtomicBool,
    /// Refuses to prepare the compaction's own request.
    ///
    /// The live Grok and Codex growth cases failed exactly here: an active
    /// Responses artifact no longer matched the request's canonical coverage,
    /// so the compaction could not be prepared at all.
    pub(super) fail_compaction_prepare: AtomicBool,
    pub(super) opaque_tool_history: AtomicBool,
    pub(super) tiny_reasoning_history: AtomicBool,
    pub(super) artifact_gated_reasoning_bytes: AtomicUsize,
    pub(super) gated_fresh_wires: Mutex<Vec<(Digest, Digest, bool)>>,
    pub(super) refuse_native_until_round: AtomicUsize,
    pub(super) opaque_repeats: AtomicUsize,
    pub(super) native_protected_prepare: AtomicBool,
    pub(super) allow_unbound_opaque: AtomicBool,
    pub(super) native_protected_history_refusals: AtomicUsize,
    pub(super) native_protected_refusal_attempts: Mutex<Vec<RequestMetadata>>,
    pub(super) paid_compaction_calls: AtomicUsize,
    /// Dispatches the compaction's own request, then fails it definitely.
    pub(super) fail_compaction_call: AtomicBool,
    pub(super) fail_first_compaction_call: AtomicBool,
    pub(super) fail_compaction_calls: Mutex<BTreeSet<usize>>,
    pub(super) reject_compaction_calls: Mutex<BTreeSet<usize>>,
    pub(super) compaction_starts: AtomicUsize,
    pub(super) compaction_start_rounds: Mutex<Vec<usize>>,
    pub(super) paid_compaction_rounds: Mutex<Vec<usize>>,
    /// Makes a provider-successful compaction result fail the net-shrink proof.
    pub(super) compaction_response_bytes: AtomicUsize,
    pub(super) empty_refusal: bool,
    pub(super) response_provenance: bool,
    /// Reports this served model instead of the requested one.
    pub(super) served_model: Mutex<Option<ModelId>>,
    /// Whether each ordinary request was prepared with a request anchor.
    pub(super) observed_request_anchors: Mutex<Vec<bool>>,
    pub(super) observed_provenance: Mutex<Vec<(ValidatedRequestPlan, ModelPreparationContext)>>,
    /// Preparation contexts for the compaction's own request.
    ///
    /// Captured unconditionally, because the pin on what a compaction may
    /// attach has to hold for every fixture, not only the ones that opt in.
    pub(super) observed_compaction_contexts: Mutex<Vec<ModelPreparationContext>>,
    pub(super) observed_compaction_anchors: Mutex<Vec<bool>>,
    pub(super) ttft_probe: bool,
    pub(super) ttft_content_delay: Duration,
    /// Calls the provider refuses as over its context window.
    pub(super) overflow_calls: BTreeSet<usize>,
    pub(super) overflow_from_call: AtomicUsize,
    pub(super) overflow_once_at_call: AtomicUsize,
    pub(super) foreign_overflow: AtomicBool,
    pub(super) item_context: AtomicBool,
    pub(super) item_system_bytes: AtomicUsize,
    pub(super) item_provider_input_tokens: AtomicUsize,
    pub(super) usage_suppressed_until: AtomicUsize,
}

impl ArtifactModel {
    pub(super) fn new(
        descriptor: ResolvedModelDescriptor,
        authority: RuntimeModelAuthority,
        candidate_calls: usize,
        entry_base: u128,
        tamper_coverage: bool,
    ) -> Self {
        Self {
            descriptor,
            authority,
            dispatch_authority: None,
            authority_checks: AtomicUsize::new(0),
            candidate_calls,
            native_final_family: None,
            refuse_scheduling_measurement: AtomicBool::new(false),
            operation_roots: Mutex::new(Vec::new()),
            prepares: AtomicUsize::new(0),
            prepared_attempts: Mutex::new(BTreeSet::new()),
            prepared_records: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            observed: Mutex::new(Vec::new()),
            observed_affinities: Mutex::new(Vec::new()),
            observed_prepare_contexts: Mutex::new(Vec::new()),
            observed_request_texts: Mutex::new(Vec::new()),
            produced_affinities: Mutex::new(Vec::new()),
            dispatch_probe: None,
            entry_base,
            tamper_coverage,
            tamper_affinity: false,
            tamper_run: false,
            tool_call_on: None,
            tool_owner: ToolExecutionOwner::Local,
            tool_correlation: ToolCorrelation::default(),
            secondary_tool_correlation: None,
            secondary_tool_name: None,
            secondary_tool_on: None,
            scripted_tool_calls: BTreeMap::new(),
            script_tools_by_ordinary_call: AtomicBool::new(false),
            ordinary_calls: AtomicUsize::new(0),
            reject_round_dispatch: AtomicBool::new(false),
            provider_pause_calls: BTreeSet::new(),
            assistant_continuation_calls: Mutex::new(BTreeSet::new()),
            report_usage: false,
            usage_calls: None,
            prepared_estimates: Mutex::new(Vec::new()),
            compaction_wire_estimates: Mutex::new(Vec::new()),
            terminal_only: false,
            duplicate_terminal: false,
            panic_call: false,
            join_cancellation: None,
            block_call_after: None,
            held_call: AtomicUsize::new(usize::MAX),
            held_call_as_stream: AtomicBool::new(false),
            held_call_stream_polled: AtomicBool::new(false),
            held_call_joined: AtomicBool::new(false),
            held_call_started: Notify::new(),
            held_call_release: Mutex::new(None),
            context_tokens_from_prepare: None,
            context_growth_ends_at_fold: AtomicBool::new(false),
            byte_derived_context_tokens: false,
            context_token_multiplier: AtomicUsize::new(1),
            summary_context_overhead: AtomicUsize::new(0),
            ordinary_context_overhead: AtomicUsize::new(0),
            tool_successor_capacity_loss: AtomicUsize::new(0),
            capacity_loss_after_fold: AtomicBool::new(false),
            fail_compaction_prepare: AtomicBool::new(false),
            opaque_tool_history: AtomicBool::new(false),
            tiny_reasoning_history: AtomicBool::new(false),
            artifact_gated_reasoning_bytes: AtomicUsize::new(0),
            gated_fresh_wires: Mutex::new(Vec::new()),
            refuse_native_until_round: AtomicUsize::new(0),
            opaque_repeats: AtomicUsize::new(512),
            native_protected_prepare: AtomicBool::new(false),
            allow_unbound_opaque: AtomicBool::new(false),
            native_protected_history_refusals: AtomicUsize::new(0),
            native_protected_refusal_attempts: Mutex::new(Vec::new()),
            paid_compaction_calls: AtomicUsize::new(0),
            fail_compaction_call: AtomicBool::new(false),
            fail_first_compaction_call: AtomicBool::new(false),
            fail_compaction_calls: Mutex::new(BTreeSet::new()),
            reject_compaction_calls: Mutex::new(BTreeSet::new()),
            compaction_starts: AtomicUsize::new(0),
            compaction_start_rounds: Mutex::new(Vec::new()),
            paid_compaction_rounds: Mutex::new(Vec::new()),
            compaction_response_bytes: AtomicUsize::new(0),
            empty_refusal: false,
            response_provenance: false,
            served_model: Mutex::new(None),
            observed_request_anchors: Mutex::new(Vec::new()),
            observed_provenance: Mutex::new(Vec::new()),
            observed_compaction_contexts: Mutex::new(Vec::new()),
            observed_compaction_anchors: Mutex::new(Vec::new()),
            ttft_probe: false,
            ttft_content_delay: Duration::from_millis(50),
            overflow_calls: BTreeSet::new(),
            overflow_from_call: AtomicUsize::new(usize::MAX),
            overflow_once_at_call: AtomicUsize::new(usize::MAX),
            foreign_overflow: AtomicBool::new(false),
            item_context: AtomicBool::new(false),
            item_system_bytes: AtomicUsize::new(0),
            item_provider_input_tokens: AtomicUsize::new(0),
            usage_suppressed_until: AtomicUsize::new(0),
        }
    }

    pub(super) fn with_provider_overflows(
        mut self,
        calls: impl IntoIterator<Item = usize>,
    ) -> Self {
        self.overflow_calls.extend(calls);
        self
    }

    pub(super) fn with_ttft_probe(mut self, content_delay: Duration) -> Self {
        self.ttft_probe = true;
        self.ttft_content_delay = content_delay;
        self
    }

    pub(super) fn with_dispatch_authority(mut self, authority: RuntimeModelAuthority) -> Self {
        self.dispatch_authority = Some(authority);
        self
    }

    pub(super) fn with_tool_first(mut self) -> Self {
        self.tool_call_on = Some(0);
        self
    }

    pub(super) fn with_programmatic_mcp_tool(mut self) -> TestResult<Self> {
        self.tool_owner = ToolExecutionOwner::Mcp {
            server: plakous_model::McpServerId::new("test_mcp")?,
        };
        self.tool_correlation = ToolCorrelation::new(
            Some("remote-call-1".to_owned()),
            Some("container-1".to_owned()),
            plakous_model::ToolCallerLineage::provider_programmatic(
                NamespaceId::new("test.program")?,
                "execution-1",
            )?,
        )?;
        Ok(self)
    }

    pub(super) fn with_programmatic_local_tool(mut self) -> TestResult<Self> {
        self.tool_correlation = ToolCorrelation::new(
            Some("remote-call-local".to_owned()),
            Some("container-1".to_owned()),
            plakous_model::ToolCallerLineage::provider_programmatic(
                NamespaceId::new("test.program")?,
                "execution-1",
            )?,
        )?;
        Ok(self)
    }

    pub(super) fn with_mixed_programmatic_and_direct_tools(mut self) -> TestResult<Self> {
        self.tool_correlation = ToolCorrelation::new(
            Some("remote-call-programmatic".to_owned()),
            Some("container-1".to_owned()),
            plakous_model::ToolCallerLineage::provider_programmatic(
                NamespaceId::new("test.program")?,
                "execution-1",
            )?,
        )?;
        self.secondary_tool_correlation = Some(ToolCorrelation::new(
            Some("remote-call-direct".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?);
        Ok(self)
    }

    pub(super) fn with_second_direct_tool(mut self) -> TestResult<Self> {
        self.secondary_tool_correlation = Some(ToolCorrelation::new(
            Some("remote-call-second".to_owned()),
            None,
            plakous_model::ToolCallerLineage::Direct,
        )?);
        Ok(self)
    }

    pub(super) fn with_second_direct_tool_named(mut self, name: &str) -> TestResult<Self> {
        self = self.with_second_direct_tool()?;
        self.secondary_tool_name = Some(name.to_owned());
        Ok(self)
    }

    pub(super) fn with_tool_call_on(mut self, call: usize) -> Self {
        self.tool_call_on = Some(call);
        self
    }

    pub(super) fn with_scripted_tool_calls(
        mut self,
        calls: impl IntoIterator<Item = (usize, &'static str, String)>,
    ) -> Self {
        self.scripted_tool_calls = calls
            .into_iter()
            .map(|(round, name, arguments)| (round, (name.to_owned(), arguments)))
            .collect();
        self
    }

    pub(super) fn with_provider_pauses(mut self, calls: impl IntoIterator<Item = usize>) -> Self {
        self.provider_pause_calls = calls.into_iter().collect();
        self
    }

    pub(super) fn with_assistant_continuations(
        self,
        calls: impl IntoIterator<Item = usize>,
    ) -> Self {
        *lock_mutex(&self.assistant_continuation_calls) = calls.into_iter().collect();
        self
    }

    pub(super) fn with_report_usage(mut self) -> Self {
        self.report_usage = true;
        self
    }

    pub(super) fn with_usage_on_calls(mut self, calls: impl IntoIterator<Item = usize>) -> Self {
        self.report_usage = true;
        self.usage_calls = Some(calls.into_iter().collect());
        self
    }

    pub(super) fn with_tampered_affinity(mut self) -> Self {
        self.tamper_affinity = true;
        self
    }

    pub(super) fn with_dispatch_probe(mut self, probe: Arc<AtomicBool>) -> Self {
        self.dispatch_probe = Some(probe);
        self
    }

    pub(super) fn with_terminal_only(mut self, duplicate_terminal: bool) -> Self {
        self.terminal_only = true;
        self.duplicate_terminal = duplicate_terminal;
        self
    }

    pub(super) fn with_joined_cancellation(mut self, joined: Arc<AtomicBool>) -> Self {
        self.join_cancellation = Some(joined);
        self
    }

    pub(super) fn with_block_call_after(mut self, call: usize) -> Self {
        self.block_call_after = Some(call);
        self
    }

    pub(super) fn with_panicking_call(mut self) -> Self {
        self.panic_call = true;
        self
    }

    pub(super) fn with_context_tokens_from_prepare(mut self, prepare: usize, tokens: u64) -> Self {
        self.context_tokens_from_prepare = Some((prepare, tokens));
        self
    }

    pub(super) fn with_byte_derived_context_tokens(mut self) -> Self {
        self.byte_derived_context_tokens = true;
        self
    }

    pub(super) fn refuse_compaction_prepare(&self) {
        self.fail_compaction_prepare.store(true, Ordering::Release);
    }

    pub(super) fn fail_compaction_call(&self) {
        self.fail_compaction_call.store(true, Ordering::Release);
    }

    pub(super) fn observed(&self) -> Vec<Option<ActiveFamilyArtifact>> {
        lock_mutex(&self.observed).clone()
    }

    pub(super) fn observed_affinities(&self) -> Vec<Option<ModelCacheAffinity>> {
        lock_mutex(&self.observed_affinities).clone()
    }

    pub(super) fn observed_compaction_contexts(&self) -> Vec<ModelPreparationContext> {
        lock_mutex(&self.observed_compaction_contexts).clone()
    }

    pub(super) fn make_compaction_success_non_shrinking(&self, bytes: usize) {
        self.compaction_response_bytes
            .store(bytes, Ordering::Release);
    }

    pub(super) fn script_tool_rounds_around_compactions(&self) {
        self.script_tools_by_ordinary_call
            .store(true, Ordering::Release);
    }

    pub(super) fn observed_prepare_contexts(&self) -> Vec<PrepareContextObservation> {
        lock_mutex(&self.observed_prepare_contexts).clone()
    }

    pub(super) fn observed_request_texts(&self) -> Vec<Vec<String>> {
        lock_mutex(&self.observed_request_texts).clone()
    }

    pub(super) fn produced_affinities(&self) -> Vec<Option<Digest>> {
        lock_mutex(&self.produced_affinities).clone()
    }

    pub(super) fn prepared_records(&self) -> Vec<PreparedCallRecord> {
        lock_mutex(&self.prepared_records).clone()
    }
}

fn semantic_history_tokens(history: &CanonicalHistory) -> u64 {
    let bytes = history.segments().iter().fold(0usize, |total, segment| {
        total.saturating_add(segment.canonical_bytes().len())
    });
    u64::try_from(bytes).unwrap_or(u64::MAX).div_ceil(4).max(1)
}

/// The two session-workflow prepares the v2 shrink proof compares. Other
/// prepares, including the paid summary itself, keep the fixture's flat count.
fn shrink_proof_measurement(metadata: RequestMetadata) -> bool {
    let Some(workflow) = metadata.root().session_workflow_id() else {
        return false;
    };
    let attempt = metadata.attempt_id();
    [
        b"successor-measurement".as_slice(),
        b"source-proof-measurement".as_slice(),
    ]
    .into_iter()
    .any(|role| compaction_attempt_id(workflow, role).ok() == Some(attempt))
}

impl LanguageModel for ArtifactModel {
    fn begin_operation_root(
        self: Arc<Self>,
        root: ModelOperationRoot,
    ) -> Result<Box<dyn plakous_model::ModelOperationScope>, ModelPreparationError> {
        lock_mutex(&self.operation_roots).push(root);
        Ok(Box::new(scheduling_diagnostics::ArtifactOperationScope))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let check = self.authority_checks.fetch_add(1, Ordering::AcqRel);
        let authority = if check >= 2 {
            self.dispatch_authority.as_ref().unwrap_or(&self.authority)
        } else {
            &self.authority
        };
        let result = authority
            .clone()
            .validate(&self.descriptor, checked_at)
            .map(|()| authority.clone());
        Box::pin(async move { result })
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        if self.refuse_scheduling_measurement.load(Ordering::Acquire)
            && matches!(
                plan.request().metadata().root(),
                ModelOperationRoot::Turn(_)
            )
            && !lock_mutex(&self.operation_roots).contains(&plan.request().metadata().root())
        {
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::ResourceLimit,
                "optional scheduling measurement unavailable",
            ));
        }
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        if !self.authority.same_selected_authority(context.authority()) {
            return Err(plakous_model::RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        lock_mutex(&self.observed_request_texts).push(
            plan.request()
                .input()
                .items()
                .iter()
                .flat_map(ConversationItem::blocks)
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text().to_owned()),
                    _ => None,
                })
                .collect(),
        );
        if self.response_provenance {
            lock_mutex(&self.observed_provenance).push((plan.clone(), context.clone()));
        }
        let compaction_request = plan.request().input().items().iter().any(|item| {
            item.blocks().iter().any(|block| {
                matches!(block, ContentBlock::Text(text)
                    if text.text() == plakous_model::SEMANTIC_COMPACTION_INSTRUCTION)
            })
        });
        let native_compaction_namespace =
            NamespaceId::new("plakous.compaction").expect("fixture compaction namespace is valid");
        let native_compaction_request = plan
            .request()
            .stable_prefix()
            .native()
            .entries()
            .get(&native_compaction_namespace)
            .is_some_and(|value| value == "native_responses_trigger_v1");
        let paid_compaction_request = compaction_request || native_compaction_request;
        if paid_compaction_request {
            lock_mutex(&self.observed_compaction_contexts).push(context.clone());
        }
        if native_compaction_request
            && self.ordinary_calls.load(Ordering::Acquire)
                < self.refuse_native_until_round.load(Ordering::Acquire)
        {
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "first pressure native input is not admitted",
            ));
        }
        if native_compaction_request
            && self.native_protected_prepare.load(Ordering::Acquire)
            && plan.request().input().items().iter().any(|item| {
                item.blocks().iter().any(|block| {
                    matches!(block, ContentBlock::Opaque(_) | ContentBlock::Reasoning(_))
                })
            })
        {
            self.native_protected_history_refusals
                .fetch_add(1, Ordering::AcqRel);
            lock_mutex(&self.native_protected_refusal_attempts).push(plan.request().metadata());
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::ProtectedHistory,
                "native replay cannot be bound",
            ));
        }
        if self.fail_compaction_prepare.load(Ordering::Acquire) && paid_compaction_request {
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "active Responses artifact is not reusable for this compaction",
            ));
        }
        if self.opaque_tool_history.load(Ordering::Acquire)
            && !self.allow_unbound_opaque.load(Ordering::Acquire)
            && context.active_family_artifact().is_none()
            && plan.request().input().items().iter().any(|item| {
                item.blocks()
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Opaque(_)))
            })
        {
            if native_compaction_request {
                self.native_protected_history_refusals
                    .fetch_add(1, Ordering::AcqRel);
                lock_mutex(&self.native_protected_refusal_attempts).push(plan.request().metadata());
            }
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::ProtectedHistory,
                "unbound opaque Code Mode replay",
            ));
        }
        let prepare = self.prepares.fetch_add(1, Ordering::AcqRel);
        if !lock_mutex(&self.prepared_attempts).insert(plan.request().metadata().attempt_id()) {
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "artifact fixture poisoned a duplicate lowering for one attempt",
            ));
        }
        let active_artifact = context.active_family_artifact().cloned();
        lock_mutex(&self.observed).push(active_artifact.clone());
        lock_mutex(&self.observed_affinities).push(context.cache_affinity().cloned());
        lock_mutex(&self.observed_prepare_contexts).push((
            plan.request().metadata().attempt_id(),
            context.cache_affinity().cloned(),
            active_artifact.clone(),
        ));
        let active_artifact_integrity = active_artifact
            .as_ref()
            .map(ActiveFamilyArtifact::integrity);
        let gated_replay = self.artifact_gated_reasoning_bytes.load(Ordering::Acquire) > 0;
        let lowered_items = plan
            .request()
            .input()
            .items()
            .iter()
            .map(|item| {
                let blocks = item
                    .blocks()
                    .iter()
                    .filter(|block| {
                        !(gated_replay
                            && active_artifact.is_none()
                            && matches!(block, ContentBlock::Reasoning(_)))
                    })
                    .collect::<Vec<_>>();
                let role = match item.actor() {
                    Actor::Assistant => "assistant",
                    Actor::Tool => "tool",
                    _ => "user",
                };
                serde_json::json!({"role": role, "content": blocks})
            })
            .filter(|item| !item["content"].as_array().is_some_and(Vec::is_empty))
            .collect::<Vec<_>>();
        let lowered_bytes =
            serde_json::to_vec(&lowered_items).map_err(|_| PreparedCallError::Serialization)?;
        let plan_digest = plan.digest();
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[
                b"plakous-agent/artifact-model-wire-v1",
                if gated_replay {
                    lowered_bytes.as_slice()
                } else {
                    plan_digest.bytes()
                },
                active_artifact_integrity
                    .as_ref()
                    .map_or(&[][..], |integrity| integrity.bytes().as_slice()),
            ],
        )
        .map_err(PreparedCallError::from)?;
        if gated_replay && active_artifact.is_none() {
            lock_mutex(&self.gated_fresh_wires).push((
                plan.request().input().history().identity(),
                wire_digest,
                plan.request().metadata().turn_id().is_some(),
            ));
        }
        let wire_size = u64::try_from(wire_digest.bytes().len())
            .map_err(|_| PreparedCallError::InvalidWireSize)?;
        let mut counted_tokens = if self.byte_derived_context_tokens {
            u64::try_from(
                plan.request()
                    .input()
                    .history()
                    .retained_bytes_upper_bound(),
            )
            .unwrap_or(u64::MAX)
            .div_ceil(4)
        } else if shrink_proof_measurement(plan.request().metadata()) {
            // The publication proof re-measures source and successor. Count
            // canonical history bytes so a fold has to shrink real content.
            // Turn rounds and the paid summary keep the fixture's flat count.
            semantic_history_tokens(plan.request().input().history())
        } else {
            let folded = self.context_growth_ends_at_fold.load(Ordering::Acquire)
                && !lock_mutex(&self.observed_compaction_contexts).is_empty();
            self.context_tokens_from_prepare
                .map_or(64, |(from, tokens)| {
                    if prepare >= from && !folded {
                        tokens
                    } else {
                        64
                    }
                })
        };
        if gated_replay {
            counted_tokens = (lowered_bytes.len() as u64).div_ceil(4);
        }
        counted_tokens = counted_tokens
            .saturating_mul(self.context_token_multiplier.load(Ordering::Acquire) as u64);
        if compaction_request {
            counted_tokens = counted_tokens
                .saturating_add(self.summary_context_overhead.load(Ordering::Acquire) as u64);
        }
        if !compaction_request {
            counted_tokens = counted_tokens
                .saturating_add(self.ordinary_context_overhead.load(Ordering::Acquire) as u64);
        }
        let successor_capacity_loss = if !compaction_request
            && plan.request().input().items().iter().any(|item| {
                item.blocks()
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult(_)))
            })
            && (!self.capacity_loss_after_fold.load(Ordering::Acquire)
                || !lock_mutex(&self.observed_compaction_contexts).is_empty())
        {
            self.tool_successor_capacity_loss.load(Ordering::Acquire)
        } else {
            0
        };
        counted_tokens = counted_tokens.saturating_add(successor_capacity_loss as u64);
        // A fixture that sizes its context also reports that size as the
        // provider's input count, so a round anchors the measure the way a
        // real provider's usage does.
        let reported_input = self.item_provider_input_tokens.load(Ordering::Acquire);
        let counted_usage = (self.byte_derived_context_tokens
            || self.context_tokens_from_prepare.is_some())
        .then(|| {
            Usage::new(
                if reported_input > 0 {
                    reported_input as u64
                } else {
                    counted_tokens
                },
                2,
                0,
                0,
                0,
            )
        });
        // The byte parts stand in for a real lowered body sized to the count,
        // so the bytes/4 estimate the measure anchors on tracks it.
        let parts_bytes = usize::try_from(counted_tokens.saturating_mul(4)).unwrap_or(usize::MAX);
        let parts_body = format!(r#"{{"input":"{}"}}"#, "x".repeat(parts_bytes));
        let mut token_projection = PreparedTokenProjection::new(
            wire_digest,
            wire_size,
            Some(
                CountObservation::new(
                    "agent-scripted-counter",
                    "agent-scripted-exact-v1",
                    counted_tokens,
                )
                .expect("valid artifact fixture count"),
            ),
        )?
        .with_input_parts(plakous_model::ContextInputParts::measure_wire(
            parts_body.as_bytes(),
        ));
        if self.item_context.load(Ordering::Acquire) {
            let messages = lowered_items;
            let body = serde_json::to_vec(&serde_json::json!({"system": "x".repeat(self.item_system_bytes.load(Ordering::Acquire).saturating_add(successor_capacity_loss.saturating_mul(4)).saturating_add(if compaction_request { self.summary_context_overhead.load(Ordering::Acquire) } else { 0 })), "messages": messages}))
                .map_err(|_| PreparedCallError::Serialization)?;
            if compaction_request {
                lock_mutex(&self.compaction_wire_estimates).push(
                    plakous_model::ContextInputParts::measure_wire(&body)
                        .expect("fixture wire input parts")
                        .occupancy_tokens(),
                );
            }
            token_projection = token_projection.with_wire_input(
                &body,
                Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
                &context,
            )?;
        }
        if paid_compaction_request {
            lock_mutex(&self.observed_compaction_anchors)
                .push(token_projection.request_anchor().is_some());
        } else if !compaction_request {
            lock_mutex(&self.observed_request_anchors)
                .push(token_projection.request_anchor().is_some());
        }
        lock_mutex(&self.prepared_estimates).push((
            compaction_request,
            token_projection
                .input_parts()
                .map(plakous_model::ContextInputParts::occupancy_tokens),
        ));
        let output_limit = Some(PreparedOutputLimit::enforced_wire_maximum(
            NonZeroU32::new(128).expect("nonzero artifact output cap"),
            "max_output_tokens",
            wire_digest,
            "artifact-lowering-v1",
        )?);
        let cache_use = active_artifact
            .as_ref()
            .map_or(PreparedCacheUse::None, |artifact| {
                PreparedCacheUse::ActiveFamilyArtifact {
                    artifact: artifact.integrity(),
                }
            });
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "artifact-lowering-v1",
            "artifact-projection-v1",
            "artifact-accounting-v1",
            wire_digest,
            wire_size,
            token_projection,
            cache_use,
            output_limit,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        let cache_affinity = context.cache_affinity().map(ModelCacheAffinity::identity);
        lock_mutex(&self.prepared_records).push(record.clone());
        let failing_compaction =
            paid_compaction_request && self.fail_compaction_call.load(Ordering::Acquire);
        let rejection = ModelError::new(ModelErrorKind::ResourceLimit, "local admission rejected")
            .with_dispatch_rejection(plakous_model::ModelDispatchRejection::for_prepared_call(
                plakous_model::ModelDispatchRejectionClass::Admission,
                plakous_model::ModelDispatchRejectionPhase::Revalidation,
                record.identity(),
                plan.request().metadata().attempt_id(),
            ))
            .expect("exact dispatch rejection proof");
        Ok(PreparedModelCall::new(record, move |call_context| {
            assert!(!self.panic_call, "injected active model panic");
            let call = self.calls.fetch_add(1, Ordering::AcqRel);
            if !compaction_request && self.reject_round_dispatch.load(Ordering::Acquire) {
                return Box::pin(async move { Err(rejection) });
            }
            let compaction_call = if paid_compaction_request {
                let start = self.compaction_starts.fetch_add(1, Ordering::AcqRel) + 1;
                lock_mutex(&self.compaction_start_rounds)
                    .push(self.ordinary_calls.load(Ordering::Acquire));
                if lock_mutex(&self.reject_compaction_calls).contains(&start) {
                    return Box::pin(async move { Err(rejection) });
                }
                start
            } else {
                0
            };
            if paid_compaction_request {
                self.paid_compaction_calls.fetch_add(1, Ordering::AcqRel);
                lock_mutex(&self.paid_compaction_rounds)
                    .push(self.ordinary_calls.load(Ordering::Acquire));
            }
            if failing_compaction
                || lock_mutex(&self.fail_compaction_calls).contains(&compaction_call)
                || paid_compaction_request
                    && self
                        .fail_first_compaction_call
                        .swap(false, Ordering::AcqRel)
            {
                return Box::pin(async {
                    Err(ModelError::new(
                        ModelErrorKind::RateLimited,
                        "provider rate limited the compaction",
                    ))
                });
            }
            if self.overflow_calls.contains(&call)
                || call >= self.overflow_from_call.load(Ordering::Acquire)
                || self
                    .overflow_once_at_call
                    .compare_exchange(call, usize::MAX, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                let mut error = ModelError::new(
                    ModelErrorKind::ContextLimit,
                    "prompt is too long: 270000 tokens > 256000 maximum",
                );
                if self.foreign_overflow.load(Ordering::Acquire) {
                    let target = plakous_model::ModelErrorTarget::new(
                        self.descriptor.protocol().clone(),
                        RouteId::new("foreign.route").expect("route"),
                        ModelId::new("foreign-model").expect("model"),
                    )
                    .expect("target");
                    error
                        .attach_context(
                            target.unknown_context(plakous_model::ModelErrorPhase::FirstEvent),
                        )
                        .expect("foreign context");
                }
                return Box::pin(async move { Err(error) });
            }
            if self.held_call.load(Ordering::Acquire) == call
                && lock_mutex(&self.held_call_release).is_none()
            {
                self.held_call_started.notify_one();
                return Box::pin(async move {
                    if self.held_call_as_stream.load(Ordering::Acquire) {
                        Ok(Box::pin(futures::stream::once(async move {
                            call_context.cancellation().cancelled().await;
                            self.held_call_joined.store(true, Ordering::Release);
                            Err(ModelError::cancelled())
                        })) as ModelStream)
                    } else {
                        call_context.cancellation().cancelled().await;
                        self.held_call_joined.store(true, Ordering::Release);
                        Err(ModelError::cancelled())
                    }
                });
            }
            if self.block_call_after.is_some_and(|blocked| call >= blocked) {
                return Box::pin(async move {
                    call_context.cancellation().cancelled().await;
                    Err(ModelError::cancelled())
                });
            }
            if self
                .dispatch_probe
                .as_ref()
                .is_some_and(|probe| !probe.load(Ordering::Acquire))
            {
                return Box::pin(async {
                    Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        "prepared call started before its Dispatched checkpoint",
                    ))
                });
            }
            if let Some(joined) = self.join_cancellation.clone() {
                return Box::pin(async move {
                    call_context.cancellation().cancelled().await;
                    joined.store(true, Ordering::Release);
                    Err(ModelError::cancelled())
                });
            }
            Box::pin(async move {
                let release = lock_mutex(&self.held_call_release).clone();
                let hold_stream = self.held_call.load(Ordering::Acquire) == call
                    && self.held_call_as_stream.load(Ordering::Acquire);
                if self.held_call.load(Ordering::Acquire) == call
                    && !hold_stream
                    && let Some(release) = release.clone()
                {
                    self.held_call_started.notify_one();
                    tokio::select! {
                        () = call_context.cancellation().cancelled() => {
                            self.held_call_joined.store(true, Ordering::Release);
                            return Err(ModelError::cancelled());
                        }
                        () = release.notified() => {}
                    }
                }
                let entry_offset = u128::try_from(call).map_err(|_| {
                    ModelError::new(ModelErrorKind::InvalidEvent, "artifact fixture overflowed")
                })?;
                let scripted_call = if self.script_tools_by_ordinary_call.load(Ordering::Acquire)
                    && !paid_compaction_request
                {
                    self.ordinary_calls.fetch_add(1, Ordering::AcqRel)
                } else {
                    call
                };
                let scripted = (!paid_compaction_request)
                    .then(|| self.scripted_tool_calls.get(&scripted_call))
                    .flatten();
                let tool_call = self.tool_call_on == Some(call) || scripted.is_some();
                let mut blocks = if tool_call {
                    let (name, arguments) = scripted.map_or(("probe", "{}"), |(name, args)| {
                        (name.as_str(), args.as_str())
                    });
                    vec![ContentBlock::ToolCall(
                        ToolCallBlock::new(
                            CallId::try_from(uuid::Uuid::from_u128(
                                self.entry_base
                                    .saturating_add(10_000)
                                    .saturating_add(entry_offset),
                            ))
                            .map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture tool call is invalid",
                                )
                            })?,
                            ToolName::new(name).map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture tool name is invalid",
                                )
                            })?,
                            self.tool_owner.clone(),
                            ToolCallState::Requested,
                            self.tool_correlation.clone(),
                            arguments,
                        )
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture tool block is invalid",
                            )
                        })?,
                    )]
                } else {
                    let response = if paid_compaction_request {
                        let bytes = self.compaction_response_bytes.load(Ordering::Acquire);
                        if bytes == 0 {
                            format!("artifact response {call}")
                        } else {
                            "n".repeat(bytes)
                        }
                    } else {
                        format!("artifact response {call}")
                    };
                    vec![ContentBlock::Text(TextBlock::new(response, None).map_err(
                        |_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture text is invalid",
                            )
                        },
                    )?)]
                };
                if tool_call && self.opaque_tool_history.load(Ordering::Acquire) {
                    let replay = plakous_model::ReplayEnvelope::new(
                        ProtocolId::new("test.responses").expect("fixture protocol"),
                        None,
                        plakous_model::ReplayScope::Protocol,
                        plakous_model::ReplayKindId::new("code-mode-exec").expect("fixture kind"),
                        1,
                        plakous_model::DataClassification::Sensitive,
                        serde_json::value::RawValue::from_string(
                            json!({"type": "custom_tool_call", "encrypted_content": "fixture".repeat(self.opaque_repeats.load(Ordering::Acquire))})
                                .to_string(),
                        )
                        .expect("fixture replay"),
                    )
                    .expect("fixture envelope");
                    blocks.push(ContentBlock::Opaque(
                        plakous_model::OpaqueBlock::new(
                            replay,
                            plakous_model::OpaqueFallback::CompatibleReplay,
                        )
                        .expect("fixture opaque"),
                    ));
                }
                if tool_call && self.artifact_gated_reasoning_bytes.load(Ordering::Acquire) > 0 {
                    blocks.push(ContentBlock::Reasoning(
                        ReasoningBlock::new(
                            ReasoningVisibility::Visible,
                            Some("r".repeat(
                                self.artifact_gated_reasoning_bytes.load(Ordering::Acquire),
                            )),
                            None,
                        )
                        .expect("fixture artifact-gated reasoning"),
                    ));
                }
                if tool_call && self.tiny_reasoning_history.load(Ordering::Acquire) {
                    blocks.push(ContentBlock::Reasoning(
                        ReasoningBlock::new(
                            ReasoningVisibility::Visible,
                            Some("r".to_owned()),
                            None,
                        )
                        .expect("fixture tiny reasoning"),
                    ));
                }
                if tool_call && self.opaque_tool_history.load(Ordering::Acquire) {
                    blocks.push(ContentBlock::Reasoning(
                        ReasoningBlock::new(
                            ReasoningVisibility::Visible,
                            Some("fixture reasoning outside result selection".to_owned()),
                            None,
                        )
                        .expect("fixture reasoning"),
                    ));
                }
                if let Some(correlation) = &self.secondary_tool_correlation
                    && tool_call
                    && self.secondary_tool_on.is_none_or(|round| round == call)
                {
                    blocks.push(ContentBlock::ToolCall(
                        ToolCallBlock::new(
                            CallId::try_from(uuid::Uuid::from_u128(
                                self.entry_base
                                    .saturating_add(20_000)
                                    .saturating_add(entry_offset),
                            ))
                            .map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture secondary tool call is invalid",
                                )
                            })?,
                            ToolName::new(self.secondary_tool_name.as_deref().unwrap_or("probe"))
                                .map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture secondary tool name is invalid",
                                )
                            })?,
                            self.tool_owner.clone(),
                            ToolCallState::Requested,
                            correlation.clone(),
                            "{}",
                        )
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture secondary tool block is invalid",
                            )
                        })?,
                    ));
                }
                if !tool_call && let Some(family) = self.native_final_family {
                    let native = if family == "messages" {
                        json!({"type": "thinking", "thinking": "fixture thought", "signature": "fixture-signature"})
                    } else {
                        json!({"type": "reasoning", "encrypted_content": "fixture-encrypted-reasoning"})
                    };
                    let envelope = plakous_model::ReplayEnvelope::new(
                        ProtocolId::new(format!("test.{family}")).map_err(native_fixture_error)?,
                        None,
                        plakous_model::ReplayScope::Protocol,
                        plakous_model::ReplayKindId::new("thinking")
                            .map_err(native_fixture_error)?,
                        1,
                        plakous_model::DataClassification::Sensitive,
                        serde_json::value::RawValue::from_string(native.to_string())
                            .map_err(native_fixture_error)?,
                    )
                    .map_err(native_fixture_error)?;
                    blocks.insert(
                        0,
                        ContentBlock::Reasoning(
                            ReasoningBlock::new(
                                ReasoningVisibility::Visible,
                                Some("fixture thought".to_owned()),
                                Some(envelope.clone()),
                            )
                            .map_err(native_fixture_error)?,
                        ),
                    );
                    blocks.insert(
                        1,
                        ContentBlock::Opaque(
                            plakous_model::OpaqueBlock::new(
                                envelope,
                                plakous_model::OpaqueFallback::CompatibleReplay,
                            )
                            .map_err(native_fixture_error)?,
                        ),
                    );
                    if family == "responses" {
                        let native_text = plakous_model::ReplayEnvelope::new(
                            ProtocolId::new("test.responses").map_err(native_fixture_error)?,
                            None,
                            plakous_model::ReplayScope::Protocol,
                            plakous_model::ReplayKindId::new("message")
                                .map_err(native_fixture_error)?,
                            1,
                            plakous_model::DataClassification::Sensitive,
                            serde_json::value::RawValue::from_string(
                                json!({"type": "message", "text": "fixture final"}).to_string(),
                            )
                            .map_err(native_fixture_error)?,
                        )
                        .map_err(native_fixture_error)?;
                        blocks.push(ContentBlock::Opaque(
                            plakous_model::OpaqueBlock::new(
                                native_text,
                                plakous_model::OpaqueFallback::CompatibleReplay,
                            )
                            .map_err(native_fixture_error)?,
                        ));
                    }
                }
                let output = ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::from_u128(
                        self.entry_base.saturating_add(entry_offset),
                    ))
                    .map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "artifact fixture entry is invalid",
                        )
                    })?,
                    Actor::Assistant,
                    blocks,
                    Provenance::ModelOutput,
                )
                .map_err(|_| {
                    ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "artifact fixture output is invalid",
                    )
                })?;
                let mut response = ModelResponse::new(
                    if self.empty_refusal {
                        Vec::new()
                    } else {
                        vec![output.clone()]
                    },
                    FinishDetails::new(
                        if self.empty_refusal {
                            FinishReason::Refusal
                        } else if tool_call {
                            FinishReason::ToolCalls
                        } else if self.provider_pause_calls.contains(&call) {
                            FinishReason::ProviderContinuation
                        } else if lock_mutex(&self.assistant_continuation_calls).contains(&call) {
                            FinishReason::AssistantContinuation
                        } else {
                            FinishReason::Stop
                        },
                        None,
                    ),
                    (self.report_usage
                        && self
                            .usage_calls
                            .as_ref()
                            .is_none_or(|calls| calls.contains(&call)))
                    .then(|| Usage::new(3, 2, 0, 0, 0))
                    .or(counted_usage)
                    .filter(|_| call >= self.usage_suppressed_until.load(Ordering::Acquire)),
                    self.response_provenance
                        .then(|| format!("msg_fixture_{}_{call}", self.entry_base)),
                    plan.digest(),
                );
                let served_model = lock_mutex(&self.served_model).clone();
                if self.response_provenance || served_model.is_some() {
                    response = response.with_metadata(plakous_model::ModelResponseMetadata::new(
                        Some(
                            served_model
                                .unwrap_or_else(|| self.descriptor.requested_model().clone()),
                        ),
                        Some(
                            plakous_model::ModelProviderRequestId::new(format!(
                                "req_fixture_{}_{call}",
                                self.entry_base
                            ))
                            .expect("valid fixture request id"),
                        ),
                    ));
                }
                if call < self.candidate_calls {
                    let covered = plan
                        .request()
                        .input()
                        .staged_canonical_item_prefix_identity(std::slice::from_ref(&output))
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture coverage is invalid",
                            )
                        })?;
                    let request = plan.request();
                    let metadata = request.metadata();
                    let source = request
                        .input()
                        .canonical_item_prefix_identity(request.input().items().len())
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture source is invalid",
                            )
                        })?;
                    let producer_cache_affinity = if self.tamper_affinity {
                        Some(
                            Digest::sha256(
                                DigestDomain::CacheAffinity,
                                1,
                                &[b"foreign-artifact-cache-affinity"],
                            )
                            .map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture affinity is invalid",
                                )
                            })?,
                        )
                    } else {
                        cache_affinity
                    };
                    lock_mutex(&self.produced_affinities).push(producer_cache_affinity);
                    let producer = FamilyArtifactProducerFacts::new(
                        request.identity(),
                        request.model_input_identity(),
                        plan.digest(),
                        producer_cache_affinity,
                        if self.tamper_run {
                            RunId::try_from(uuid::Uuid::from_u128(0xE8C_FFFF))
                                .map_err(native_fixture_error)?
                        } else {
                            metadata.run_id()
                        },
                        metadata.root(),
                        metadata.attempt_id(),
                        source.identity(),
                        u32::try_from(request.input().items().len()).map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture source count is invalid",
                            )
                        })?,
                        1,
                        if self.tamper_coverage {
                            Digest::sha256(
                                DigestDomain::CanonicalItemPrefix,
                                1,
                                &[b"foreign-covered-projection"],
                            )
                            .map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture tamper identity is invalid",
                                )
                            })?
                        } else {
                            covered.identity()
                        },
                        u32::try_from(covered.item_count()).map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture covered count is invalid",
                            )
                        })?,
                        Digest::sha256(
                            DigestDomain::ProviderOperation,
                            3,
                            &[
                                b"artifact-fixture-physical",
                                metadata.attempt_id().as_uuid().as_bytes(),
                            ],
                        )
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture physical identity is invalid",
                            )
                        })?,
                        Digest::sha256(
                            DigestDomain::ProviderOperation,
                            3,
                            &[
                                b"artifact-fixture-terminal",
                                metadata.attempt_id().as_uuid().as_bytes(),
                            ],
                        )
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture terminal identity is invalid",
                            )
                        })?,
                    )
                    .map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "artifact fixture producer is invalid",
                        )
                    })?;
                    let candidate = SealedFamilyArtifactCandidate::new(
                        self.native_final_family.unwrap_or("responses"),
                        if self.native_final_family == Some("messages") {
                            crate::manifest::MESSAGES_FAMILY_ARTIFACT_SCHEMA_REVISION
                        } else {
                            crate::internal::RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION
                        },
                        if self.native_final_family == Some("messages") {
                            crate::manifest::MESSAGES_FAMILY_ARTIFACT_CODEC_REVISION
                        } else {
                            crate::internal::RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION
                        },
                        Digest::sha256(
                            DigestDomain::ModelBinding,
                            3,
                            &[
                                b"artifact-fixture-binding",
                                &self.entry_base.to_be_bytes(),
                                &call.to_be_bytes(),
                            ],
                        )
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "artifact fixture binding is invalid",
                            )
                        })?,
                        producer,
                        if let Some(family) = self.native_final_family {
                            serde_json::to_vec(&if family == "messages" { json!({"thinking": "fixture thought", "signature": "fixture-signature"}) } else { json!({"encrypted_content": "fixture-encrypted-reasoning"}) }).map_err(native_fixture_error)?
                        } else { format!("responses-v4-artifact-{}-{call}", self.entry_base).into_bytes() },
                    )
                    .map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "artifact fixture candidate is invalid",
                        )
                    })?;
                    response =
                        response
                            .with_family_artifact_candidate(candidate)
                            .map_err(|_| {
                                ModelError::new(
                                    ModelErrorKind::InvalidEvent,
                                    "artifact fixture response binding is invalid",
                                )
                            })?;
                }
                let mut events = Vec::new();
                let output_location = OutputLocation::new(0, 0);
                if !self.terminal_only {
                    events.push(Ok(ModelEvent::ResponseStarted(
                        ResponseStarted::new(None, plan.digest())
                            .with_metadata(response.metadata().clone()),
                    )));
                    if self.ttft_probe {
                        events.push(Ok(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                            1, 0, 0, 0, 0,
                        )))));
                    }
                    // The probe mirrors real dialects: open a text container,
                    // then stream deltas. A `Complete` start would itself count
                    // as first content.
                    let block_start = if self.ttft_probe {
                        BlockStart::Text { phase: None }
                    } else if self.empty_refusal
                        && let ContentBlock::ToolCall(call) = &output.blocks()[0]
                    {
                        BlockStart::ToolCall {
                            call_id: call.call_id(),
                            tool: call.exposed_name().clone(),
                            owner: call.owner().clone(),
                            state: call.state(),
                            correlation: call.correlation().clone(),
                        }
                    } else {
                        BlockStart::Complete(output.blocks()[0].clone())
                    };
                    events.push(Ok(ModelEvent::BlockStarted(BlockStarted::new(
                        output.id(),
                        output_location,
                        block_start,
                    ))));
                    if self.empty_refusal && tool_call {
                        events.push(Ok(ModelEvent::ToolInputDelta(
                            plakous_model::ToolInputDelta::new(output_location, b"{unfinished"),
                        )));
                    }
                }
                let text_delta = (!self.terminal_only)
                    .then(|| {
                        output.blocks().first().and_then(|block| {
                            let ContentBlock::Text(text) = block else {
                                return None;
                            };
                            Some(TextDelta::new(
                                output_location,
                                text.text().as_bytes().to_vec(),
                            ))
                        })
                    })
                    .flatten();
                if self.ttft_probe {
                    let response = response.clone();
                    let delay = self.ttft_content_delay;
                    return Ok(Box::pin(
                        futures_util::stream::iter(events)
                            .chain(futures_util::stream::once(async move {
                                tokio::time::sleep(delay).await;
                                Ok(ModelEvent::TextDelta(text_delta.ok_or_else(|| {
                                    ModelError::new(
                                        ModelErrorKind::InvalidEvent,
                                        "ttft probe requires a text output block",
                                    )
                                })?))
                            }))
                            .chain(futures_util::stream::iter([Ok(
                                ModelEvent::ResponseCompleted(response),
                            )])),
                    ) as ModelStream);
                }
                if let Some(delta) = text_delta {
                    events.push(Ok(ModelEvent::TextDelta(delta)));
                }
                events.push(Ok(ModelEvent::ResponseCompleted(response.clone())));
                if self.duplicate_terminal {
                    events.push(Ok(ModelEvent::ResponseCompleted(response)));
                }
                if hold_stream && let Some(release) = release {
                    // The start future has returned a real stream before this
                    // gate is first polled. Controls must remain live here too.
                    let gate = futures_util::stream::once(async move {
                        self.held_call_stream_polled.store(true, Ordering::Release);
                        self.held_call_started.notify_one();
                        tokio::select! {
                            () = call_context.cancellation().cancelled() => {
                                self.held_call_joined.store(true, Ordering::Release);
                                Some(Err(ModelError::cancelled()))
                            }
                            () = release.notified() => None,
                        }
                    })
                    .filter_map(std::future::ready);
                    return Ok(
                        Box::pin(gate.chain(futures_util::stream::iter(events))) as ModelStream
                    );
                }
                Ok(Box::pin(futures_util::stream::iter(events)) as ModelStream)
            })
        }))
    }
}

#[derive(Clone)]
pub(super) enum CompactionFixtureOutcome {
    Summary,
    OversizedResponse,
    LengthSummary,
    EmptySummary,
    InvalidSummary,
    DefiniteFailure(ModelError),
    AmbiguousFailure(ModelError),
    /// The start fails before anything is sent, as final revalidation does.
    DispatchRejected(ModelError),
}

pub(super) struct RecoveryCompactionModel {
    pub(super) scripted: Arc<plakous_test_support::ScriptedModel>,
    pub(super) dispatch_authority: Mutex<Option<RuntimeModelAuthority>>,
    pub(super) authority_checks: AtomicUsize,
    pub(super) outcome: CompactionFixtureOutcome,
    pub(super) fail_prepare_on: Option<usize>,
    pub(super) prepares: AtomicUsize,
    pub(super) prepared_requests: Mutex<Vec<Vec<u8>>>,
    pub(super) starts: AtomicUsize,
    pub(super) recover_calls: AtomicUsize,
    pub(super) omit_locator: AtomicBool,
    pub(super) stall_successor_authority: AtomicBool,
    pub(super) slow_body: AtomicBool,
}

impl RecoveryCompactionModel {
    pub(super) fn new(
        descriptor: ResolvedModelDescriptor,
        authority: RuntimeModelAuthority,
        recovery: plakous_test_support::ScriptedRecoveryState,
        outcome: CompactionFixtureOutcome,
        fail_prepare_on: Option<usize>,
    ) -> TestResult<Arc<Self>> {
        let scenario = plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::Immediate,
            vec![plakous_test_support::ScenarioStep::Eof],
        );
        let scripted = plakous_test_support::ScriptedModel::new(descriptor, [scenario])
            .with_runtime_authority(authority)
            .with_context_observation(CountObservation::new(
                "agent-scripted-counter",
                "agent-scripted-exact-v1",
                64,
            )?)
            .with_content_sensitive_observation()
            .with_call_recovery(recovery);
        Ok(Arc::new(Self {
            scripted: Arc::new(scripted),
            dispatch_authority: Mutex::new(None),
            authority_checks: AtomicUsize::new(0),
            outcome,
            fail_prepare_on,
            prepares: AtomicUsize::new(0),
            prepared_requests: Mutex::new(Vec::new()),
            starts: AtomicUsize::new(0),
            recover_calls: AtomicUsize::new(0),
            omit_locator: AtomicBool::new(false),
            stall_successor_authority: AtomicBool::new(false),
            slow_body: AtomicBool::new(false),
        }))
    }

    pub(super) fn omit_recovery_locator(&self) {
        self.omit_locator.store(true, Ordering::Release);
    }

    pub(super) fn drift_final_dispatch_authority(&self, authority: RuntimeModelAuthority) {
        *lock_mutex(&self.dispatch_authority) = Some(authority);
    }
}

impl LanguageModel for RecoveryCompactionModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        if self.stall_successor_authority.load(Ordering::Acquire)
            && self.starts.load(Ordering::Acquire) > 0
        {
            return Box::pin(std::future::pending());
        }
        let check = self
            .authority_checks
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if check >= 4
            && let Some(authority) = lock_mutex(&self.dispatch_authority).clone()
        {
            return Box::pin(async move { Ok(authority) });
        }
        Arc::clone(&self.scripted).revalidate_runtime_authority(checked_at)
    }

    fn recover_call(
        self: Arc<Self>,
        request: ModelCallRecoveryRequest,
    ) -> ModelCallRecoveryOperation {
        self.recover_calls.fetch_add(1, Ordering::AcqRel);
        Arc::clone(&self.scripted).recover_call(request)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        lock_mutex(&self.prepared_requests)
            .push(serde_json::to_vec(plan.request()).unwrap_or_default());
        let prepare = self
            .prepares
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if self.fail_prepare_on == Some(prepare) {
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "fixture successor measurement infrastructure is unavailable",
            ));
        }
        let record_context = context.clone();
        let inert = Arc::clone(&self.scripted).prepare_call(plan.clone(), context)?;
        let record = if self.omit_locator.load(Ordering::Acquire) {
            PreparedCallRecord::for_prepared(
                self.scripted.descriptor(),
                &plan,
                inert.record().projection().clone(),
                &record_context,
            )?
        } else {
            inert.record().clone()
        };
        let fixture = Arc::clone(&self);
        Ok(PreparedModelCall::new(record, move |call_context| {
            Box::pin(async move {
                fixture.starts.fetch_add(1, Ordering::AcqRel);
                if let CompactionFixtureOutcome::DispatchRejected(error) = &fixture.outcome {
                    return Err(error.clone());
                }
                let hard_deadline = call_context.deadline();
                let dispatch_timeout = call_context.dispatch_timeout();
                let mut inert_stream = inert.start(call_context).await?;
                while let Some(event) = inert_stream.next().await {
                    event?;
                }
                if let CompactionFixtureOutcome::AmbiguousFailure(error) = &fixture.outcome {
                    return Ok(
                        Box::pin(futures_util::stream::iter([Err(error.clone())])) as ModelStream
                    );
                }
                let mut entry_bytes = [0_u8; 16];
                entry_bytes.copy_from_slice(&plan.digest().bytes()[..16]);
                let block = if matches!(fixture.outcome, CompactionFixtureOutcome::EmptySummary) {
                    ContentBlock::Reasoning(
                        ReasoningBlock::new(
                            ReasoningVisibility::Summary,
                            Some("reasoning without summary text".to_owned()),
                            None,
                        )
                        .map_err(|_| {
                            ModelError::new(
                                ModelErrorKind::InvalidEvent,
                                "compaction fixture reasoning is invalid",
                            )
                        })?,
                    )
                } else {
                    // JSON escaping makes one valid canonical text block exceed
                    // the operation bound without constructing dozens of blocks.
                    let summary =
                        if matches!(fixture.outcome, CompactionFixtureOutcome::OversizedResponse) {
                            "\0".repeat((64 * 1024 * 1024) / 6 + 1)
                        } else {
                            "bounded semantic summary".to_owned()
                        };
                    ContentBlock::Text(TextBlock::new(summary, None).map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "compaction fixture summary is invalid",
                        )
                    })?)
                };
                let output = ConversationItem::new(
                    EntryId::try_from(uuid::Uuid::from_bytes(entry_bytes)).map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "compaction fixture entry identity is invalid",
                        )
                    })?,
                    Actor::Assistant,
                    vec![block],
                    Provenance::ModelOutput,
                )
                .map_err(|_| {
                    ModelError::new(
                        ModelErrorKind::InvalidEvent,
                        "compaction fixture output is invalid",
                    )
                })?;
                let finish = match fixture.outcome {
                    CompactionFixtureOutcome::Summary
                    | CompactionFixtureOutcome::OversizedResponse
                    | CompactionFixtureOutcome::EmptySummary
                    | CompactionFixtureOutcome::DefiniteFailure(_) => FinishReason::Stop,
                    CompactionFixtureOutcome::LengthSummary => FinishReason::Length,
                    CompactionFixtureOutcome::InvalidSummary => FinishReason::ToolCalls,
                    CompactionFixtureOutcome::AmbiguousFailure(_)
                    | CompactionFixtureOutcome::DispatchRejected(_) => unreachable!(),
                };
                let plan_digest = match &fixture.outcome {
                    CompactionFixtureOutcome::DefiniteFailure(error) => Digest::sha256(
                        DigestDomain::RequestPlan,
                        1,
                        &[b"foreign-compaction-plan", error.to_string().as_bytes()],
                    )
                    .map_err(|_| {
                        ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "foreign fixture plan digest is invalid",
                        )
                    })?,
                    CompactionFixtureOutcome::Summary
                    | CompactionFixtureOutcome::OversizedResponse
                    | CompactionFixtureOutcome::LengthSummary
                    | CompactionFixtureOutcome::EmptySummary
                    | CompactionFixtureOutcome::InvalidSummary => plan.digest(),
                    CompactionFixtureOutcome::AmbiguousFailure(_)
                    | CompactionFixtureOutcome::DispatchRejected(_) => unreachable!(),
                };
                let response = ModelResponse::new(
                    vec![output],
                    FinishDetails::new(finish, None),
                    matches!(fixture.outcome, CompactionFixtureOutcome::EmptySummary)
                        .then(|| Usage::new(101, 7, 5, 3, 2).with_cost_micros(Some(666))),
                    None,
                    plan_digest,
                );
                Ok(Box::pin(futures_util::stream::once(async move {
                    if fixture.slow_body.load(Ordering::Acquire) {
                        assert_eq!(dispatch_timeout, Some(Duration::from_millis(10)));
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        assert!(
                            hard_deadline.is_none(),
                            "paid body must not inherit the dispatch deadline"
                        );
                    }
                    Ok(ModelEvent::ResponseCompleted(response))
                })) as ModelStream)
            })
        }))
    }
}
