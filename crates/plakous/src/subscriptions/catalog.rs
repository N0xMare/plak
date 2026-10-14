//! Private catalog encoding; admission and credential authority stay with the workflow.

use serde::{Deserialize, Serialize};

use super::{
    NativeModelCatalog, NativeModelSummary, NativeModelVisibility, NativeProvider,
    NativeServiceTierSummary, NativeSubscriptionError, NativeToolMode, ReasoningEffort, unix_now,
};

const PERSISTED_CATALOG_SCHEMA: u32 = 1;
pub(super) const MAX_PERSISTED_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
const PERSISTED_CATALOG_TTL_SECS: u64 = 900;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNativeModelCatalog {
    schema: u32,
    provider: String,
    observed_at_unix: u64,
    refreshed_at_unix: u64,
    expires_at_unix: u64,
    models: Vec<PersistedNativeModelSummary>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNativeModelSummary {
    exact_id: String,
    visibility: PersistedModelVisibility,
    native_tool_mode: Option<PersistedNativeToolMode>,
    streaming: bool,
    tools: bool,
    reasoning_efforts: Vec<ReasoningEffort>,
    default_reasoning_effort: Option<ReasoningEffort>,
    service_tiers: Vec<PersistedNativeServiceTier>,
    default_service_tier: Option<String>,
    reported_context_length: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistedModelVisibility {
    List,
    Hide,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistedNativeToolMode {
    Direct,
    CodeMode,
    CodeModeOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedNativeServiceTier {
    id: String,
    name: String,
    description: String,
}

const fn persisted_visibility(visibility: NativeModelVisibility) -> PersistedModelVisibility {
    match visibility {
        NativeModelVisibility::List => PersistedModelVisibility::List,
        NativeModelVisibility::Hide => PersistedModelVisibility::Hide,
        NativeModelVisibility::None => PersistedModelVisibility::None,
    }
}

const fn facade_persisted_visibility(
    visibility: PersistedModelVisibility,
) -> NativeModelVisibility {
    match visibility {
        PersistedModelVisibility::List => NativeModelVisibility::List,
        PersistedModelVisibility::Hide => NativeModelVisibility::Hide,
        PersistedModelVisibility::None => NativeModelVisibility::None,
    }
}

const fn persisted_tool_mode(mode: NativeToolMode) -> PersistedNativeToolMode {
    match mode {
        NativeToolMode::Direct => PersistedNativeToolMode::Direct,
        NativeToolMode::CodeMode => PersistedNativeToolMode::CodeMode,
        NativeToolMode::CodeModeOnly => PersistedNativeToolMode::CodeModeOnly,
    }
}

const fn facade_persisted_tool_mode(mode: PersistedNativeToolMode) -> NativeToolMode {
    match mode {
        PersistedNativeToolMode::Direct => NativeToolMode::Direct,
        PersistedNativeToolMode::CodeMode => NativeToolMode::CodeMode,
        PersistedNativeToolMode::CodeModeOnly => NativeToolMode::CodeModeOnly,
    }
}

fn persisted_catalog_from_models(
    provider: NativeProvider,
    models: &[NativeModelSummary],
    observed_at_unix: u64,
    refreshed_at_unix: u64,
    expires_at_unix: u64,
) -> PersistedNativeModelCatalog {
    PersistedNativeModelCatalog {
        schema: PERSISTED_CATALOG_SCHEMA,
        provider: provider.as_str().to_owned(),
        observed_at_unix,
        refreshed_at_unix,
        expires_at_unix,
        models: models
            .iter()
            .map(|model| PersistedNativeModelSummary {
                exact_id: model.exact_id().to_owned(),
                visibility: persisted_visibility(model.visibility()),
                native_tool_mode: model.native_tool_mode().map(persisted_tool_mode),
                streaming: model.supports_streaming(),
                tools: model.supports_tools(),
                reasoning_efforts: model.reasoning_efforts().to_vec(),
                default_reasoning_effort: model.default_reasoning_effort(),
                service_tiers: model
                    .service_tiers()
                    .iter()
                    .map(|tier| PersistedNativeServiceTier {
                        id: tier.id().to_owned(),
                        name: tier.name().to_owned(),
                        description: tier.description().to_owned(),
                    })
                    .collect(),
                default_service_tier: model.default_service_tier().map(str::to_owned),
                reported_context_length: model.reported_context_length(),
            })
            .collect(),
    }
}

fn native_catalog_from_persisted(
    provider: NativeProvider,
    persisted: PersistedNativeModelCatalog,
) -> Result<NativeModelCatalog, NativeSubscriptionError> {
    if persisted.schema != PERSISTED_CATALOG_SCHEMA || persisted.provider != provider.as_str() {
        return Err(NativeSubscriptionError::ModelUnavailable);
    }
    let models: Vec<NativeModelSummary> = persisted
        .models
        .into_iter()
        .map(|model| NativeModelSummary {
            exact_id: model.exact_id,
            visibility: facade_persisted_visibility(model.visibility),
            native_tool_mode: model.native_tool_mode.map(facade_persisted_tool_mode),
            streaming: model.streaming,
            tools: model.tools,
            reasoning_efforts: model.reasoning_efforts,
            default_reasoning_effort: model.default_reasoning_effort,
            service_tiers: model
                .service_tiers
                .into_iter()
                .map(|tier| NativeServiceTierSummary {
                    id: tier.id,
                    name: tier.name,
                    description: tier.description,
                })
                .collect(),
            default_service_tier: model.default_service_tier,
            reported_context_length: model.reported_context_length,
        })
        .collect();
    if models.is_empty() {
        return Err(NativeSubscriptionError::ModelUnavailable);
    }
    Ok(NativeModelCatalog { provider, models })
}

pub(super) fn decode_persisted_catalog(
    bytes: &[u8],
    provider: NativeProvider,
) -> Result<NativeModelCatalog, NativeSubscriptionError> {
    let persisted: PersistedNativeModelCatalog =
        serde_json::from_slice(bytes).map_err(|_| NativeSubscriptionError::ModelUnavailable)?;
    native_catalog_from_persisted(provider, persisted)
}

pub(super) fn encode_catalog_snapshot(
    provider: NativeProvider,
    models: &[NativeModelSummary],
) -> Result<Vec<u8>, NativeSubscriptionError> {
    let now = unix_now()?;
    let expires_at_unix = now
        .checked_add(PERSISTED_CATALOG_TTL_SECS)
        .ok_or(NativeSubscriptionError::Internal)?;
    let snapshot = persisted_catalog_from_models(provider, models, now, now, expires_at_unix);
    let bytes = serde_json::to_vec(&snapshot).map_err(|_| NativeSubscriptionError::Internal)?;
    if bytes.len() as u64 > MAX_PERSISTED_CATALOG_BYTES {
        return Err(NativeSubscriptionError::Internal);
    }
    Ok(bytes)
}
