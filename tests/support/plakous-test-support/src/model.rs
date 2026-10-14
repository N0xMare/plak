//! Pull-driven deterministic model scenarios for neutral conformance tests.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use plakous_model::{
    CancellationToken, CountObservation, Digest, DigestDomain, LanguageModel, ModelCallContext,
    ModelCallRecoveryError, ModelCallRecoveryOperation, ModelCallRecoveryRequest,
    ModelCallRecoveryResult, ModelError, ModelErrorKind, ModelEvent, ModelPreparationContext,
    ModelPreparationError, ModelPreparationErrorKind, ModelRequest, ModelResponse, ModelStream,
    PreparedCacheUse, PreparedCallError, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection,
    ProviderCallRecoveryLocator, ResolvedModelDescriptor, ResponseStarted, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, Timestamp, ValidatedRequestPlan,
};

fn register_cancellation(token: &CancellationToken, context: &mut Context<'_>) {
    let mut cancelled = Box::pin(token.cancelled());
    let _ = cancelled.as_mut().poll(context);
}

#[derive(Debug, Default)]
struct BarrierState {
    released: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

/// A deterministic externally released model-scenario barrier.
#[derive(Clone, Default)]
pub struct ScenarioBarrier {
    state: Arc<BarrierState>,
}

impl ScenarioBarrier {
    /// Creates a blocked barrier.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Releases the barrier and wakes every waiting operation.
    pub fn release(&self) {
        if !self.state.released.swap(true, Ordering::AcqRel) {
            let wakers = {
                let mut registered = match self.state.wakers.lock() {
                    Ok(registered) => registered,
                    Err(poisoned) => poisoned.into_inner(),
                };
                std::mem::take(&mut *registered)
            };
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Reports whether the barrier is released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.state.released.load(Ordering::Acquire)
    }

    fn register(&self, waker: &Waker) {
        if self.is_released() {
            waker.wake_by_ref();
            return;
        }
        let mut registered = match self.state.wakers.lock() {
            Ok(registered) => registered,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !registered.iter().any(|known| known.will_wake(waker)) {
            registered.push(waker.clone());
        }
        if self.is_released() {
            waker.wake_by_ref();
        }
    }
}

impl fmt::Debug for ScenarioBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScenarioBarrier")
            .field("released", &self.is_released())
            .finish()
    }
}

/// A declarative request expectation checked against the exact prepared plan.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExpectedRequest {
    /// Accept any validated request.
    Any,
    /// Require exact canonical request equality.
    Exact(Box<ModelRequest>),
    /// Require one exact validated-plan digest.
    PlanDigest(Digest),
    /// Require an exact request except for the runtime-owned harness-plan identity.
    ///
    /// This expectation is only for agent-runtime fixtures whose request is
    /// constructed before the runtime compiles and binds its harness plan.
    ExactBeforeRuntimeHarnessPlan(Box<ModelRequest>),
}

impl ExpectedRequest {
    fn matches(&self, plan: &ValidatedRequestPlan) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => expected.as_ref() == plan.request(),
            Self::PlanDigest(expected) => *expected == plan.digest(),
            Self::ExactBeforeRuntimeHarnessPlan(expected) => {
                exactly_matches_before_runtime_harness_plan(expected, plan.request())
            }
        }
    }
}

/// Deterministic behavior before a scripted stream is connected.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ScenarioStart {
    /// Connect immediately.
    Immediate,
    /// Wait at a caller-controlled barrier.
    Barrier(ScenarioBarrier),
    /// Remain pending until cancellation.
    AwaitCancellation,
    /// Yield for a deterministic number of polls.
    DelayPolls(u32),
    /// Fail before a stream is returned.
    Error(ModelError),
}

/// One pull-driven step in a deterministic model stream.
#[allow(
    clippy::large_enum_variant,
    reason = "script fixtures intentionally retain structural model events inline for ergonomic deterministic scenarios"
)]
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ScenarioStep {
    /// Emit one structural event.
    Emit(ModelEvent),
    /// Wait at a caller-controlled barrier.
    Barrier(ScenarioBarrier),
    /// Yield for a deterministic number of stream polls.
    DelayPolls(u32),
    /// Remain pending until cancellation.
    AwaitCancellation,
    /// Emit one typed terminal stream error.
    Error(ModelError),
    /// End the stream without synthesizing a terminal event.
    Eof,
}

/// One expected request and its deterministic start and stream schedule.
#[derive(Clone, Debug)]
pub struct ModelScenario {
    expected: ExpectedRequest,
    start: ScenarioStart,
    steps: Vec<ScenarioStep>,
}

impl ModelScenario {
    /// Creates a model scenario.
    #[must_use]
    pub fn new(expected: ExpectedRequest, start: ScenarioStart, steps: Vec<ScenarioStep>) -> Self {
        Self {
            expected,
            start,
            steps,
        }
    }

    /// Returns the request expectation.
    #[must_use]
    pub const fn expected(&self) -> &ExpectedRequest {
        &self.expected
    }

    /// Returns start behavior.
    #[must_use]
    pub const fn start(&self) -> &ScenarioStart {
        &self.start
    }

    /// Returns ordered stream steps.
    #[must_use]
    pub fn steps(&self) -> &[ScenarioStep] {
        &self.steps
    }

    /// Rebinds the scripted response to the digest the runtime actually planned.
    ///
    /// `Exact` and `PlanDigest` scenarios already name the exact plan, so they
    /// keep their scripted digest. The expectations that do not pin the request
    /// — the pre-harness-plan form and [`ExpectedRequest::Any`] — cannot know
    /// the digest in advance and would otherwise script a response that the
    /// runtime rejects as belonging to another plan.
    fn bind_runtime_plan_digest(&mut self, plan_digest: Digest) -> Result<(), ModelError> {
        if matches!(
            self.expected,
            ExpectedRequest::Exact(_) | ExpectedRequest::PlanDigest(_)
        ) {
            return Ok(());
        }
        let mut steps = self.steps.clone();
        for step in &mut steps {
            let ScenarioStep::Emit(event) = step else {
                continue;
            };
            match event {
                ModelEvent::ResponseStarted(started) => {
                    *started = ResponseStarted::new(
                        started.response_id().map(ToOwned::to_owned),
                        plan_digest,
                    )
                    .with_metadata(started.metadata().clone());
                }
                ModelEvent::ResponseCompleted(response) => {
                    if response.family_artifact_candidate().is_some() {
                        return Err(ModelError::new(
                            ModelErrorKind::InvalidEvent,
                            "scripted response with a family artifact cannot rebind its plan digest",
                        ));
                    }
                    *response = ModelResponse::new(
                        response.output().to_vec(),
                        response.finish().clone(),
                        response.usage(),
                        response.response_id().map(ToOwned::to_owned),
                        plan_digest,
                    )
                    .with_metadata(response.metadata().clone());
                }
                _ => {}
            }
        }
        self.steps = steps;
        Ok(())
    }
}

fn exactly_matches_before_runtime_harness_plan(
    expected: &ModelRequest,
    actual: &ModelRequest,
) -> bool {
    expected.resolved_harness_plan_identity().is_none()
        && actual.resolved_harness_plan_identity().is_some()
        && expected.selected_provider_configuration_id()
            == actual.selected_provider_configuration_id()
        && expected.effective_provider_configuration_id()
            == actual.effective_provider_configuration_id()
        && expected.instructions() == actual.instructions()
        && expected.input() == actual.input()
        && expected.tools() == actual.tools()
        && expected.output() == actual.output()
        && expected.reasoning() == actual.reasoning()
        && expected.tool_choice() == actual.tool_choice()
        && expected.cache() == actual.cache()
        && expected.limits() == actual.limits()
        && expected.context_admission_requirement() == actual.context_admission_requirement()
        && expected.metadata() == actual.metadata()
        && expected.native() == actual.native()
}

#[allow(
    clippy::large_enum_variant,
    reason = "deterministic recovery fixtures retain exact terminal values inline"
)]
#[derive(Clone, Debug, Deserialize, Serialize)]
enum ScriptedRecoveryOutcome {
    NotDispatched,
    Ambiguous,
    Completed(ModelResponse),
    DefinitelyFailed(ModelError),
}

#[derive(Clone, Debug)]
struct ScriptedRecoveryEntry {
    record: PreparedCallRecord,
    request: ModelCallRecoveryRequest,
    outcome: ScriptedRecoveryOutcome,
    starts: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScriptedRecoverySnapshotEntry {
    record: PreparedCallRecord,
    outcome: ScriptedRecoveryOutcome,
    starts: u32,
}

const MAX_SCRIPTED_RECOVERY_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

/// Invalid deterministic scripted recovery snapshot.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ScriptedRecoverySnapshotError {
    /// Exact provider recovery scope or terminal facts were invalid.
    #[error(transparent)]
    Recovery(#[from] ModelCallRecoveryError),
    /// Snapshot encoding or decoding failed.
    #[error("scripted provider call recovery snapshot codec failed")]
    Codec,
    /// Snapshot facts were duplicate or contradictory.
    #[error("scripted provider call recovery snapshot is invalid")]
    Invalid,
    /// Snapshot bytes exceeded the strict test bound.
    #[error("scripted provider call recovery snapshot exceeds its retention bound")]
    TooLarge,
}

/// Restart-shareable deterministic provider-call recovery authority.
///
/// A fresh [`ScriptedModel`] may be given a clone of this value to simulate a
/// process restart. The state is keyed only by the sealed locator/request, and
/// recovery reads it without consuming scenarios or starting a call.
#[derive(Clone, Default)]
pub struct ScriptedRecoveryState {
    entries: Arc<Mutex<HashMap<Digest, ScriptedRecoveryEntry>>>,
}

impl ScriptedRecoveryState {
    /// Creates an empty deterministic recovery registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, record: PreparedCallRecord) -> Result<(), ModelCallRecoveryError> {
        let request = record.recovery_request()?;
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        match entries.get(&request.identity()) {
            Some(entry) if entry.request == request => Ok(()),
            Some(_) => Err(ModelCallRecoveryError::ForeignScope {
                field: "scripted recovery registry",
            }),
            None => {
                entries.insert(
                    request.identity(),
                    ScriptedRecoveryEntry {
                        record,
                        request,
                        outcome: ScriptedRecoveryOutcome::NotDispatched,
                        starts: 0,
                    },
                );
                Ok(())
            }
        }
    }

    /// Serializes the complete deterministic recovery authority for restart tests.
    ///
    /// # Errors
    ///
    /// Rejects serialization failure or a snapshot beyond the scripted bound.
    pub fn snapshot(&self) -> Result<Vec<u8>, ScriptedRecoverySnapshotError> {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut snapshot = entries
            .values()
            .map(|entry| ScriptedRecoverySnapshotEntry {
                record: entry.record.clone(),
                outcome: entry.outcome.clone(),
                starts: entry.starts,
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|left, right| {
            left.record
                .identity()
                .identity_bytes()
                .cmp(&right.record.identity().identity_bytes())
        });
        let bytes =
            serde_json::to_vec(&snapshot).map_err(|_| ScriptedRecoverySnapshotError::Codec)?;
        if bytes.len() > MAX_SCRIPTED_RECOVERY_SNAPSHOT_BYTES {
            return Err(ScriptedRecoverySnapshotError::TooLarge);
        }
        Ok(bytes)
    }

    /// Reconstructs a fresh deterministic recovery authority from a snapshot.
    ///
    /// # Errors
    ///
    /// Rejects oversized, malformed, duplicate, contradictory, or foreign
    /// prepared/outcome state.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, ScriptedRecoverySnapshotError> {
        if bytes.len() > MAX_SCRIPTED_RECOVERY_SNAPSHOT_BYTES {
            return Err(ScriptedRecoverySnapshotError::TooLarge);
        }
        let snapshot: Vec<ScriptedRecoverySnapshotEntry> =
            serde_json::from_slice(bytes).map_err(|_| ScriptedRecoverySnapshotError::Codec)?;
        let state = Self::new();
        for entry in snapshot {
            let request = entry.record.recovery_request()?;
            let valid_starts = matches!(&entry.outcome, ScriptedRecoveryOutcome::NotDispatched)
                && entry.starts == 0
                || !matches!(&entry.outcome, ScriptedRecoveryOutcome::NotDispatched)
                    && entry.starts == 1;
            if !valid_starts {
                return Err(ScriptedRecoverySnapshotError::Invalid);
            }
            match &entry.outcome {
                ScriptedRecoveryOutcome::NotDispatched => {
                    ModelCallRecoveryResult::proven_not_dispatched(&request, None)?;
                }
                ScriptedRecoveryOutcome::Ambiguous => {
                    ModelCallRecoveryResult::ambiguous(&request, None, None)?;
                }
                ScriptedRecoveryOutcome::Completed(response) => {
                    ModelCallRecoveryResult::completed(
                        &request,
                        response.clone(),
                        response.usage(),
                        None,
                    )?;
                }
                ScriptedRecoveryOutcome::DefinitelyFailed(error) => {
                    ModelCallRecoveryResult::definitely_failed(
                        &request,
                        error.clone(),
                        None,
                        None,
                    )?;
                }
            }
            let mut entries = match state.entries.lock() {
                Ok(entries) => entries,
                Err(poisoned) => poisoned.into_inner(),
            };
            if entries
                .insert(
                    request.identity(),
                    ScriptedRecoveryEntry {
                        record: entry.record,
                        request,
                        outcome: entry.outcome,
                        starts: entry.starts,
                    },
                )
                .is_some()
            {
                return Err(ScriptedRecoverySnapshotError::Invalid);
            }
        }
        Ok(state)
    }

    fn begin(&self, request: &ModelCallRecoveryRequest) -> bool {
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(entry) = entries.get_mut(&request.identity()) else {
            return false;
        };
        if entry.request != *request
            || !matches!(entry.outcome, ScriptedRecoveryOutcome::NotDispatched)
        {
            return false;
        }
        entry.starts = entry.starts.saturating_add(1);
        entry.outcome = ScriptedRecoveryOutcome::Ambiguous;
        true
    }

    fn complete(&self, request: &ModelCallRecoveryRequest, response: ModelResponse) {
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = entries.get_mut(&request.identity())
            && entry.request == *request
            && matches!(entry.outcome, ScriptedRecoveryOutcome::Ambiguous)
        {
            entry.outcome = ScriptedRecoveryOutcome::Completed(response);
        }
    }

    fn fail(&self, request: &ModelCallRecoveryRequest, error: ModelError) {
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = entries.get_mut(&request.identity())
            && entry.request == *request
            && matches!(entry.outcome, ScriptedRecoveryOutcome::Ambiguous)
            && error.kind() != ModelErrorKind::OutcomeUnknown
        {
            entry.outcome = ScriptedRecoveryOutcome::DefinitelyFailed(error);
        }
    }

    fn recover(
        &self,
        request: &ModelCallRecoveryRequest,
    ) -> Result<ModelCallRecoveryResult, ModelCallRecoveryError> {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(entry) = entries.get(&request.identity()) else {
            return ModelCallRecoveryResult::unavailable(request);
        };
        if entry.request != *request {
            return ModelCallRecoveryResult::unavailable(request);
        }
        match &entry.outcome {
            ScriptedRecoveryOutcome::NotDispatched => {
                ModelCallRecoveryResult::proven_not_dispatched(request, None)
            }
            ScriptedRecoveryOutcome::Ambiguous => {
                ModelCallRecoveryResult::ambiguous(request, None, None)
            }
            ScriptedRecoveryOutcome::Completed(response) => ModelCallRecoveryResult::completed(
                request,
                response.clone(),
                response.usage(),
                None,
            ),
            ScriptedRecoveryOutcome::DefinitelyFailed(error) => {
                ModelCallRecoveryResult::definitely_failed(request, error.clone(), None, None)
            }
        }
    }

    /// Returns the number of admitted physical starts for one exact locator.
    #[must_use]
    pub fn start_count(&self, request: &ModelCallRecoveryRequest) -> u32 {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        entries
            .get(&request.identity())
            .filter(|entry| entry.request == *request)
            .map_or(0, |entry| entry.starts)
    }
}

impl fmt::Debug for ScriptedRecoveryState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = match self.entries.lock() {
            Ok(entries) => entries.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        };
        formatter
            .debug_struct("ScriptedRecoveryState")
            .field("tracked_calls", &entries)
            .finish_non_exhaustive()
    }
}

/// A pull-driven, transport-free implementation of [`LanguageModel`].
pub struct ScriptedModel {
    descriptor: ResolvedModelDescriptor,
    runtime_authority: Option<RuntimeModelAuthority>,
    context_observation: Option<CountObservation>,
    /// Replace the observation's token count with canonical history bytes.
    content_sensitive_observation: bool,
    content_filter_instruction: Option<String>,
    recovery: Option<ScriptedRecoveryState>,
    /// Wire maximum sealed at preparation, when it must differ from the request bound.
    prepared_output_limit: Option<NonZeroU32>,
    scenarios: Mutex<VecDeque<ModelScenario>>,
    captured_plans: Mutex<Vec<ValidatedRequestPlan>>,
    captured_records: Mutex<Vec<PreparedCallRecord>>,
}

impl ScriptedModel {
    /// Creates a scripted model with FIFO scenarios.
    #[must_use]
    pub fn new(
        descriptor: ResolvedModelDescriptor,
        scenarios: impl IntoIterator<Item = ModelScenario>,
    ) -> Self {
        Self {
            descriptor,
            runtime_authority: None,
            context_observation: None,
            content_sensitive_observation: false,
            content_filter_instruction: None,
            recovery: None,
            prepared_output_limit: None,
            scenarios: Mutex::new(scenarios.into_iter().collect()),
            captured_plans: Mutex::new(Vec::new()),
            captured_records: Mutex::new(Vec::new()),
        }
    }

    /// Returns the immutable descriptor supplied by this test fixture's owner.
    #[must_use]
    pub const fn descriptor(&self) -> &ResolvedModelDescriptor {
        &self.descriptor
    }

    /// Adds explicit credential-free authority facts for durable-runtime tests.
    ///
    /// The scripted model is a deterministic test double. Production adapters
    /// must obtain equivalent evidence from their actual route/auth owner.
    #[must_use]
    pub fn with_runtime_authority(mut self, authority: RuntimeModelAuthority) -> Self {
        self.runtime_authority = Some(authority);
        self
    }

    /// Adds an explicit complete-projection count produced by this scripted
    /// fixture's deterministic lowering.
    ///
    /// The observation remains non-authoritative until the supplied descriptor
    /// admits its exact certificate and scope. Production adapters must use
    /// their family-owned exact counter or certified upper-bound procedure.
    #[must_use]
    pub fn with_context_observation(mut self, observation: CountObservation) -> Self {
        self.context_observation = Some(observation);
        self
    }

    /// Counts canonical history bytes instead of the fixed observation.
    ///
    /// The certificate id and revision stay the ones from
    /// [`Self::with_context_observation`]. A fold whose summary is larger than
    /// the history it replaced then measures as growth.
    #[must_use]
    pub fn with_content_sensitive_observation(mut self) -> Self {
        self.content_sensitive_observation = true;
        self
    }

    /// Supplies the host-authored instruction for a scripted filtered retry.
    #[must_use]
    pub fn with_content_filter_instruction(mut self, instruction: String) -> Self {
        self.content_filter_instruction = Some(instruction);
        self
    }

    /// Opts this scripted fixture into restart-safe provider-call recovery.
    #[must_use]
    pub fn with_call_recovery(mut self, recovery: ScriptedRecoveryState) -> Self {
        self.recovery = Some(recovery);
        self
    }

    /// Seals a wire output maximum other than the request's product bound.
    ///
    /// Production lowering can put a smaller maximum on the wire than the
    /// product bound. Unset, preparation uses the request's `max_output_tokens`,
    /// so the two figures match and a test cannot tell which one a retry named.
    #[must_use]
    pub fn with_prepared_output_limit(mut self, tokens: NonZeroU32) -> Self {
        self.prepared_output_limit = Some(tokens);
        self
    }

    /// Returns cloned plans captured at successful start admission.
    #[must_use]
    pub fn captured_plans(&self) -> Vec<ValidatedRequestPlan> {
        match self.captured_plans.lock() {
            Ok(plans) => plans.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns inert records captured at successful start admission.
    #[must_use]
    pub fn captured_records(&self) -> Vec<PreparedCallRecord> {
        match self.captured_records.lock() {
            Ok(records) => records.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns the number of unconsumed scenarios.
    #[must_use]
    pub fn remaining_scenarios(&self) -> usize {
        match self.scenarios.lock() {
            Ok(scenarios) => scenarios.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    fn pop_scenario(&self) -> Option<ModelScenario> {
        let mut scenarios = match self.scenarios.lock() {
            Ok(scenarios) => scenarios,
            Err(poisoned) => poisoned.into_inner(),
        };
        scenarios.pop_front()
    }

    fn restore_scenario(&self, scenario: ModelScenario) {
        let mut scenarios = match self.scenarios.lock() {
            Ok(scenarios) => scenarios,
            Err(poisoned) => poisoned.into_inner(),
        };
        scenarios.push_front(scenario);
    }

    fn capture(&self, plan: ValidatedRequestPlan, record: PreparedCallRecord) {
        let mut plans = match self.captured_plans.lock() {
            Ok(plans) => plans,
            Err(poisoned) => poisoned.into_inner(),
        };
        plans.push(plan);
        let mut records = match self.captured_records.lock() {
            Ok(records) => records,
            Err(poisoned) => poisoned.into_inner(),
        };
        records.push(record);
    }
}

impl fmt::Debug for ScriptedModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptedModel")
            .field("descriptor", &self.descriptor)
            .field("remaining_scenarios", &self.remaining_scenarios())
            .field("captured_plan_count", &self.captured_plans().len())
            .finish_non_exhaustive()
    }
}

impl LanguageModel for ScriptedModel {
    fn content_filter_retry_instruction(&self) -> Option<String> {
        self.content_filter_instruction.clone()
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        let result = self
            .runtime_authority
            .clone()
            .ok_or(RuntimeModelAuthorityError::Unavailable)
            .and_then(|authority| {
                authority.validate(&self.descriptor, checked_at)?;
                Ok(authority)
            });
        Box::pin(async move { result })
    }

    fn recover_call(
        self: Arc<Self>,
        request: ModelCallRecoveryRequest,
    ) -> ModelCallRecoveryOperation {
        let result = self.recovery.as_ref().map_or_else(
            || ModelCallRecoveryResult::unsupported(&request),
            |recovery| {
                if request.validate_for_descriptor(&self.descriptor).is_err() {
                    ModelCallRecoveryResult::unavailable(&request)
                } else {
                    recovery.recover(&request)
                }
            },
        );
        Box::pin(async move { result })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "scripted preparation keeps exact lowering, recovery receipt, and one-shot capture together"
    )]
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        let expected_authority = self
            .runtime_authority
            .as_ref()
            .ok_or(RuntimeModelAuthorityError::Unavailable)?;
        if !expected_authority.same_selected_authority(context.authority()) {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        let recovery_receipt = self
            .recovery
            .as_ref()
            .map(|_| {
                Digest::sha256(
                    DigestDomain::ProviderOperation,
                    1,
                    &[
                        b"plakous-model/scripted-provider-recovery-receipt-v1",
                        &plan.digest().identity_bytes(),
                    ],
                )
                .map(|digest| *digest.bytes())
            })
            .transpose()
            .map_err(PreparedCallError::from)?;
        let wire = recovery_receipt
            .map_or_else(
                || serde_json::to_vec(&(plan.request().model_input_identity(), plan.decisions())),
                |receipt| {
                    serde_json::to_vec(&(
                        plan.request().model_input_identity(),
                        plan.decisions(),
                        receipt,
                    ))
                },
            )
            .map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::Projection,
                    "scripted request projection serialization failed",
                )
            })?;
        let wire_size = u64::try_from(wire.len()).map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::ResourceLimit,
                "scripted request projection exceeds addressable size",
            )
        })?;
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-model/scripted-wire-v1", &wire],
        )
        .map_err(PreparedCallError::from)?;
        let summary_request = plan.request().input().items().iter().any(|item| {
            item.blocks().iter().any(|block| {
                matches!(
                    block,
                    plakous_model::ContentBlock::Text(text)
                        if text.text() == plakous_model::SEMANTIC_COMPACTION_INSTRUCTION
                )
            })
        });
        let observation = self.context_observation.clone().map(|observation| {
            // The paid summary stays on the scripted count so an oversized
            // request is still refused by its byte bound, not by this
            // projection. Proof measurements are ordinary history prepares.
            if !self.content_sensitive_observation || summary_request {
                return observation;
            }
            let bytes = plan
                .request()
                .input()
                .history()
                .segments()
                .iter()
                .fold(0usize, |total, segment| {
                    total.saturating_add(segment.canonical_bytes().len())
                });
            let tokens = u64::try_from(bytes).unwrap_or(u64::MAX).div_ceil(4).max(1);
            CountObservation::new(
                observation.certificate_id(),
                observation.certificate_revision(),
                tokens,
            )
            .unwrap_or(observation)
        });
        let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, observation)?;
        let output_limit = self
            .prepared_output_limit
            .or_else(|| plan.request().limits().max_output_tokens())
            .map(|tokens| {
                PreparedOutputLimit::enforced_wire_maximum(
                    tokens,
                    "max_output_tokens",
                    wire_digest,
                    "scripted-lowering-v1",
                )
            })
            .transpose()?;
        let (protocol_family, projection_revision, accounting_revision) =
            self.descriptor.context_contract().map_or_else(
                || {
                    (
                        self.descriptor.protocol().clone(),
                        "scripted-projection-v1".to_owned(),
                        "scripted-accounting-v1".to_owned(),
                    )
                },
                |contract| {
                    (
                        contract.protocol_family().clone(),
                        contract.projection_revision().to_owned(),
                        contract.accounting_revision().to_owned(),
                    )
                },
            );
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            protocol_family,
            "scripted-lowering-v1",
            projection_revision,
            accounting_revision,
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            output_limit,
        )?;
        let recovery_locator = recovery_receipt
            .map(|receipt| ProviderCallRecoveryLocator::seal(&projection, NonZeroU32::MIN, receipt))
            .transpose()
            .map_err(PreparedCallError::from)?;
        let record = PreparedCallRecord::for_prepared_with_recovery_locator(
            &self.descriptor,
            &plan,
            projection,
            &context,
            recovery_locator,
        )?;
        let recovery_request = record
            .recovery_locator()
            .map(|_| record.recovery_request())
            .transpose()
            .map_err(PreparedCallError::from)?;
        if let Some(recovery) = &self.recovery {
            recovery
                .register(record.clone())
                .map_err(PreparedCallError::from)?;
        }
        let captured_record = record.clone();
        let prepared_wire: Arc<[u8]> = wire.into();
        Ok(PreparedModelCall::new(record, move |call_context| {
            Box::pin(ScriptStartFuture {
                model: self,
                plan: Some(plan),
                record: Some(captured_record),
                prepared_wire: Some(prepared_wire),
                call_context: Some(call_context),
                recovery_request,
                scenario: None,
                loaded: false,
                completed: false,
            })
        }))
    }
}

struct ScriptStartFuture {
    model: Arc<ScriptedModel>,
    plan: Option<ValidatedRequestPlan>,
    record: Option<PreparedCallRecord>,
    prepared_wire: Option<Arc<[u8]>>,
    call_context: Option<ModelCallContext>,
    recovery_request: Option<ModelCallRecoveryRequest>,
    scenario: Option<ModelScenario>,
    loaded: bool,
    completed: bool,
}

impl Future for ScriptStartFuture {
    type Output = Result<ModelStream, ModelError>;

    #[allow(
        clippy::too_many_lines,
        reason = "the deterministic scripted start keeps its one-shot integrity and scenario transition auditable in one poll implementation"
    )]
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(call_context) = self.call_context.as_ref() else {
            self.completed = true;
            return Poll::Ready(Err(ModelError::new(
                ModelErrorKind::Protocol,
                "script start future polled after completion",
            )));
        };
        if call_context.cancellation().is_cancelled() {
            self.completed = true;
            return Poll::Ready(Err(ModelError::cancelled()));
        }
        if !self.loaded {
            let Some(plan) = self.plan.as_ref() else {
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "script start lost its validated request plan",
                )));
            };
            if let Err(error) = self.model.descriptor.validate_plan_binding(plan) {
                self.completed = true;
                return Poll::Ready(Err(ModelError::from(error)));
            }
            let Some(record) = self.record.as_ref() else {
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "script start lost its inert prepared record",
                )));
            };
            let Some(prepared_wire) = self.prepared_wire.as_ref() else {
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "script start lost its exact prepared projection",
                )));
            };
            let wire_digest = match Digest::sha256(
                DigestDomain::Blob,
                1,
                &[b"plakous-model/scripted-wire-v1", prepared_wire],
            ) {
                Ok(digest) => digest,
                Err(error) => {
                    self.completed = true;
                    return Poll::Ready(Err(ModelError::new(
                        ModelErrorKind::Protocol,
                        error.to_string(),
                    )));
                }
            };
            if wire_digest != record.projection().wire_digest()
                || u64::try_from(prepared_wire.len()).ok() != Some(record.projection().wire_size())
            {
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::Protocol,
                    "scripted prepared projection integrity drifted before start",
                )));
            }
            let Some(mut scenario) = self.model.pop_scenario() else {
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::ScriptExhausted,
                    "no scripted model scenario remains",
                )));
            };
            let matches = scenario.expected.matches(plan);
            let plan_digest = plan.digest();
            if !matches {
                self.model.restore_scenario(scenario);
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::ExpectedRequestMismatch,
                    format!("prepared request did not match scenario; plan digest {plan_digest}"),
                )));
            }
            if let Err(error) = scenario.bind_runtime_plan_digest(plan_digest) {
                self.model.restore_scenario(scenario);
                self.completed = true;
                return Poll::Ready(Err(error));
            }
            if let (Some(recovery), Some(request)) = (&self.model.recovery, &self.recovery_request)
                && !recovery.begin(request)
            {
                self.model.restore_scenario(scenario);
                self.completed = true;
                return Poll::Ready(Err(ModelError::new(
                    ModelErrorKind::OutcomeUnknown,
                    "scripted provider call locator was already started",
                )));
            }
            self.model.capture(plan.clone(), record.clone());
            self.scenario = Some(scenario);
            self.loaded = true;
        }

        let Some(scenario) = self.scenario.as_mut() else {
            self.completed = true;
            return Poll::Ready(Err(ModelError::new(
                ModelErrorKind::Protocol,
                "script start lost its scenario",
            )));
        };
        match &mut scenario.start {
            ScenarioStart::Immediate => self.finish_start(),
            ScenarioStart::Barrier(barrier) => {
                if barrier.is_released() {
                    self.finish_start()
                } else {
                    barrier.register(context.waker());
                    if let Some(call_context) = self.call_context.as_ref() {
                        register_cancellation(call_context.cancellation(), context);
                    }
                    Poll::Pending
                }
            }
            ScenarioStart::AwaitCancellation => {
                if let Some(call_context) = self.call_context.as_ref() {
                    register_cancellation(call_context.cancellation(), context);
                }
                Poll::Pending
            }
            ScenarioStart::DelayPolls(remaining) => {
                if *remaining == 0 {
                    self.finish_start()
                } else {
                    *remaining -= 1;
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            ScenarioStart::Error(error) => {
                let error = error.clone();
                if let (Some(recovery), Some(request)) =
                    (&self.model.recovery, &self.recovery_request)
                {
                    recovery.fail(request, error.clone());
                }
                self.completed = true;
                Poll::Ready(Err(error))
            }
        }
    }
}

impl ScriptStartFuture {
    fn finish_start(&mut self) -> Poll<Result<ModelStream, ModelError>> {
        let Some(scenario) = self.scenario.take() else {
            self.completed = true;
            return Poll::Ready(Err(ModelError::new(
                ModelErrorKind::Protocol,
                "script start lost its scenario",
            )));
        };
        let Some(call_context) = self.call_context.take() else {
            self.completed = true;
            return Poll::Ready(Err(ModelError::new(
                ModelErrorKind::Protocol,
                "script start lost its call context",
            )));
        };
        self.plan = None;
        self.record = None;
        self.prepared_wire = None;
        let recovery = self
            .model
            .recovery
            .clone()
            .zip(self.recovery_request.take());
        self.completed = true;
        Poll::Ready(Ok(Box::pin(ScriptStream {
            steps: scenario.steps.into(),
            call_context,
            terminal: false,
            completed_response: false,
            recovery,
        })))
    }
}

impl Drop for ScriptStartFuture {
    fn drop(&mut self) {
        if !self.completed
            && let Some(context) = &self.call_context
        {
            context.cancellation().cancel();
        }
    }
}

struct ScriptStream {
    steps: VecDeque<ScenarioStep>,
    call_context: ModelCallContext,
    terminal: bool,
    completed_response: bool,
    recovery: Option<(ScriptedRecoveryState, ModelCallRecoveryRequest)>,
}

impl Stream for ScriptStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }
        if self.completed_response {
            self.terminal = true;
            return Poll::Ready(None);
        }
        if self.call_context.cancellation().is_cancelled() {
            self.terminal = true;
            return Poll::Ready(Some(Err(ModelError::cancelled())));
        }
        loop {
            let Some(step) = self.steps.front_mut() else {
                self.terminal = true;
                return Poll::Ready(None);
            };
            match step {
                ScenarioStep::Emit(_) => {
                    let Some(ScenarioStep::Emit(event)) = self.steps.pop_front() else {
                        return Poll::Pending;
                    };
                    if let ModelEvent::ResponseCompleted(response) = &event {
                        self.completed_response = true;
                        if let Some((recovery, request)) = &self.recovery {
                            recovery.complete(request, response.clone());
                        }
                    }
                    return Poll::Ready(Some(Ok(event)));
                }
                ScenarioStep::Barrier(barrier) if barrier.is_released() => {
                    self.steps.pop_front();
                }
                ScenarioStep::Barrier(barrier) => {
                    barrier.register(context.waker());
                    register_cancellation(self.call_context.cancellation(), context);
                    return Poll::Pending;
                }
                ScenarioStep::DelayPolls(remaining) if *remaining != 0 => {
                    *remaining -= 1;
                    context.waker().wake_by_ref();
                    return Poll::Pending;
                }
                ScenarioStep::DelayPolls(_) => {
                    self.steps.pop_front();
                }
                ScenarioStep::AwaitCancellation => {
                    register_cancellation(self.call_context.cancellation(), context);
                    return Poll::Pending;
                }
                ScenarioStep::Error(_) => {
                    let Some(ScenarioStep::Error(error)) = self.steps.pop_front() else {
                        return Poll::Pending;
                    };
                    if let Some((recovery, request)) = &self.recovery {
                        recovery.fail(request, error.clone());
                    }
                    self.terminal = true;
                    return Poll::Ready(Some(Err(error)));
                }
                ScenarioStep::Eof => {
                    self.steps.pop_front();
                    self.terminal = true;
                    return Poll::Ready(None);
                }
            }
        }
    }
}

impl Drop for ScriptStream {
    fn drop(&mut self) {
        if !self.terminal && !self.completed_response {
            self.call_context.cancellation().cancel();
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    use uuid::Uuid;

    use futures_util::{FutureExt, StreamExt};

    use super::*;
    use plakous_model::{
        Actor, AttemptId, CancellationToken, Capability, CapabilityConstraints, CapabilitySet,
        CapacityCertificate, CapacityRule, ContentBlock, ContextContract, ContextEvidenceSource,
        ConversationItem, ConversationView, CountCertificateKind, DigestDomain, EntryId,
        EvidenceFreshness, FactProvenance, FactSource, FeatureSupport, FinishDetails, FinishReason,
        GenerationLimits, MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES,
        MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION, MeasurementCertificate,
        ModelAdapterProfileId, ModelCallRecoveryOutcome, ModelErrorContext, ModelErrorPhase,
        ModelId, ModelRetryability, ModelStreamItem, PREPARED_CALL_RECORD_SEMANTIC_REVISION,
        PROVIDER_OPERATION_SEMANTIC_REVISION, ProtocolId, Provenance,
        ProviderCallRecoveryLocatorStage, RequestMetadata, RouteId, RunId, SafeLimit, Support,
        TextBlock, Timestamp, TurnId, Usage, ValidationMode,
    };

    fn context_contract() -> ContextContract {
        let freshness =
            EvidenceFreshness::new(Timestamp::new(1, 0).expect("observed timestamp"), None)
                .expect("context freshness");
        ContextContract::new(
            ProtocolId::new("test-protocol").expect("protocol family"),
            "scripted-accounting-v1",
            "scripted-projection-v1",
            "scripted-reservation-v1",
            CapacityCertificate::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                "scripted-capacity",
                "scripted-capacity-v1",
                ContextEvidenceSource::BuiltInCatalog,
                CapacityRule::SharedWindow {
                    total: SafeLimit::Exact(1_000),
                },
                None,
                freshness,
            )
            .expect("capacity certificate"),
            vec![
                MeasurementCertificate::standard(
                    &plakous_model::StandardIntegrationAuthority::release_owned(),
                    "scripted-counter",
                    "scripted-exact-v1",
                    "scripted-complete-projection",
                    "scripted-method-v1",
                    ContextEvidenceSource::BuiltInCatalog,
                    CountCertificateKind::ExactTokenizer,
                    None,
                    freshness,
                )
                .expect("measurement certificate"),
            ],
        )
        .expect("context contract")
    }

    fn context_observation() -> CountObservation {
        CountObservation::new("scripted-counter", "scripted-exact-v1", 64)
            .expect("context observation")
    }

    fn descriptor() -> Result<ResolvedModelDescriptor, plakous_model::IdentityError> {
        let observed_at = Timestamp::new(1, 0)?;
        let mut features = BTreeMap::new();
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                FactProvenance::new(FactSource::TestFixture, observed_at, None),
            ),
        );
        Ok(ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test-protocol")?,
            RouteId::new("test-route")?,
            ModelId::new("test-model")?,
            None,
            None,
            ModelAdapterProfileId::new("test-profile")?,
            CapabilitySet::new(features, BTreeMap::new()),
        )
        .with_context_contract(context_contract()))
    }

    fn request(value: u128) -> Result<ModelRequest, Box<dyn std::error::Error>> {
        let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"scripted-test-selected"],
        )?)?)?;
        let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"scripted-test-effective"],
        )?)?)?;
        Ok(ModelRequest::builder(
            selected,
            effective,
            ConversationView::default(),
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(value))?,
                TurnId::try_from(Uuid::from_u128(value + 1))?,
                AttemptId::try_from(Uuid::from_u128(value + 2))?,
                Timestamp::new(1, 0)?,
            ),
        )
        .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
        .build()?)
    }

    fn authority(
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<RuntimeModelAuthority, Box<dyn std::error::Error>> {
        let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"script-account"])?;
        let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"script-credential"])?;
        Ok(RuntimeModelAuthority::new(
            descriptor,
            account,
            credential,
            1,
            Timestamp::new(1, 0)?,
            None,
        )?)
    }

    fn model(
        descriptor: ResolvedModelDescriptor,
        scenarios: impl IntoIterator<Item = ModelScenario>,
    ) -> Result<Arc<ScriptedModel>, Box<dyn std::error::Error>> {
        let authority = authority(&descriptor)?;
        Ok(Arc::new(
            ScriptedModel::new(descriptor, scenarios)
                .with_runtime_authority(authority)
                .with_context_observation(context_observation()),
        ))
    }

    fn recovery_model(
        descriptor: ResolvedModelDescriptor,
        scenarios: impl IntoIterator<Item = ModelScenario>,
        recovery: ScriptedRecoveryState,
    ) -> Result<Arc<ScriptedModel>, Box<dyn std::error::Error>> {
        let authority = authority(&descriptor)?;
        Ok(Arc::new(
            ScriptedModel::new(descriptor, scenarios)
                .with_runtime_authority(authority)
                .with_context_observation(context_observation())
                .with_call_recovery(recovery),
        ))
    }

    fn restarted_recovery_model(
        descriptor: &ResolvedModelDescriptor,
        recovery: &ScriptedRecoveryState,
    ) -> Result<Arc<ScriptedModel>, Box<dyn std::error::Error>> {
        let snapshot = recovery.snapshot()?;
        recovery_model(
            descriptor.clone(),
            [],
            ScriptedRecoveryState::from_snapshot(&snapshot)?,
        )
    }

    fn response(plan: Digest, label: &str) -> Result<ModelResponse, Box<dyn std::error::Error>> {
        let item = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(900))?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new(label, None)?)],
            Provenance::ModelOutput,
        )?;
        Ok(ModelResponse::new(
            vec![item],
            FinishDetails::new(FinishReason::Stop, None),
            Some(Usage::new(10, 2, 0, 0, 0)),
            None,
            plan,
        ))
    }

    fn prepare(
        model: &Arc<ScriptedModel>,
        request: ModelRequest,
    ) -> Result<(AttemptId, Digest, PreparedModelCall), Box<dyn std::error::Error>> {
        let attempt = request.metadata().attempt_id();
        let plan = model
            .descriptor()
            .validate_request(request, ValidationMode::Strict)?;
        let digest = plan.digest();
        let authority = model
            .runtime_authority
            .clone()
            .ok_or("scripted test authority missing")?;
        let prepared = Arc::clone(model).prepare_call(
            plan,
            ModelPreparationContext::new(authority, Timestamp::new(1, 0)?),
        )?;
        Ok((attempt, digest, prepared))
    }

    fn context(attempt: AttemptId, token: CancellationToken) -> ModelCallContext {
        ModelCallContext::new(attempt, token, None)
    }

    #[test]
    fn expected_request_mismatch_is_typed_and_redacted() -> Result<(), Box<dyn std::error::Error>> {
        let model = model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Exact(Box::new(request(10)?)),
                ScenarioStart::Immediate,
                vec![],
            )],
        )?;
        let (attempt, _, prepared) = prepare(&model, request(20)?)?;
        let result =
            futures::executor::block_on(prepared.start(context(attempt, CancellationToken::new())));
        let error = result.err();
        assert_eq!(
            error.as_ref().map(ModelError::kind),
            Some(ModelErrorKind::ExpectedRequestMismatch)
        );
        assert!(
            !error
                .as_ref()
                .is_some_and(|error| error.message().contains("ConversationView"))
        );
        assert_eq!(model.remaining_scenarios(), 1);
        Ok(())
    }

    #[test]
    fn cancellation_before_connect_is_terminal() -> Result<(), Box<dyn std::error::Error>> {
        let token = CancellationToken::new();
        token.cancel();
        let model = model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![],
            )],
        )?;
        let (attempt, _, prepared) = prepare(&model, request(10)?)?;
        let result = futures::executor::block_on(prepared.start(context(attempt, token)));
        assert_eq!(
            result.err().as_ref().map(ModelError::kind),
            Some(ModelErrorKind::Cancelled)
        );
        Ok(())
    }

    #[test]
    fn drop_before_start_has_no_dispatch_or_scenario_consumption()
    -> Result<(), Box<dyn std::error::Error>> {
        let model = model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
        )?;
        let (_, _, prepared) = prepare(&model, request(10)?)?;
        drop(prepared);
        assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
        assert_eq!(model.remaining_scenarios(), 1);
        Ok(())
    }

    #[test]
    fn prepared_call_is_non_clone_and_consumed_once() -> Result<(), Box<dyn std::error::Error>> {
        trait AmbiguousIfClone<A> {
            fn marker() {}
        }
        impl<T: ?Sized> AmbiguousIfClone<()> for T {}
        impl<T: Clone> AmbiguousIfClone<u8> for T {}
        let _ = <PreparedModelCall as AmbiguousIfClone<_>>::marker;

        let model = model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
        )?;
        let (attempt, _, prepared) = prepare(&model, request(10)?)?;
        let _stream = futures::executor::block_on(
            prepared.start(context(attempt, CancellationToken::new())),
        )?;
        assert_eq!(model.captured_records().len(), 1);
        assert_eq!(model.remaining_scenarios(), 0);
        Ok(())
    }

    #[test]
    fn prepared_output_limit_can_be_smaller_than_the_request_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let wire = NonZeroU32::new(64).ok_or("nonzero wire limit")?;
        let descriptor = descriptor()?;
        let request = request(10)?;
        assert_eq!(
            request.limits().max_output_tokens().map(NonZeroU32::get),
            Some(128)
        );
        let overridden = Arc::new(
            ScriptedModel::new(descriptor.clone(), [])
                .with_runtime_authority(authority(&descriptor)?)
                .with_context_observation(context_observation())
                .with_prepared_output_limit(wire),
        );
        let (_, _, prepared) = prepare(&overridden, request.clone())?;
        assert_eq!(
            prepared
                .record()
                .projection()
                .output_limit()
                .map(PreparedOutputLimit::tokens),
            Some(wire)
        );
        let baseline = Arc::new(
            ScriptedModel::new(descriptor.clone(), [])
                .with_runtime_authority(authority(&descriptor)?)
                .with_context_observation(context_observation()),
        );
        let (_, _, prepared) = prepare(&baseline, request)?;
        assert_eq!(
            prepared
                .record()
                .projection()
                .output_limit()
                .map(PreparedOutputLimit::tokens)
                .map(NonZeroU32::get),
            Some(128)
        );
        Ok(())
    }

    #[test]
    fn inert_record_roundtrip_cannot_dispatch() -> Result<(), Box<dyn std::error::Error>> {
        let expected = request(10)?;
        let model = model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Exact(Box::new(expected.clone())),
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
        )?;
        let plan = model
            .descriptor()
            .validate_request(expected.clone(), ValidationMode::Strict)?;
        let (attempt, digest, prepared) = prepare(&model, expected)?;
        let encoded = serde_json::to_vec(prepared.record())?;
        let decoded: PreparedCallRecord = serde_json::from_slice(&encoded)?;
        let cloned = decoded.clone();
        assert_eq!(decoded, cloned);
        decoded.revalidate(model.descriptor(), &plan, Timestamp::new(1, 0)?)?;
        assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
        assert_eq!(model.remaining_scenarios(), 1);
        let _stream = futures::executor::block_on(
            prepared.start(context(attempt, CancellationToken::new())),
        )?;
        assert_eq!(model.captured_plans()[0].digest(), digest);
        Ok(())
    }

    #[test]
    fn start_rejects_foreign_plan_before_consuming_scenario()
    -> Result<(), Box<dyn std::error::Error>> {
        let first_descriptor = descriptor()?;
        let second_descriptor = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            first_descriptor.protocol().clone(),
            RouteId::new("other-route")?,
            first_descriptor.requested_model().clone(),
            None,
            None,
            first_descriptor.profile().clone(),
            first_descriptor.capabilities().clone(),
        );
        let foreign_plan =
            second_descriptor.validate_request(request(10)?, ValidationMode::Strict)?;
        let model = model(
            first_descriptor,
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
        )?;
        let authority = model
            .runtime_authority
            .clone()
            .ok_or("scripted test authority missing")?;
        let result = Arc::clone(&model).prepare_call(
            foreign_plan,
            ModelPreparationContext::new(authority, Timestamp::new(1, 0)?),
        );
        assert!(matches!(result, Err(ModelPreparationError::Validation(_))));
        assert_eq!(model.remaining_scenarios(), 1);
        Ok(())
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one contract test keeps locator bounds, scope, stage, redaction, and tamper cases together"
    )]
    fn recovery_locator_is_exact_versioned_redacted_and_tamper_evident()
    -> Result<(), Box<dyn std::error::Error>> {
        let ordinary = model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
        )?;
        let (_, _, ordinary_prepared) = prepare(&ordinary, request(10)?)?;
        assert_eq!(
            ordinary_prepared.record().recovery_request(),
            Err(ModelCallRecoveryError::MissingLocator)
        );

        let shared = ScriptedRecoveryState::new();
        let exact_request = request(20)?;
        let recoverable = recovery_model(
            descriptor()?,
            [ModelScenario::new(
                ExpectedRequest::Exact(Box::new(exact_request.clone())),
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
            shared,
        )?;
        let (_, _, prepared) = prepare(&recoverable, exact_request)?;
        let record = prepared.record();
        let recovery = record.recovery_request()?;
        let locator = record.recovery_locator().ok_or("locator missing")?;
        assert_eq!(
            locator.availability_stage(),
            ProviderCallRecoveryLocatorStage::Prepared
        );
        assert_eq!(
            locator.operation_root(),
            record.projection().operation_root()
        );
        assert_eq!(
            record.semantic_revision(),
            PREPARED_CALL_RECORD_SEMANTIC_REVISION
        );
        assert_eq!(
            record.identity().version(),
            PROVIDER_OPERATION_SEMANTIC_REVISION
        );
        assert_eq!(recovery.prepared_call(), record.identity());
        assert_eq!(recovery.request(), record.projection().request());
        assert_eq!(recovery.plan(), record.projection().plan());
        assert_eq!(recovery.projection(), record.projection().identity());
        let exact_plan = recoverable
            .descriptor()
            .validate_request(request(20)?, ValidationMode::Strict)?;
        let expected_wire = serde_json::to_vec(&(
            exact_plan.request().model_input_identity(),
            exact_plan.decisions(),
            locator.provider_material(),
        ))?;
        assert_eq!(
            record.projection().wire_digest(),
            Digest::sha256(
                DigestDomain::Blob,
                1,
                &[b"plakous-model/scripted-wire-v1", &expected_wire],
            )?
        );
        let debug = format!("{locator:?}");
        assert!(debug.contains("redacted bytes"));
        assert!(!debug.contains(&format!("{:?}", locator.provider_material())));

        let learned = ProviderCallRecoveryLocator::learned_after_dispatch(
            record.projection(),
            NonZeroU32::new(2).ok_or("provider revision")?,
            [0_u8, 0xff, 0x80, 7],
        )?;
        assert_eq!(
            learned.availability_stage(),
            ProviderCallRecoveryLocatorStage::LearnedAfterDispatch
        );
        assert_eq!(learned.provider_material(), [0_u8, 0xff, 0x80, 7]);
        assert_eq!(
            record
                .recovery_request_with_locator(learned.clone())?
                .locator(),
            &learned
        );
        assert!(matches!(
            ModelStreamItem::RecoveryLocatorBound(learned.clone()),
            ModelStreamItem::RecoveryLocatorBound(bound) if bound == learned
        ));
        assert_eq!(
            ProviderCallRecoveryLocator::seal(record.projection(), NonZeroU32::MIN, [],),
            Err(ModelCallRecoveryError::InvalidProviderMaterial)
        );
        assert_eq!(
            ProviderCallRecoveryLocator::seal(
                record.projection(),
                NonZeroU32::MIN,
                vec![1; MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES + 1],
            ),
            Err(ModelCallRecoveryError::InvalidProviderMaterial)
        );
        let learned_value = serde_json::to_value(&learned)?;
        assert_eq!(
            serde_json::from_value::<ProviderCallRecoveryLocator>(learned_value.clone())?,
            learned
        );
        let mut stage_tamper = learned_value.clone();
        stage_tamper["availability_stage"] = serde_json::json!("Prepared");
        assert!(serde_json::from_value::<ProviderCallRecoveryLocator>(stage_tamper).is_err());
        let mut root_tamper = learned_value.clone();
        root_tamper["operation_root"] = serde_json::json!({
            "SessionWorkflow": vec![3_u8; 32]
        });
        assert!(serde_json::from_value::<ProviderCallRecoveryLocator>(root_tamper).is_err());
        let mut oversized = learned_value;
        oversized["provider_material"] =
            serde_json::json!(vec![1_u8; MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES + 1]);
        assert!(serde_json::from_value::<ProviderCallRecoveryLocator>(oversized).is_err());

        let encoded = serde_json::to_value(record)?;
        let decoded: PreparedCallRecord = serde_json::from_value(encoded.clone())?;
        assert_eq!(decoded, *record);
        decoded.recovery_request()?.validate_for_record(&decoded)?;

        let mut receipt = encoded.clone();
        let changed = receipt["recovery_locator"]["provider_material"][0]
            .as_u64()
            .map_or(1, |value| (value + 1) % 256);
        receipt["recovery_locator"]["provider_material"][0] = serde_json::json!(changed);
        assert!(serde_json::from_value::<PreparedCallRecord>(receipt).is_err());
        let mut revision = encoded.clone();
        revision["semantic_revision"] = serde_json::json!(3);
        assert!(serde_json::from_value::<PreparedCallRecord>(revision).is_err());
        let mut request_scope = encoded.clone();
        request_scope["recovery_locator"]["request"]["bytes"][0] = serde_json::json!(255);
        assert!(serde_json::from_value::<PreparedCallRecord>(request_scope).is_err());
        let mut plan_scope = encoded.clone();
        plan_scope["recovery_locator"]["plan"]["bytes"][0] = serde_json::json!(255);
        assert!(serde_json::from_value::<PreparedCallRecord>(plan_scope).is_err());
        let mut projection_scope = encoded.clone();
        projection_scope["recovery_locator"]["projection"]["bytes"][0] = serde_json::json!(255);
        assert!(serde_json::from_value::<PreparedCallRecord>(projection_scope).is_err());
        let mut provider_revision = encoded;
        provider_revision["recovery_locator"]["provider_revision"] = serde_json::json!(2);
        assert!(serde_json::from_value::<PreparedCallRecord>(provider_revision).is_err());

        let foreign = ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            recoverable.descriptor().protocol().clone(),
            RouteId::new("foreign-route")?,
            recoverable.descriptor().requested_model().clone(),
            None,
            None,
            recoverable.descriptor().profile().clone(),
            recoverable.descriptor().capabilities().clone(),
        );
        assert!(recovery.validate_for_descriptor(&foreign).is_err());
        Ok(())
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one restart matrix proves every closed scripted recovery classification and no duplicate start"
    )]
    fn scripted_recovery_survives_restart_and_starts_each_locator_at_most_once()
    -> Result<(), Box<dyn std::error::Error>> {
        let shared = ScriptedRecoveryState::new();
        let descriptor = descriptor()?;

        let not_dispatched_model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
            shared.clone(),
        )?;
        let (_, _, not_dispatched) = prepare(&not_dispatched_model, request(100)?)?;
        let not_dispatched_request = not_dispatched.record().recovery_request()?;
        let restarted = restarted_recovery_model(&descriptor, &shared)?;
        let recovered = futures::executor::block_on(crate::recover_model_operation(
            restarted,
            not_dispatched_request.clone(),
        ))?;
        assert!(matches!(
            recovered.outcome(),
            ModelCallRecoveryOutcome::ProvenNotDispatched
        ));
        assert_eq!(shared.start_count(&not_dispatched_request), 0);
        let unsupported = model(descriptor.clone(), [])?;
        let unavailable_in_ordinary_script =
            futures::executor::block_on(unsupported.recover_call(not_dispatched_request.clone()))?;
        assert!(matches!(
            unavailable_in_ordinary_script.outcome(),
            ModelCallRecoveryOutcome::Unsupported
        ));

        let cancelled_model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
            shared.clone(),
        )?;
        let (cancelled_attempt, _, cancelled) = prepare(&cancelled_model, request(200)?)?;
        let cancelled_request = cancelled.record().recovery_request()?;
        let token = CancellationToken::new();
        token.cancel();
        let error =
            futures::executor::block_on(cancelled.start(context(cancelled_attempt, token))).err();
        assert_eq!(
            error.as_ref().map(ModelError::kind),
            Some(ModelErrorKind::Cancelled)
        );
        let restarted = restarted_recovery_model(&descriptor, &shared)?;
        let recovered = futures::executor::block_on(
            Arc::clone(&restarted).recover_call(cancelled_request.clone()),
        )?;
        assert!(matches!(
            recovered.outcome(),
            ModelCallRecoveryOutcome::ProvenNotDispatched
        ));
        assert_eq!(shared.start_count(&cancelled_request), 0);

        let barrier = ScenarioBarrier::new();
        let ambiguous_input = request(300)?;
        let ambiguous_model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::Exact(Box::new(ambiguous_input.clone())),
                ScenarioStart::Barrier(barrier),
                vec![ScenarioStep::Eof],
            )],
            shared.clone(),
        )?;
        let (ambiguous_attempt, _, ambiguous) = prepare(&ambiguous_model, ambiguous_input.clone())?;
        let ambiguous_request = ambiguous.record().recovery_request()?;
        assert!(
            ambiguous
                .start(context(ambiguous_attempt, CancellationToken::new()))
                .now_or_never()
                .is_none()
        );
        let restarted = restarted_recovery_model(&descriptor, &shared)?;
        let recovered = futures::executor::block_on(
            Arc::clone(&restarted).recover_call(ambiguous_request.clone()),
        )?;
        assert!(matches!(
            recovered.outcome(),
            ModelCallRecoveryOutcome::Ambiguous
        ));
        assert_eq!(shared.start_count(&ambiguous_request), 1);

        let duplicate_model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
            shared.clone(),
        )?;
        let (duplicate_attempt, _, duplicate) = prepare(&duplicate_model, ambiguous_input)?;
        let duplicate_error = futures::executor::block_on(
            duplicate.start(context(duplicate_attempt, CancellationToken::new())),
        )
        .err();
        assert_eq!(
            duplicate_error.as_ref().map(ModelError::kind),
            Some(ModelErrorKind::OutcomeUnknown)
        );
        assert_eq!(shared.start_count(&ambiguous_request), 1);

        let completed_input = request(400)?;
        let completed_plan = descriptor
            .validate_request(completed_input.clone(), ValidationMode::Strict)?
            .digest();
        let completed_response = response(completed_plan, "completed")?;
        let completed_model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::PlanDigest(completed_plan),
                ScenarioStart::Immediate,
                vec![ScenarioStep::Emit(ModelEvent::ResponseCompleted(
                    completed_response.clone(),
                ))],
            )],
            shared.clone(),
        )?;
        let (completed_attempt, _, completed) = prepare(&completed_model, completed_input)?;
        let completed_request = completed.record().recovery_request()?;
        futures::executor::block_on(async {
            let mut stream = completed
                .start(context(completed_attempt, CancellationToken::new()))
                .await?;
            while let Some(event) = stream.next().await {
                event?;
            }
            Ok::<(), ModelError>(())
        })?;
        let restarted = restarted_recovery_model(&descriptor, &shared)?;
        let recovered = futures::executor::block_on(
            Arc::clone(&restarted).recover_call(completed_request.clone()),
        )?;
        match recovered.outcome() {
            ModelCallRecoveryOutcome::Completed(response) => {
                assert_eq!(response.as_ref(), &completed_response);
            }
            outcome => panic!("unexpected completed recovery outcome: {outcome:?}"),
        }
        assert_eq!(shared.start_count(&completed_request), 1);

        let failed_input = request(500)?;
        let definite = ModelError::new(ModelErrorKind::Protocol, "definite scripted failure");
        let failed_model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Error(definite.clone())],
            )],
            shared.clone(),
        )?;
        let (failed_attempt, _, failed) = prepare(&failed_model, failed_input)?;
        let failed_request = failed.record().recovery_request()?;
        let observed = futures::executor::block_on(async {
            let mut stream = failed
                .start(context(failed_attempt, CancellationToken::new()))
                .await?;
            stream.next().await.transpose()
        });
        assert_eq!(
            observed.err().as_ref().map(ModelError::kind),
            Some(ModelErrorKind::Protocol)
        );
        let restarted = restarted_recovery_model(&descriptor, &shared)?;
        let recovered =
            futures::executor::block_on(restarted.recover_call(failed_request.clone()))?;
        match recovered.outcome() {
            ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
                assert_eq!(error.as_ref(), &definite);
            }
            outcome => panic!("unexpected failed recovery outcome: {outcome:?}"),
        }
        assert_eq!(shared.start_count(&failed_request), 1);
        Ok(())
    }

    #[test]
    fn recovered_terminal_values_are_exact_and_tamper_evident()
    -> Result<(), Box<dyn std::error::Error>> {
        let shared = ScriptedRecoveryState::new();
        let descriptor = descriptor()?;
        let input = request(600)?;
        let plan = descriptor
            .validate_request(input.clone(), ValidationMode::Strict)?
            .digest();
        let model = recovery_model(
            descriptor.clone(),
            [ModelScenario::new(
                ExpectedRequest::Any,
                ScenarioStart::Immediate,
                vec![ScenarioStep::Eof],
            )],
            shared,
        )?;
        let (_, _, prepared) = prepare(&model, input)?;
        let request = prepared.record().recovery_request()?;
        let completed = ModelCallRecoveryResult::completed(
            &request,
            response(plan, "exact")?,
            Some(Usage::new(10, 2, 0, 0, 0)),
            None,
        )?;
        let encoded = serde_json::to_value(&completed)?;
        let decoded: ModelCallRecoveryResult = serde_json::from_value(encoded.clone())?;
        decoded.validate_for_request(&request)?;
        assert_eq!(
            decoded.identity().version(),
            PROVIDER_OPERATION_SEMANTIC_REVISION
        );
        assert_eq!(
            encoded["semantic_revision"],
            serde_json::json!(MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION)
        );
        let mut tampered = encoded;
        tampered["usage"]["output_tokens"] = serde_json::json!(3);
        assert!(serde_json::from_value::<ModelCallRecoveryResult>(tampered).is_err());

        let foreign = response(
            Digest::sha256(DigestDomain::RequestPlan, 99, &[b"foreign-plan"])?,
            "foreign",
        )?;
        assert!(ModelCallRecoveryResult::completed(&request, foreign, None, None).is_err());
        assert!(
            ModelCallRecoveryResult::definitely_failed(
                &request,
                ModelError::new(ModelErrorKind::OutcomeUnknown, "ambiguous"),
                None,
                None,
            )
            .is_err()
        );
        let nonterminal = ModelError::contextual(
            ModelErrorKind::Protocol,
            "not proved permanent",
            ModelErrorContext::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.requested_model().clone(),
                ModelErrorPhase::Terminal,
                ModelRetryability::Unknown,
            )?,
        )?;
        assert!(
            ModelCallRecoveryResult::definitely_failed(&request, nonterminal, None, None).is_err()
        );
        Ok(())
    }
}
