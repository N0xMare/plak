use super::catalog::ApiModelStatus;
use super::listing::{CachedRows, Lookup};
use super::{
    ApiCatalogSource, ApiModelCatalog, ApiModelLists, ApiToolMode, Arc, CliFailure, CliResult,
    ConfiguredLanguageModel, Construction, PlatformCredentials, PlatformModel, PlatformProvider,
    api_error, bundled_catalog, unix_now,
};
pub(in crate::platform) use plakous::anthropic::AnthropicApiModel as AdmittedModel;
pub(super) use plakous::anthropic::AnthropicListedModel as ListedModel;

/// Product-owned credentials for this compiled API route.
pub trait Credentials: Send + Sync + 'static {
    fn anthropic(
        &self,
        _account: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::anthropic::AnthropicKeySource>>;
}
#[cfg(any(test, feature = "test-support"))]
#[path = "test_support/anthropic.rs"]
mod test_support;

pub(super) fn construct(
    input: Construction<'_, '_>,
) -> std::future::Ready<CliResult<ConfiguredLanguageModel>> {
    std::future::ready(construct_now(input))
}

fn construct_now(input: Construction<'_, '_>) -> CliResult<ConfiguredLanguageModel> {
    let Construction {
        provider,
        credentials,
        spec,
        row,
        behavior,
        maximum,
        window,
        observer,
    } = input;
    let model = row
        .anthropic
        .as_ref()
        .ok_or_else(|| CliFailure::new("Anthropic API model facts are unavailable"))?;
    let (configured, retrieval) =
        plakous::anthropic::AnthropicLanguageModel::configured_with_retrieval(
            credentials.anthropic(&spec.account)?,
            &spec.account.source_authority("plak/anthropic"),
            model,
            &row.binding_evidence_for(provider, window),
            behavior,
            maximum,
            observer.clone(),
        )
        .map_err(api_error)?;
    spec.topology_extensions.connect_retrieval(Some(Arc::new(
        plakous::anthropic::AnthropicRetrievalPort::new(retrieval),
    )))?;
    Ok(configured)
}

/// The Anthropic catalog over one listing, classified by the provider
/// crate's bundled table and family rules.
pub(in crate::platform) fn catalog(
    listed: &[plakous::anthropic::AnthropicListedModel],
    source: ApiCatalogSource,
    now_unix_seconds: u64,
) -> Result<ApiModelCatalog, String> {
    use plakous::anthropic::AnthropicApiModelStatus as Status;
    let models = plakous::anthropic::anthropic_api_models(listed, now_unix_seconds)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|model| PlatformModel {
            model: model.id().to_owned(),
            status: match model.status() {
                Status::Usable => ApiModelStatus::Usable,
                Status::NeedsReview(reason) => ApiModelStatus::NeedsReview(reason.clone()),
                Status::NotCodingModel(reason) => ApiModelStatus::NotCodingModel(reason.clone()),
                Status::Retired(reason) => ApiModelStatus::Retired(reason.clone()),
                _ => ApiModelStatus::NeedsReview("unknown provider status".to_owned()),
            },
            efforts: model.efforts().to_vec(),
            full_window: model.max_input_tokens(),
            price_threshold: model.price_threshold_input_tokens(),
            max_output_tokens: model.max_tokens(),
            tool_mode: ApiToolMode::Direct,
            deprecation: model.deprecation().map(str::to_owned),
            provider_facts: model.facts_digest(),
            anthropic: Some(model),
        })
        .collect();
    Ok(ApiModelCatalog {
        provider: PlatformProvider::Anthropic,
        source,
        models,
    })
}

pub(super) fn bundled(source: ApiCatalogSource) -> ApiModelCatalog {
    let provider = PlatformProvider::Anthropic;
    plakous::anthropic::anthropic_api_bundled_listing()
        .map_err(|error| error.to_string())
        .and_then(|listed| catalog(&listed, source.clone(), unix_now()))
        .unwrap_or(ApiModelCatalog {
            provider,
            source,
            models: Vec::new(),
        })
}
pub(super) fn rows_valid(models: &[ListedModel]) -> bool {
    plakous::anthropic::accept_anthropic_listed_models(models).is_ok()
}

pub(super) async fn listing(
    provider: PlatformProvider,
    account: &crate::account::AccountRef,
    credentials: &dyn PlatformCredentials,
    product_home: Option<&std::path::Path>,
    lists: ApiModelLists,
    now: u64,
    observer: crate::exchange_observation::Observer,
) -> ApiModelCatalog {
    let source = match credentials.anthropic(account) {
        Ok(source) => source,
        Err(error) => return bundled_catalog(provider, error.message()),
    };
    let fingerprint = match plakous::anthropic::anthropic_api_key_fingerprint(source.as_ref()) {
        Ok(fingerprint) => fingerprint,
        Err(error) => return bundled_catalog(provider, &error.to_string()),
    };
    let lookup = Lookup::new(provider, account, product_home, &fingerprint, lists, now);
    lookup
        .run(
            async {
                plakous::anthropic::list_anthropic_api_models(source.as_ref(), observer)
                    .await
                    .map_err(|error| error.to_string())
            },
            CachedRows::Anthropic,
            |rows| match rows {
                CachedRows::Anthropic(models) => Some(models.clone()),
                CachedRows::OpenAi(_) | CachedRows::Xai(_) => None,
            },
            |models, source| catalog(models, source, now),
        )
        .await
}
