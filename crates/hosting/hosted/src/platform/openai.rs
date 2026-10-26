/// `openai` API models enrolled as `CodeModeOnly`. Reviewed 2026-10-07 against
/// the `tool_mode` of the same model ids in the Codex 0.162.1 reference
/// catalog; a test holds this table to that fixture.
pub(super) const OPENAI_API_CODE_MODE_ONLY: &[&str] = &[
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

fn openai_api_tool_mode(model: &str) -> ApiToolMode {
    if OPENAI_API_CODE_MODE_ONLY.contains(&model) {
        ApiToolMode::CodeModeOnly
    } else {
        ApiToolMode::Direct
    }
}

use super::catalog::ApiModelStatus;
use super::listing::{CachedRows, Lookup};
use super::retention::exact_route_retention;
use super::{
    ApiCatalogSource, ApiModelCatalog, ApiModelLists, ApiToolMode, Arc, CliFailure, CliResult,
    ConfiguredLanguageModel, Construction, PlatformCredentials, PlatformModel, PlatformProvider,
    api_error, bundled_catalog,
};
pub(super) use plakous::openai::OpenAiListedModel as ListedModel;
use plakous::openai::{
    OpenAiPlatformModel, OpenAiPlatformModelBuilder, OpenAiPlatformModelFacts,
    PlatformProjectPolicy, PlatformStoragePolicy,
};
pub(super) fn profile(model: &str) -> Option<&'static str> {
    OpenAiPlatformModelFacts::from_model_id(model).map(OpenAiPlatformModelFacts::profile_id)
}

/// Product-owned credentials for this compiled API route.
pub trait Credentials: Send + Sync + 'static {
    fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>>;
}
#[cfg(any(test, feature = "test-support"))]
#[path = "test_support/openai.rs"]
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

    let facts = OpenAiPlatformModelFacts::from_model_id(spec.literal_model)
        .ok_or_else(|| CliFailure::new("OpenAI API model is not enrolled"))?;
    let authority = OpenAiPlatformModelBuilder::new(
        openai_credential_reference()?,
        credentials.openai()?,
        facts,
        PlatformStoragePolicy::Stateless,
        PlatformProjectPolicy::DefaultProject,
    )
    .clock(Arc::clone(&clock))
    .context_envelope(maximum)
    .admitted_facts(row.binding_evidence_for(provider, window))
    .seal()
    .map_err(api_error)?;
    let provisional = authority
        .selected_configuration(behavior)
        .map_err(api_error)?;
    let selected = authority
        .selected_configuration(exact_route_retention(&provisional))
        .map_err(api_error)?;
    let model = Arc::new(
        OpenAiPlatformModel::with_exchange_observer(authority, selected, observer.clone())
            .map_err(api_error)?,
    );
    spec.topology_extensions.connect_retrieval(Some(Arc::new(
        plakous::openai::OpenAiRetrievalPort::new(
            model.retrieval(plakous::openai::lowest_responses_effort(&row.efforts)),
        ),
    )))?;
    spec.topology_extensions.connect_image_gen(Some(Arc::new(
        plakous::openai::OpenAiImagePort::new(model.images()),
    )))?;
    model.configured().await.map_err(api_error)
}
/// The credential reference the OpenAI API route resolves its key under, for
/// both session construction and the model listing.
pub(super) fn openai_credential_reference() -> CliResult<plakous::openai::CredentialReference> {
    plakous::openai::CredentialReference::application("plak/openai").map_err(api_error)
}

/// The OpenAI catalog over one id list, classified by the provider crate's
/// bundled table and family rules. The live list publishes ids only.
pub(in crate::platform) fn catalog(
    listed: &[plakous::openai::OpenAiListedModel],
    source: ApiCatalogSource,
) -> Result<ApiModelCatalog, String> {
    use plakous::openai::OpenAiApiModelStatus as Status;
    // Cached rows take the same id checks as a fetched page. The live payload
    // has no window, so it cannot raise a reviewed limit; facts stay the table's.
    let listed =
        plakous::openai::accept_openai_listed_models(listed).map_err(|error| error.to_string())?;
    let models = plakous::openai::openai_api_models(&listed)
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
            tool_mode: openai_api_tool_mode(model.id()),
            deprecation: None,
            provider_facts: model.facts_digest(),
            anthropic: None,
        })
        .collect();
    Ok(ApiModelCatalog {
        provider: PlatformProvider::OpenAi,
        source,
        models,
    })
}

pub(super) fn bundled(source: ApiCatalogSource) -> ApiModelCatalog {
    let provider = PlatformProvider::OpenAi;
    plakous::openai::openai_api_bundled_listing()
        .map_err(|error| error.to_string())
        .and_then(|listed| catalog(&listed, source.clone()))
        .unwrap_or(ApiModelCatalog {
            provider,
            source,
            models: Vec::new(),
        })
}
pub(super) fn rows_valid(models: &[ListedModel]) -> bool {
    plakous::openai::accept_openai_listed_models(models).is_ok()
}

pub(super) async fn listing(
    provider: PlatformProvider,
    credentials: &dyn PlatformCredentials,
    product_home: Option<&std::path::Path>,
    lists: ApiModelLists,
    now: u64,
    observer: crate::exchange_observation::Observer,
) -> ApiModelCatalog {
    let source = match credentials.openai() {
        Ok(source) => source,
        Err(error) => return bundled_catalog(provider, error.message()),
    };
    let reference = match openai_credential_reference() {
        Ok(reference) => reference,
        Err(error) => return bundled_catalog(provider, error.message()),
    };
    let fingerprint = match plakous::openai::openai_api_key_fingerprint(&reference, source.as_ref())
    {
        Ok(fingerprint) => fingerprint,
        Err(error) => return bundled_catalog(provider, &error.to_string()),
    };
    let lookup = Lookup::new(provider, product_home, &fingerprint, lists, now);
    lookup
        .run(
            async {
                plakous::openai::list_openai_api_models(&reference, source.as_ref(), observer)
                    .await
                    .map_err(|error| error.to_string())
            },
            CachedRows::OpenAi,
            |rows| match rows {
                CachedRows::OpenAi(models) => Some(models.clone()),
                CachedRows::Anthropic(_) | CachedRows::Xai(_) => None,
            },
            catalog,
        )
        .await
}
