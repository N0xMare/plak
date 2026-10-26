use super::catalog::ApiModelStatus;
use super::listing::{CachedRows, Lookup};
use super::retention::exact_route_retention;
use super::{
    ApiCatalogSource, ApiModelCatalog, ApiModelLists, ApiToolMode, Arc, CliResult,
    ConfiguredLanguageModel, Construction, PlatformCredentials, PlatformModel, PlatformProvider,
    api_error, bundled_catalog,
};
pub(super) use plakous::xai::XaiListedModel as ListedModel;

/// Product-owned credentials for this compiled API route.
pub trait Credentials: Send + Sync + 'static {
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>>;
}
#[cfg(any(test, feature = "test-support"))]
#[path = "test_support/xai.rs"]
mod test_support;

pub(super) async fn construct(
    input: Construction<'_, '_>,
    clock: Arc<dyn plakous::model::Clock>,
) -> CliResult<ConfiguredLanguageModel> {
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

    let source = credentials.xai()?;
    let provisional = plakous::xai::xai_platform_selection_for(spec.literal_model, behavior)
        .map_err(api_error)?;
    let selected = plakous::xai::xai_platform_selection_for(
        spec.literal_model,
        exact_route_retention(&provisional),
    )
    .map_err(api_error)?;
    plakous::xai::XaiPlatformModel::preflight_configuration(&selected).map_err(api_error)?;
    let authority = source.grant().map_err(api_error)?.authority().clone();
    let model = Arc::new(
        plakous::xai::XaiPlatformModel::with_admitted_facts(
            authority,
            source,
            maximum,
            selected,
            Arc::clone(&clock),
            observer,
            &row.binding_evidence_for(provider, window),
        )
        .map_err(api_error)?,
    );
    spec.topology_extensions.connect_retrieval(Some(Arc::new(
        plakous::xai::XaiRetrievalPort::new(
            model.retrieval(plakous::xai::lowest_responses_effort(&row.efforts)),
        ),
    )))?;
    spec.topology_extensions
        .connect_image_gen(Some(Arc::new(plakous::xai::XaiImagePort::new(
            model.images(),
        ))))?;
    model.configured().await.map_err(api_error)
}

/// The xAI catalog over one model list, classified by the provider crate's
/// bundled table and family rules. A live window or effort above the reviewed
/// row is `NeedsReview`; a lower one narrows the row. The full input window is
/// the context less the reserved output bound.
pub(in crate::platform) fn catalog(
    listed: &[plakous::xai::XaiListedModel],
    source: ApiCatalogSource,
) -> Result<ApiModelCatalog, String> {
    use plakous::xai::XaiApiModelStatus as Status;
    let listed =
        plakous::xai::accept_xai_listed_models(listed).map_err(|error| error.to_string())?;
    let models = plakous::xai::xai_api_models(&listed)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|model| PlatformModel {
            model: model.id().to_owned(),
            status: match model.status() {
                Status::Usable => ApiModelStatus::Usable,
                Status::NeedsReview(reason) => ApiModelStatus::NeedsReview(reason.clone()),
                Status::NotCodingModel(reason) => ApiModelStatus::NotCodingModel(reason.clone()),
                _ => ApiModelStatus::NeedsReview("unknown provider status".to_owned()),
            },
            efforts: model.efforts().to_vec(),
            full_window: model.max_input_tokens(),
            price_threshold: model.price_threshold_input_tokens(),
            max_output_tokens: model.max_tokens(),
            tool_mode: ApiToolMode::Direct,
            deprecation: None,
            provider_facts: model.facts_digest(),
            anthropic: None,
        })
        .collect();
    Ok(ApiModelCatalog {
        provider: PlatformProvider::Xai,
        source,
        models,
    })
}

pub(super) fn bundled(source: ApiCatalogSource) -> ApiModelCatalog {
    let provider = PlatformProvider::Xai;
    plakous::xai::xai_api_bundled_listing()
        .map_err(|error| error.to_string())
        .and_then(|listed| catalog(&listed, source.clone()))
        .unwrap_or(ApiModelCatalog {
            provider,
            source,
            models: Vec::new(),
        })
}
pub(super) fn rows_valid(models: &[ListedModel]) -> bool {
    plakous::xai::accept_xai_listed_models(models).is_ok()
}

pub(super) async fn listing(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    product_home: Option<&std::path::Path>,
    lists: ApiModelLists,
    now: u64,
    observer: crate::exchange_observation::Observer,
) -> ApiModelCatalog {
    let source = match credentials.xai() {
        Ok(source) => source,
        Err(error) => return bundled_catalog(provider, error.message()),
    };
    let fingerprint = match plakous::xai::xai_api_key_fingerprint(source.as_ref()) {
        Ok(fingerprint) => fingerprint,
        Err(error) => return bundled_catalog(provider, &error.to_string()),
    };
    let lookup = Lookup::new(provider, product_home, &fingerprint, lists, now);
    lookup
        .run(
            async {
                plakous::xai::list_xai_api_models(source.as_ref(), observer)
                    .await
                    .map_err(|error| error.to_string())
            },
            CachedRows::Xai,
            |rows| match rows {
                CachedRows::Xai(models) => Some(models.clone()),
                CachedRows::Anthropic(_) | CachedRows::OpenAi(_) => None,
            },
            catalog,
        )
        .await
}
