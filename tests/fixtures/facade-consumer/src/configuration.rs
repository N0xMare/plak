//! Complete model/configuration binding through the root facade.

use std::sync::Arc;

use plakous::model::{
    CompiledProviderConfiguration, ConfiguredLanguageModel, LanguageModel,
    ProviderConfigurationError, RuntimeModelAuthority,
};

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<CompiledProviderConfiguration>();
    let _ = std::any::type_name::<ConfiguredLanguageModel>();
}

#[allow(dead_code)]
pub(crate) fn bind_exact_configuration(
    model: Arc<dyn LanguageModel>,
    configuration: CompiledProviderConfiguration,
    authority: RuntimeModelAuthority,
) -> Result<ConfiguredLanguageModel, ProviderConfigurationError> {
    ConfiguredLanguageModel::custom(model, configuration, authority)
}

#[allow(dead_code)]
pub(crate) fn bind_host_process_prefix(
    builder: plakous::agent::AgentBuilder,
) -> Result<plakous::agent::AgentBuilder, plakous::tools::ExecutionEnvironmentError> {
    let options =
        plakous::tools::HostProcessOptions::new(Some(std::path::PathBuf::from("process-bin")))?;
    Ok(builder.host_process_options(options))
}
