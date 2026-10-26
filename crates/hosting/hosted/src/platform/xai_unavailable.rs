pub use super::unavailable::Credentials;
pub(super) use super::unavailable::listing;
pub(super) fn construct(
    input: super::Construction<'_, '_>,
    _: std::sync::Arc<dyn plakous::model::Clock>,
) -> std::future::Ready<crate::CliResult<plakous::model::ConfiguredLanguageModel>> {
    super::unavailable::construct(input)
}
use super::{ApiCatalogSource, ApiModelCatalog, PlatformProvider};
pub(super) type ListedModel = serde_json::Value;
pub(super) fn rows_valid(_: &[ListedModel]) -> bool {
    false
}
pub(super) fn bundled(source: ApiCatalogSource) -> ApiModelCatalog {
    ApiModelCatalog {
        provider: PlatformProvider::Xai,
        source,
        models: Vec::new(),
    }
}
