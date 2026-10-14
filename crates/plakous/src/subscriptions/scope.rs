use super::{
    Arc, ConfiguredLanguageModel, LanguageModel, ModelCallRecoveryOperation,
    ModelCallRecoveryRequest, ModelError, ModelErrorKind, ModelOperationRoot, ModelOperationScope,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind,
    NativeModelResolutionError, NativeSubscriptionError, PreparedModelCall, ResolvedProvider,
    RuntimeModelAuthorityOperation, SubscriptionInferenceScope, SubscriptionInferenceScopeHandle,
    SubscriptionInferenceScopeLease, Timestamp, TurnScopedSubscriptionRequestGate,
    ValidatedRequestPlan,
};

struct TurnScopedNativeModel {
    inner: Arc<dyn LanguageModel>,
    gate: Arc<TurnScopedSubscriptionRequestGate>,
}

struct TurnScopedNativeRoot {
    _inner: Box<dyn ModelOperationScope>,
    _inference: SubscriptionInferenceScopeLease,
}

impl ModelOperationScope for TurnScopedNativeRoot {}

fn native_inference_scope(
    root: ModelOperationRoot,
) -> Result<SubscriptionInferenceScope, ModelPreparationError> {
    match root {
        ModelOperationRoot::Turn(turn) => {
            SubscriptionInferenceScope::turn(*turn.as_uuid().as_bytes())
        }
        ModelOperationRoot::SessionWorkflow(workflow) => {
            SubscriptionInferenceScope::session_workflow(*workflow.as_bytes())
        }
    }
    .map_err(|_| {
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::InvalidConfiguration,
            "native subscription inference root identity is invalid",
        )
    })
}

fn scope_native_prepared_call(
    prepared: PreparedModelCall,
    scope: SubscriptionInferenceScopeHandle,
) -> PreparedModelCall {
    prepared.scope_start(move || scope.enter())
}

impl LanguageModel for TurnScopedNativeModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        self.inner.reachability_probe()
    }

    fn content_filter_retry_instruction(&self) -> Option<String> {
        self.inner.content_filter_retry_instruction()
    }

    fn begin_operation_root(
        self: Arc<Self>,
        root: ModelOperationRoot,
    ) -> Result<Box<dyn ModelOperationScope>, ModelPreparationError> {
        let inner = Arc::clone(&self.inner).begin_operation_root(root)?;
        let inference = self
            .gate
            .claim_scope(native_inference_scope(root)?)
            .map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::ResourceLimit,
                    "native subscription inference root is unavailable",
                )
            })?;
        Ok(Box::new(TurnScopedNativeRoot {
            _inner: inner,
            _inference: inference,
        }))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Arc::clone(&self.inner).revalidate_runtime_authority(checked_at)
    }

    fn recover_call(
        self: Arc<Self>,
        request: ModelCallRecoveryRequest,
    ) -> ModelCallRecoveryOperation {
        Arc::clone(&self.inner).recover_call(request)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        // Context inspect prepares an ephemeral request without claiming a
        // root. Preparation must still run so the product meter can report
        // Fits/overflow; start stays fail-closed until a live root exists.
        let inference_scope = self
            .gate
            .bind_scope(native_inference_scope(plan.request().metadata().root())?)
            .ok();
        let prepared = Arc::clone(&self.inner).prepare_call(plan, context)?;
        Ok(match inference_scope {
            Some(scope) => scope_native_prepared_call(prepared, scope),
            None => {
                let record = prepared.record().clone();
                drop(prepared);
                PreparedModelCall::new(record, |_| {
                    Box::pin(async {
                        Err(ModelError::new(
                            ModelErrorKind::ResourceLimit,
                            "native subscription inference root is unavailable",
                        ))
                    })
                })
            }
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test gate limits are a fixed nonzero deadline"
)]
pub(crate) fn wrap_turn_scoped_native_model(
    inner: Arc<dyn LanguageModel>,
) -> Arc<dyn LanguageModel> {
    Arc::new(TurnScopedNativeModel {
        inner,
        gate: Arc::new(TurnScopedSubscriptionRequestGate::new(
            SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)
                .expect("turn-scoped test limits"),
            Arc::new(SystemSubscriptionGateClock::default()),
        )),
    })
}

pub(super) fn apply_turn_scoped_provider_gate(
    mut resolved: ResolvedProvider,
    turn_gate: Arc<TurnScopedSubscriptionRequestGate>,
) -> Result<ResolvedProvider, NativeModelResolutionError> {
    let wrapped: Arc<dyn LanguageModel> = Arc::new(TurnScopedNativeModel {
        inner: resolved.model.model(),
        gate: turn_gate,
    });
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        Arc::clone(&wrapped),
        resolved.configured.configuration().clone(),
        resolved.configured.initial_authority().clone(),
    )
    .map_err(|_| {
        NativeModelResolutionError::after_selection(
            resolved.model.selected_model.clone(),
            NativeSubscriptionError::ConfiguredAggregateRejected,
        )
    })?;
    resolved.model.model = wrapped;
    resolved.configured = configured;
    Ok(resolved)
}

#[cfg(test)]
use super::{Duration, SubscriptionRequestLimits, SystemSubscriptionGateClock};
