//! The model catalog seam the three direct API-key routes share.
//!
//! A route's catalog is the provider's own live model list (or its cache, or
//! the bundled reviewed table when neither is available) classified by the
//! provider crate's reviewed family rules. Everything the product decides
//! about an API model -- whether it is usable, its effort ladder, its working
//! and full window, its output bound -- is read from one [`PlatformModel`].

use plakous::agent::ReasoningEffort;

use crate::binding::PlatformProvider;

use super::ApiToolMode;

/// Whether Plak drives one listed API model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiModelStatus {
    /// A reviewed family with reviewed facts.
    Usable,
    /// Listed, but no reviewed family rule or table row admits it.
    NeedsReview(String),
    /// Listed, but not a text and tool coding model.
    NotCodingModel(String),
    /// Retired per the provider's lifecycle fields.
    Retired(String),
}

impl ApiModelStatus {
    /// The `plak models` status word.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Usable => "usable",
            Self::NeedsReview(_) => "needs review",
            Self::NotCodingModel(_) => "not a coding model",
            Self::Retired(_) => "retired",
        }
    }

    /// Why the model is refused, when it is.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Usable => None,
            Self::NeedsReview(reason) | Self::NotCodingModel(reason) | Self::Retired(reason) => {
                Some(reason)
            }
        }
    }
}

/// Which input window an API session is admitted under.
///
/// `Working` is the provider's price threshold where it publishes one (prompts
/// above it are billed at a higher rate), else the full window. Product config
/// `[api] full_window = true` selects `Full`. Both are provider input tokens:
/// the context meter compares provider-reported input usage (plus a
/// conservative byte charge for what was added since that report) against
/// them, never raw request bytes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApiWindow {
    #[default]
    Working,
    Full,
}

/// One model a direct API-key route lists, with the facts it runs under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformModel {
    pub model: String,
    pub status: ApiModelStatus,
    /// The model's published effort levels, in ladder order.
    pub efforts: Vec<ReasoningEffort>,
    /// Full input window.
    pub full_window: u64,
    /// Prompt size above which input is priced higher, when published.
    pub price_threshold: Option<u64>,
    /// The model's own output limit; the per-response bound a session sends.
    pub max_output_tokens: u32,
    /// The tool mode the product enrolls this row under.
    pub tool_mode: ApiToolMode,
    /// A lifecycle warning (deprecated, retiring), when any.
    pub deprecation: Option<String>,
    /// The provider crate's digest of its own facts for this id.
    pub provider_facts: String,
    /// The Anthropic crate's admitted model, which its builder takes.
    pub(crate) anthropic: Option<plakous::anthropic::AnthropicApiModel>,
}

const BINDING_MARKER: &str = ";plak.api-model-binding.v2=";
const WINDOW_MARKER: &str = ";window=";

impl PlatformModel {
    /// The admitted input window under one selection.
    #[must_use]
    pub fn window(&self, window: ApiWindow) -> u64 {
        match window {
            ApiWindow::Working => self.price_threshold.map_or(self.full_window, |threshold| {
                threshold.min(self.full_window)
            }),
            ApiWindow::Full => self.full_window,
        }
    }

    /// The per-response output bound a session sends: the model's own limit.
    #[must_use]
    pub const fn output_bound(&self) -> u32 {
        self.max_output_tokens
    }

    /// Whether the product admits this row.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.status == ApiModelStatus::Usable
    }

    /// Digest of the admitted facts that change what a session sends: the
    /// provider's facts for this id (family or profile, effort ladder, output
    /// limit), the output bound and the tool mode. The window a session runs
    /// under is bound beside it as the number it chose; pricing thresholds and
    /// hosted-feature records are not bound, so a table edit to them does not
    /// break an exact reopen.
    #[must_use]
    pub fn binding_digest(&self, provider: PlatformProvider) -> String {
        let efforts = effort_names(&self.efforts).join(",");
        let text = format!(
            "{}\n{}\n{}\n{efforts}\n{}\n{:?}",
            provider.account(),
            self.model,
            self.provider_facts,
            self.max_output_tokens,
            self.tool_mode,
        );
        plakous::model::sha256_hex(&[b"plak.api-model-binding.v2", text.as_bytes()])
    }

    /// The bytes a provider appends to its semantic evidence so the admitted
    /// facts and the selected window are identity-bearing for the session.
    #[must_use]
    pub fn binding_evidence(&self, provider: PlatformProvider, window: ApiWindow) -> Vec<u8> {
        self.binding_evidence_for(provider, self.window(window))
    }

    /// [`Self::binding_evidence`] for an exact window in provider tokens.
    #[must_use]
    pub fn binding_evidence_for(&self, provider: PlatformProvider, window_tokens: u64) -> Vec<u8> {
        format!(
            "{BINDING_MARKER}{}{WINDOW_MARKER}{window_tokens};",
            self.binding_digest(provider),
        )
        .into_bytes()
    }

    /// The window, in provider tokens, a session created under `stored`
    /// evidence keeps on exact reopen: the number it was created with, as
    /// long as the model's full window still covers it.
    ///
    /// # Errors
    /// Returns an operator-facing refusal with the rebind remedy when the
    /// bound facts changed since the session was created.
    pub fn reopen_window(&self, provider: PlatformProvider, stored: &[u8]) -> Result<u64, String> {
        let changed = || {
            format!(
                "{} model `{}` changed since this session was created (its provider-published facts or this build's reviewed table moved); rebind with Fork now in the Agents tab, or start a new session",
                provider.account(),
                self.model
            )
        };
        let (digest, window) = stored_binding(stored).ok_or_else(changed)?;
        if digest != self.binding_digest(provider) || window == 0 || window > self.full_window {
            return Err(changed());
        }
        Ok(window)
    }
}

fn stored_binding(stored: &[u8]) -> Option<(String, u64)> {
    let marker = BINDING_MARKER.as_bytes();
    let start = stored
        .windows(marker.len())
        .position(|window| window == marker)?
        + marker.len();
    let rest = stored.get(start..)?;
    let digest = std::str::from_utf8(rest.get(..64)?).ok()?.to_owned();
    let rest = rest.get(64..)?.strip_prefix(WINDOW_MARKER.as_bytes())?;
    let end = rest.iter().position(|byte| *byte == b';')?;
    let window = std::str::from_utf8(rest.get(..end)?).ok()?.parse().ok()?;
    Some((digest, window))
}

/// Where a catalog's rows came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiCatalogSource {
    /// Fetched from the provider now.
    Live,
    /// The cached provider list, fetched `age_seconds` ago.
    Cached { age_seconds: u64 },
    /// The bundled reviewed table: the list was unavailable.
    Bundled { reason: String },
}

/// Every model one API route lists, in provider order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiModelCatalog {
    pub provider: PlatformProvider,
    pub source: ApiCatalogSource,
    pub models: Vec<PlatformModel>,
}

/// Why an API model selection is refused before any inference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiAdmissionError {
    /// The id is not on the provider's list.
    Unknown {
        provider: PlatformProvider,
        model: String,
        suggestions: Vec<String>,
    },
    /// Listed, but refused with a reason.
    Refused {
        provider: PlatformProvider,
        model: String,
        status: ApiModelStatus,
    },
    /// The effort is not on the model's published ladder.
    Effort {
        provider: PlatformProvider,
        model: String,
        offered: Vec<ReasoningEffort>,
    },
}

impl std::fmt::Display for ApiAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown {
                provider,
                model,
                suggestions,
            } => {
                write!(f, "{} does not list model `{model}`", provider.account())?;
                if !suggestions.is_empty() {
                    write!(f, "; closest: {}", suggestions.join(", "))?;
                }
                write!(f, "; see `plak models {}`", provider.account())
            }
            Self::Refused {
                provider,
                model,
                status,
            } => write!(
                f,
                "{} model `{model}` is {}: {}",
                provider.account(),
                status.label(),
                status.reason().unwrap_or_default()
            ),
            Self::Effort {
                provider,
                model,
                offered,
            } => write!(
                f,
                "{} model `{model}` requires --effort {}",
                provider.account(),
                effort_names(offered).join("|")
            ),
        }
    }
}

/// Lowercase effort names, in ladder order.
#[must_use]
pub fn effort_names(efforts: &[ReasoningEffort]) -> Vec<&'static str> {
    efforts
        .iter()
        .map(|effort| match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::Max => "max",
            ReasoningEffort::None => "none",
            _ => "other",
        })
        .collect()
}

impl ApiModelCatalog {
    /// The row for one exact listed id.
    #[must_use]
    pub fn find(&self, model: &str) -> Option<&PlatformModel> {
        self.models.iter().find(|row| row.model == model)
    }

    /// Admits one exact id: listed, usable, and (when given) at an effort on
    /// its ladder.
    ///
    /// # Errors
    /// Returns the typed refusal; unknown ids carry up to three closest
    /// listed ids.
    pub fn admit(
        &self,
        model: &str,
        effort: Option<ReasoningEffort>,
    ) -> Result<&PlatformModel, ApiAdmissionError> {
        let Some(row) = self.find(model) else {
            return Err(ApiAdmissionError::Unknown {
                provider: self.provider,
                model: model.to_owned(),
                suggestions: closest(model, self.models.iter().map(|row| row.model.as_str())),
            });
        };
        if !row.usable() {
            return Err(ApiAdmissionError::Refused {
                provider: self.provider,
                model: model.to_owned(),
                status: row.status.clone(),
            });
        }
        if effort.is_none_or(|effort| !row.efforts.contains(&effort)) {
            return Err(ApiAdmissionError::Effort {
                provider: self.provider,
                model: model.to_owned(),
                offered: row.efforts.clone(),
            });
        }
        Ok(row)
    }
}

/// Up to three listed ids closest to `model` by edit distance, nearest first.
fn closest<'a>(model: &str, listed: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut ranked: Vec<(usize, &str)> = listed
        .map(|candidate| (edit_distance(model, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= candidate.len().max(model.len()) / 2)
        .collect();
    ranked.sort_unstable();
    ranked
        .into_iter()
        .take(3)
        .map(|(_, candidate)| candidate.to_owned())
        .collect()
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, left_char) in left.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, right_char) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != *right_char);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[right.len()]
}

/// The Anthropic catalog over one listing, classified by the provider
/// crate's bundled table and family rules.
pub(crate) fn anthropic_catalog(
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

/// The OpenAI catalog over one id list, classified by the provider crate's
/// bundled table and family rules. The live list publishes ids only.
pub(crate) fn openai_catalog(
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
            tool_mode: super::openai_api_tool_mode(model.id()),
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

/// The xAI catalog over one model list, classified by the provider crate's
/// bundled table and family rules. A live window or effort above the reviewed
/// row is `NeedsReview`; a lower one narrows the row. The full input window is
/// the context less the reserved output bound.
pub(crate) fn xai_catalog(
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

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The catalog a route has without its live list: the bundled reviewed table,
/// classified at the current time.
#[must_use]
pub fn bundled_catalog(provider: PlatformProvider, reason: &str) -> ApiModelCatalog {
    let source = ApiCatalogSource::Bundled {
        reason: reason.to_owned(),
    };
    match provider {
        PlatformProvider::Anthropic => plakous::anthropic::anthropic_api_bundled_listing()
            .map_err(|error| error.to_string())
            .and_then(|listed| anthropic_catalog(&listed, source.clone(), unix_now()))
            .unwrap_or(ApiModelCatalog {
                provider,
                source,
                models: Vec::new(),
            }),
        PlatformProvider::OpenAi => plakous::openai::openai_api_bundled_listing()
            .map_err(|error| error.to_string())
            .and_then(|listed| openai_catalog(&listed, source.clone()))
            .unwrap_or(ApiModelCatalog {
                provider,
                source,
                models: Vec::new(),
            }),
        PlatformProvider::Xai => plakous::xai::xai_api_bundled_listing()
            .map_err(|error| error.to_string())
            .and_then(|listed| xai_catalog(&listed, source.clone()))
            .unwrap_or(ApiModelCatalog {
                provider,
                source,
                models: Vec::new(),
            }),
    }
}

#[cfg(test)]
mod tests;
