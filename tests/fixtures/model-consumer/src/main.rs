//! Exact supported Custom-model inventory and registration workflow.
//!
//! Other currently nameable implementation details are outside this direct
//! `plakous-model` support baseline.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use plakous_model::{
    CapabilitySet, LanguageModel, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ModelImplementationIdentity, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, NamespaceId, PreparedModelCall, ProtocolId, RegisteredLanguageModel,
    ResolvedModelDescriptor, RouteId, ValidatedRequestPlan,
};

struct ConsumerModel;

impl LanguageModel for ConsumerModel {
    fn prepare_call(
        self: Arc<Self>,
        _plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        if let Some(prefix) = context.completed_history_prefix() {
            let _ = context.with_completed_history_prefix(prefix);
        }
        Err(ModelPreparationError::rejected(
            ModelPreparationErrorKind::InvalidConfiguration,
            "compile-positive direct model is inert",
        ))
    }
}

fn register_custom_model() -> Result<RegisteredLanguageModel, Box<dyn Error>> {
    let descriptor = ResolvedModelDescriptor::new_custom(
        ProtocolId::new("consumer-protocol")?,
        RouteId::new("consumer-route")?,
        ModelId::new("consumer-model")?,
        None,
        Some(ModelFamilyId::new("consumer-family")?),
        ModelAdapterProfileId::new("consumer-profile")?,
        CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    );
    let implementation = ModelImplementationIdentity::custom(
        NamespaceId::new("consumer.model")?,
        1,
        NamespaceId::new("consumer.fixture")?,
    )?;
    Ok(RegisteredLanguageModel::custom(
        descriptor,
        implementation,
        Arc::new(ConsumerModel),
    )?)
}

fn main() {
    let registered = register_custom_model().expect("custom model registers");
    let _ = registered.executable();
    assert!(ConsumerModel.reachability_probe().is_none());
}
