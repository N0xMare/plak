use super::{
    Arc, LanguageModel, ModelCallRecoveryOperation, ModelCallRecoveryRequest, ModelOperationRoot,
    ModelOperationScope, ModelPreparationContext, ModelPreparationError, PreparedModelCall,
    RuntimeModelAuthorityOperation, Timestamp, ValidatedRequestPlan,
};

#[cfg(test)]
mod tests;

struct GuidedModel {
    inner: Arc<dyn LanguageModel>,
    instruction: String,
}

pub(super) fn with_content_filter_guidance(
    inner: Arc<dyn LanguageModel>,
    override_text: Option<&str>,
) -> Arc<dyn LanguageModel> {
    Arc::new(GuidedModel {
        inner,
        instruction: plakous_prompts::content_filter::render(override_text),
    })
}

impl LanguageModel for GuidedModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        self.inner.reachability_probe()
    }

    fn content_filter_retry_instruction(&self) -> Option<String> {
        Some(self.instruction.clone())
    }

    fn begin_operation_root(
        self: Arc<Self>,
        root: ModelOperationRoot,
    ) -> Result<Box<dyn ModelOperationScope>, ModelPreparationError> {
        Arc::clone(&self.inner).begin_operation_root(root)
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
        Arc::clone(&self.inner).prepare_call(plan, context)
    }
}
