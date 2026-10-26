use super::{
    ApiCatalogSource, ApiModelCatalog, ApiModelLists, CliFailure, CliResult,
    ConfiguredLanguageModel, Construction, PlatformCredentials, PlatformProvider,
};
pub trait Credentials: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> Credentials for T {}
pub(super) fn construct(
    input: Construction<'_, '_>,
) -> std::future::Ready<CliResult<ConfiguredLanguageModel>> {
    std::future::ready(Err(CliFailure::from(
        crate::capabilities::CapabilityUnavailable {
            id: input.provider.account(),
            kind: crate::capabilities::CapabilityKind::Route,
            feature: input.provider.account(),
        },
    )))
}
pub(super) fn listing(
    provider: PlatformProvider,
    _: &dyn PlatformCredentials,
    _: Option<&std::path::Path>,
    _: ApiModelLists,
    _: u64,
    _: crate::exchange_observation::Observer,
) -> std::future::Ready<ApiModelCatalog> {
    std::future::ready(ApiModelCatalog {
        provider,
        source: ApiCatalogSource::Bundled {
            reason: "not in this build".to_owned(),
        },
        models: Vec::new(),
    })
}
