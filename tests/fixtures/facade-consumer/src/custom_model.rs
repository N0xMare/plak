//! A caller-defined coarse model implementation through the root facade.

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use plakous::model::{
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
        _context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        Err(ModelPreparationError::rejected(
            ModelPreparationErrorKind::InvalidConfiguration,
            "compile-positive consumer model is inert",
        ))
    }
}

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<ConsumerModel>();
    let _ = std::any::type_name::<dyn LanguageModel>();
}

pub(crate) fn register_custom_model() -> Result<RegisteredLanguageModel, Box<dyn Error>> {
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
