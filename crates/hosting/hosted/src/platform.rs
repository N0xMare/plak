//! Direct API vocabulary with independent route implementations.
use crate::binding::{CompositionSelection, PlatformProvider};
mod catalog;
#[cfg(any(feature = "openai", feature = "anthropic", feature = "xai-api"))]
mod runtime;
#[cfg(not(any(feature = "openai", feature = "anthropic", feature = "xai-api")))]
#[path = "platform/runtime_unavailable.rs"]
mod runtime;
pub use catalog::{
    ApiAdmissionError, ApiCatalogSource, ApiModelCatalog, ApiModelStatus, ApiWindow, PlatformModel,
    bundled_catalog, effort_names,
};
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct UnavailableCredentials;
#[cfg(all(test, feature = "openai", feature = "anthropic", feature = "xai-api"))]
use runtime::listing;
#[cfg(all(test, feature = "openai", feature = "anthropic", feature = "xai-api"))]
pub(crate) use runtime::tests;
pub use runtime::{
    AnthropicCredentials, OpenAiCredentials, PlatformCredentials, XaiCredentials, api_model_catalog,
};
pub(crate) use runtime::{build_runtime, carry_input_window, openai_profile};
/// The tool mode an API model row is enrolled under.
///
/// An API key reads no authenticated Codex catalog, so the product owns this
/// fact per row (the reviewed `OPENAI_API_CODE_MODE_ONLY` table).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiToolMode {
    /// Structured tools only.
    Direct,
    /// The model is trained on Codex Code Mode: `plakous-default` binds
    /// `native-ptc`, and `direct` stays available as the second presentation.
    CodeModeOnly,
}

/// The composition an API session binds: `plakous-default` follows the row's
/// enrolled tool mode, and `native-ptc` (the vertical's own PTC system) is
/// admitted only where it is enrolled: Codex Code Mode on a `CodeModeOnly`
/// `openai` API row. The result is never `FollowCatalog`.
pub fn api_composition(
    provider: PlatformProvider,
    model: &str,
    selection: CompositionSelection,
) -> Result<CompositionSelection, &'static str> {
    let code_mode_only = platform_model(provider, model)
        .is_some_and(|row| row.tool_mode == ApiToolMode::CodeModeOnly);
    match selection {
        CompositionSelection::FollowCatalog if code_mode_only => {
            Ok(CompositionSelection::NativePtc)
        }
        CompositionSelection::FollowCatalog | CompositionSelection::Direct => {
            Ok(CompositionSelection::Direct)
        }
        CompositionSelection::JsPtc | CompositionSelection::PyPtc | CompositionSelection::PySdk => {
            Ok(selection)
        }
        CompositionSelection::NativePtc if code_mode_only => Ok(selection),
        CompositionSelection::NativePtc => Err(provider.native_ptc_refusal()),
    }
}

/// Every usable model one API route offers offline, in listing order: the
/// bundled reviewed table classified by the provider's family rules. Parse-time
/// validation, composition decisions and the Ready catalog read these rows;
/// session construction re-admits against the live list.
#[must_use]
pub fn platform_models(provider: PlatformProvider) -> Vec<PlatformModel> {
    bundled_catalog(provider, "offline")
        .models
        .into_iter()
        .filter(PlatformModel::usable)
        .collect()
}

/// The usable offline row for one exact literal on one API route.
#[must_use]
pub fn platform_model(provider: PlatformProvider, model: &str) -> Option<PlatformModel> {
    platform_models(provider)
        .into_iter()
        .find(|row| row.model == model)
}

/// What an API session is admitted under besides its spec: whether lists are
/// live, the window selection a new session binds, and, on exact reopen, the
/// semantic evidence the session was created with.
#[derive(Clone, Debug, Default)]
pub(crate) struct PlatformAdmission {
    pub lists: ApiModelLists,
    pub window: ApiWindow,
    pub created_evidence: Option<Vec<u8>>,
}

/// Whether session construction and model catalogs may fetch live lists.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApiModelLists {
    /// Cache and bundled facts only.
    #[default]
    Offline,
    /// Fetch on a missing or expired cache.
    Live,
}
