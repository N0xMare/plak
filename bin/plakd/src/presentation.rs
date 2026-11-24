//! Host provider/model catalogs derived from offline native-subscription state,
//! and the small CLI formatting helpers the command surfaces share.

use std::env;
use std::path::PathBuf;

use plakous::agent::ReasoningEffort;
use plakous::application::ApplicationExecutor;
use plakous::subscriptions::{
    NativeModelVisibility, NativeProvider, NativeSubscriptions, NativeToolMode, SubscriptionPaths,
};
pub use plakous_hosted::runtime::HostLaunchNativeModelFact;

use crate::cli_output;
use crate::commands::support::native_subscription_failure;
use crate::home::paths::{self, PlakPaths};
use crate::product_config::ProductCompositionSelection;
use crate::store::sessions as product_sessions;
use plakous_hosted::error::{CliFailure, CliResult};

/// Maximum pasted authorization input for remote OAuth completion.
pub const MAX_REMOTE_AUTHORIZATION_INPUT_BYTES: usize = 16 * 1024;

/// Offline compatibility posture for a native provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompatibilityState {
    /// Exact acceptance is required before use.
    Required,
    /// Profile is current.
    Current,
    /// Profile is inactive.
    Inactive,
    /// Profile is unsupported on this platform.
    Unsupported,
}

/// Offline credential posture for a native provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialState {
    /// No credential stored.
    SignedOut,
    /// Credential is ready for use.
    Ready,
    /// Credential needs refresh.
    RefreshRequired,
    /// Credential is quarantined.
    Quarantined,
    /// A refresh claimed by a live process is in flight.
    Refreshing,
    /// Credential was revoked.
    Revoked,
    /// A stored login this build cannot use; a new login is required.
    LoginRequired,
    /// A stored login written under another compatibility profile, or by an
    /// older build whose format reads as one; a new login replaces it.
    ProfileChanged,
    /// A newer build wrote the stored login; this build leaves it untouched.
    WrittenByNewerBuild,
    /// Credential state is unsupported.
    Unsupported,
}

/// Combined offline compatibility and credential status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliStatus {
    /// Native compatibility posture.
    pub compatibility: CompatibilityState,
    /// Native credential posture.
    pub credential: CredentialState,
}

/// One native catalog model row for host catalog presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliModelSummary {
    /// Live contract refusal; absent on an admitted row.
    pub unavailable_reason: Option<String>,
    /// Exact provider model id.
    pub exact_id: String,
    /// Catalog visibility.
    pub visibility: NativeModelVisibility,
    /// Native tool mode when present.
    pub native_tool_mode: Option<NativeToolMode>,
    /// Whether streaming is supported.
    pub streaming: bool,
    /// Whether tools are supported.
    pub tools: bool,
    /// Supported reasoning efforts.
    pub reasoning_efforts: Vec<ReasoningEffort>,
    /// Exact catalog processing-tier ids with provider name and description.
    pub service_tiers: Vec<(String, String, String)>,
    /// Provider-reported context length when known.
    pub reported_context_length: Option<u64>,
}

/// Provider rows plus the Ready catalog and full-catalog native model facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostCatalogBundle {
    /// Provider rows for external clients auth picker.
    pub providers: Vec<plakous_host_protocol::views::ProviderDef>,
    /// The exact Ready catalog: one section per account for the hosted session.
    pub sections: plakous_host_protocol::views::ReadyCatalog,
    /// Authenticated native catalog facts before picker filters.
    pub native_models: Vec<HostLaunchNativeModelFact>,
}

/// Frozen product coding tuples. Catalogs may list other enrolled ids.
pub const PRODUCT_RECOMMENDED_CODEX_MODEL: &str = crate::capabilities::RECOMMENDED_CODEX_MODEL;

/// Returns the frozen product recommendation for an OAuth account.
#[must_use]
pub fn product_recommended_model(account: &str) -> Option<&'static str> {
    crate::capabilities::recommended_model(account)
}

/// Select the recommended model id when it is present in `models`.
#[must_use]
pub fn prefer_product_model<'a, I>(account: &str, models: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let recommended = product_recommended_model(account)?;
    let mut first = None;
    for model in models {
        if first.is_none() {
            first = Some(model);
        }
        if model == recommended {
            return Some(model);
        }
    }
    first
}

struct NativePresentationBackend {
    executor: ApplicationExecutor,
    subscriptions: NativeSubscriptions,
    state_home: PathBuf,
}

impl NativePresentationBackend {
    fn new(
        home: &PlakPaths,
        state_home: PathBuf,
        custody: &crate::gateway::HostCustody,
    ) -> CliResult<Self> {
        let paths = SubscriptionPaths::from_xdg_state_home(state_home.clone())
            .map_err(|error| native_subscription_failure(error, None, &state_home))?;
        let executor = ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("application executor construction failed"))?;
        let subscriptions = custody
            .subscriptions(NativeSubscriptions::new(paths))
            .with_gateway_catalog_cache(home.home());
        Ok(Self {
            executor,
            subscriptions,
            state_home,
        })
    }
    fn offline_status(&mut self, provider: NativeProvider) -> CliResult<CliStatus> {
        self.executor
            .block_on(crate::commands::support::offline_subscription_status(
                &self.subscriptions,
                provider,
            ))
            .map_err(application_execution_failure)?
            .map_err(|error| native_subscription_failure(error, Some(provider), &self.state_home))
    }

    fn models(&mut self, provider: NativeProvider) -> CliResult<Vec<CliModelSummary>> {
        let catalog = self
            .executor
            .block_on(self.subscriptions.model_catalog_persisted(provider))
            .map_err(application_execution_failure)?
            .map_err(|error| {
                native_subscription_failure(error, Some(provider), &self.state_home)
            })?;
        if catalog.provider() != provider {
            return Err(CliFailure::new("native model catalog provider mismatch"));
        }
        Ok(catalog
            .models()
            .iter()
            .map(|model| CliModelSummary {
                unavailable_reason: model.unavailable_reason().map(str::to_owned),
                exact_id: model.exact_id().to_owned(),
                visibility: model.visibility(),
                native_tool_mode: model.native_tool_mode(),
                streaming: model.supports_streaming(),
                tools: model.supports_tools(),
                reasoning_efforts: model.reasoning_efforts().to_vec(),
                service_tiers: model
                    .service_tiers()
                    .iter()
                    .map(|tier| {
                        (
                            tier.id().to_owned(),
                            tier.name().to_owned(),
                            tier.description().to_owned(),
                        )
                    })
                    .collect(),
                reported_context_length: model.reported_context_length(),
            })
            .collect())
    }
}

/// Build host catalog rows and full-catalog native model facts from the
/// persisted catalog snapshot. Does not refresh live catalogs.
pub(crate) fn catalog_bundle(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
) -> CliResult<HostCatalogBundle> {
    let state_home = paths::application_state_home().map_err(CliFailure::new)?;
    catalog_bundle_at(paths, custody, Some(&state_home))
}

pub(crate) fn catalog_bundle_at(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
    state_home: Option<&std::path::Path>,
) -> CliResult<HostCatalogBundle> {
    let Some(gateway) = custody.admin() else {
        return Ok(HostCatalogBundle {
            providers: Vec::new(),
            sections: plakous_host_protocol::views::ReadyCatalog::default(),
            native_models: Vec::new(),
        });
    };
    let mut backend = state_home
        .map(|state| NativePresentationBackend::new(paths, state.to_path_buf(), custody))
        .transpose()?;
    Ok(catalog_bundle_with_backend(
        paths,
        backend.as_mut(),
        ProductCompositionSelection::FollowCatalog,
        gateway,
    ))
}

/// Build provider and model sections for external clients from the persisted catalog
/// snapshot. Does not refresh live catalogs.
pub(crate) fn catalog_values(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
) -> CliResult<(
    Vec<plakous_host_protocol::views::ProviderDef>,
    Vec<plakous_host_protocol::views::ProviderSection>,
)> {
    let bundle = catalog_bundle(paths, custody)?;
    Ok((bundle.providers, bundle.sections.accounts))
}

/// Pure catalog projection shared by the session picker and home console.
pub(crate) fn native_model_entries(
    provider: NativeProvider,
    models: Vec<CliModelSummary>,
    composition: ProductCompositionSelection,
) -> Vec<plakous_host_protocol::views::ModelEntry> {
    let entries = models
        .into_iter()
        .filter(|model| model.visibility == NativeModelVisibility::List)
        .filter(|model| provider.enrolls(&model.exact_id))
        .map(|model| native_model_entry(provider, model, composition))
        .collect::<Option<Vec<_>>>();
    entries.unwrap_or_default()
}

fn native_model_entry(
    provider: NativeProvider,
    model: CliModelSummary,
    composition: ProductCompositionSelection,
) -> Option<plakous_host_protocol::views::ModelEntry> {
    let qualified_code_mode_only = provider == NativeProvider::Codex
        && provider.enrolled_tool_mode(&model.exact_id) == Some(NativeToolMode::CodeModeOnly)
        && model.native_tool_mode == Some(NativeToolMode::CodeModeOnly)
        && !model.reasoning_efforts.is_empty();
    let unavailable_reason = if provider != NativeProvider::Codex
        || model.native_tool_mode == Some(NativeToolMode::Direct)
        || qualified_code_mode_only
    {
        None
    } else {
        Some(
            match model.native_tool_mode {
                Some(NativeToolMode::CodeMode) => "hybrid Code Mode is not yet exactly qualified",
                Some(NativeToolMode::CodeModeOnly) => {
                    "this Code Mode-only tuple is not yet exactly qualified"
                }
                Some(NativeToolMode::Direct) => unreachable!(),
                None => "catalog tool mode is unrecognized",
            }
            .to_owned(),
        )
    };
    // Codex `ultra` lowers to provider effort `max` and
    // separately enables proactive multi-agent delegation.
    // It is therefore an orchestration mode, not a portable
    // single-agent reasoning effort.
    let reasoning_efforts = model
        .reasoning_efforts
        .into_iter()
        .filter(|effort| *effort != ReasoningEffort::Ultra)
        .collect::<Vec<_>>();
    let direct_composition_capable =
        plakous_hosted::agent::direct_composition_capable(provider, model.native_tool_mode);
    let composition_refusal = match composition {
        ProductCompositionSelection::JsPtc
        | ProductCompositionSelection::PyPtc
        | ProductCompositionSelection::PySdk
            if direct_composition_capable =>
        {
            None
        }
        ProductCompositionSelection::JsPtc
        | ProductCompositionSelection::PyPtc
        | ProductCompositionSelection::PySdk => Some(
            "Plak-owned custom composition requires exact Direct-capable catalog authority"
                .to_owned(),
        ),
        _ => None,
    };
    let unavailable_reason = model
        .unavailable_reason
        .or(composition_refusal)
        .or(unavailable_reason);
    let reasoning = !reasoning_efforts.is_empty();
    Some(plakous_host_protocol::views::ModelEntry {
        tag: [
            model.streaming.then_some("stream"),
            model.tools.then_some("tools"),
            reasoning.then_some("reasoning"),
            model.native_tool_mode.map(native_tool_mode_label),
            unavailable_reason.as_ref().map(|_| "not qualified"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · "),
        effort_options: model_effort_options(&reasoning_efforts)?,
        extras: model
            .service_tiers
            .into_iter()
            .map(
                |(id, name, description)| plakous_host_protocol::views::ExtraState {
                    enabled: false,
                    id,
                    label: format!("{name} · {description}"),
                },
            )
            .collect(),
        unavailable_reason,
        id: model.exact_id,
        reasoning,
    })
}

fn catalog_bundle_with_backend(
    paths: &PlakPaths,
    mut backend: Option<&mut NativePresentationBackend>,
    composition: ProductCompositionSelection,
    gateway: &crate::gateway::AdminLink,
) -> HostCatalogBundle {
    let oauth = crate::capabilities::ROUTES.iter().filter_map(|capability| {
        capability.route.native_provider().map(|provider| {
            (
                capability.route.account,
                capability.label,
                capability.short_label,
                provider,
            )
        })
    });
    let mut providers = Vec::new();
    let mut sections = Vec::new();
    let mut native_models = Vec::new();
    for (account, label, short, provider) in oauth {
        let Some(backend) = backend.as_deref_mut() else {
            break;
        };
        let status = backend.offline_status(provider);
        // Spec 02 §5.5 states: a stored-but-unusable credential reads `expired`,
        // an absent one reads `not logged in`, and a store that cannot be read
        // reads `unavailable`. Quarantine after an ambiguous refresh is not
        // expiry: it reads `refresh outcome unknown`. Compatibility trouble is
        // a credential the operator cannot use until consent is re-taken, so
        // it is `expired`.
        let (state, mut detail, admitted) = match status {
            Ok(CliStatus {
                compatibility: CompatibilityState::Current,
                credential: CredentialState::Ready,
            }) => (
                plakous_host_protocol::views::ProviderState::Ready,
                String::new(),
                true,
            ),
            Ok(CliStatus {
                compatibility: CompatibilityState::Required,
                ..
            }) => (
                plakous_host_protocol::views::ProviderState::Expired,
                "acceptance required".to_owned(),
                false,
            ),
            Ok(CliStatus {
                compatibility: CompatibilityState::Inactive,
                ..
            }) => (
                plakous_host_protocol::views::ProviderState::Expired,
                "compatibility inactive".to_owned(),
                false,
            ),
            Ok(CliStatus {
                compatibility: CompatibilityState::Unsupported,
                ..
            }) => (
                plakous_host_protocol::views::ProviderState::Unavailable,
                "unsupported platform".to_owned(),
                false,
            ),
            Ok(CliStatus { credential, .. }) => match credential {
                CredentialState::SignedOut => (
                    plakous_host_protocol::views::ProviderState::NotLoggedIn,
                    String::new(),
                    false,
                ),
                CredentialState::RefreshRequired => (
                    plakous_host_protocol::views::ProviderState::Expired,
                    "refresh required".to_owned(),
                    false,
                ),
                CredentialState::Quarantined => (
                    plakous_host_protocol::views::ProviderState::RefreshUnknown,
                    "quarantined".to_owned(),
                    false,
                ),
                CredentialState::Refreshing => (
                    plakous_host_protocol::views::ProviderState::Ready,
                    "refreshing".to_owned(),
                    false,
                ),
                CredentialState::Revoked => (
                    plakous_host_protocol::views::ProviderState::Expired,
                    "revoked".to_owned(),
                    false,
                ),
                CredentialState::LoginRequired => (
                    plakous_host_protocol::views::ProviderState::Expired,
                    "login made by an earlier build; log in again".to_owned(),
                    false,
                ),
                CredentialState::ProfileChanged => (
                    plakous_host_protocol::views::ProviderState::Expired,
                    "login from an older build or another profile; log in again".to_owned(),
                    false,
                ),
                CredentialState::WrittenByNewerBuild => (
                    plakous_host_protocol::views::ProviderState::Unavailable,
                    "written by a newer plakd build".to_owned(),
                    false,
                ),
                CredentialState::Ready | CredentialState::Unsupported => (
                    plakous_host_protocol::views::ProviderState::Unavailable,
                    "unsupported credential state".to_owned(),
                    false,
                ),
            },
            Err(_) => (
                plakous_host_protocol::views::ProviderState::Unavailable,
                "credential store unreadable".to_owned(),
                false,
            ),
        };
        if admitted {
            match backend.models(provider) {
                Ok(models) => {
                    native_models.extend(models.iter().map(|model| HostLaunchNativeModelFact {
                        account: account.to_owned(),
                        exact_id: model.exact_id.clone(),
                        native_tool_mode: model.native_tool_mode,
                        reasoning_efforts: model.reasoning_efforts.clone(),
                        service_tiers: model.service_tiers.clone(),
                    }));
                    let mut entries = native_model_entries(provider, models, composition);
                    if entries.is_empty() {
                        "no picker-visible supported models".clone_into(&mut detail);
                    } else if let Some(recommended) = product_recommended_model(account) {
                        entries.sort_by_key(|entry| entry.id != recommended);
                    }
                    sections.push(plakous_host_protocol::views::ProviderSection {
                        account: account.to_owned(),
                        label: short.to_owned(),
                        ready: true,
                        entries,
                    });
                }
                Err(_) => "catalog unavailable".clone_into(&mut detail),
            }
        }
        if (matches!(detail.as_str(), "refresh required" | "quarantined")
            || state == plakous_host_protocol::views::ProviderState::Ready)
            && let Some(failure) =
                crate::product_doctor::last_failed_refresh(paths, account, crate::local_time_zone())
        {
            detail = if state == plakous_host_protocol::views::ProviderState::Ready
                && !detail.is_empty()
            {
                format!("{failure}; {detail}")
            } else {
                failure
            };
        }
        // A subscription credential exposes no offline account identity, so
        // no label is invented for it.
        providers.push(plakous_host_protocol::views::ProviderDef {
            id: account.to_owned(),
            label: label.to_owned(),
            kind: plakous_host_protocol::views::AuthKind::Oauth,
            state,
            detail,
            account_label: None,
        });
    }
    for (account, label) in crate::capabilities::ROUTES
        .iter()
        .filter(|capability| {
            capability.auth_kind() == plakous_host_protocol::views::AuthKind::ApiKey
        })
        .map(|capability| (capability.route.account, capability.label))
    {
        // The only account identity an API key supplies is its fingerprint,
        // or the environment variable that won. Neither is key material.
        let (state, detail, account_label) = gateway_key_presentation(gateway, account);
        if let Some(provider) = plakous_hosted::binding::PlatformProvider::for_account(account) {
            sections.push(plakous_host_protocol::views::ProviderSection {
                account: account.to_owned(),
                label: format!("{label} API"),
                ready: state == plakous_host_protocol::views::ProviderState::Ready,
                entries: if state == plakous_host_protocol::views::ProviderState::Ready {
                    crate::platform_catalog::entries(provider, composition)
                } else {
                    Vec::new()
                },
            });
        }
        providers.push(plakous_host_protocol::views::ProviderDef {
            id: account.to_owned(),
            label: label.to_owned(),
            kind: plakous_host_protocol::views::AuthKind::ApiKey,
            state,
            detail,
            account_label,
        });
    }
    HostCatalogBundle {
        providers,
        sections: plakous_host_protocol::views::ReadyCatalog::new(sections),
        native_models,
    }
}

fn gateway_key_presentation(
    gateway: &crate::gateway::AdminLink,
    account: &str,
) -> (
    plakous_host_protocol::views::ProviderState,
    String,
    Option<String>,
) {
    use plakous::gateway::protocol::facts::KeySource;
    use plakous_host_protocol::views::ProviderState;
    match gateway.status(account) {
        Ok(facts) => match facts.and_then(|facts| facts.key) {
            Some(key) if key.source != KeySource::Missing => (
                ProviderState::Ready,
                key.env_var
                    .map_or_else(|| "stored".to_owned(), |name| name.as_str().to_owned()),
                key.fingerprint
                    .map(|fingerprint| fingerprint.as_str().to_owned()),
            ),
            _ => (ProviderState::NoKey, String::new(), None),
        },
        Err(_) => (
            ProviderState::Unavailable,
            "gateway key status unavailable".to_owned(),
            None,
        ),
    }
}

fn model_effort_options(efforts: &[ReasoningEffort]) -> Option<Vec<String>> {
    efforts
        .iter()
        .filter(|effort| **effort != ReasoningEffort::Ultra)
        .map(|effort| match effort {
            ReasoningEffort::Low => Some("low".to_owned()),
            ReasoningEffort::Medium => Some("medium".to_owned()),
            ReasoningEffort::High => Some("high".to_owned()),
            ReasoningEffort::XHigh => Some("xhigh".to_owned()),
            ReasoningEffort::Max => Some("max".to_owned()),
            ReasoningEffort::Ultra => None,
            ReasoningEffort::None => Some("none".to_owned()),
            ReasoningEffort::TokenBudget(_) => None,
            _ => None,
        })
        .collect()
}

const fn native_tool_mode_label(mode: NativeToolMode) -> &'static str {
    match mode {
        NativeToolMode::Direct => "direct",
        NativeToolMode::CodeMode => "code_mode",
        NativeToolMode::CodeModeOnly => "code_mode_only",
    }
}

fn application_execution_failure<T>(_: T) -> CliFailure {
    CliFailure::new("application execution failed")
}

pub(crate) fn transcript_entry(
    row: &product_sessions::TranscriptRow,
) -> Option<(&'static str, &str)> {
    use product_sessions::TranscriptRow;
    match row {
        TranscriptRow::User { text, .. } => Some(("User", text)),
        TranscriptRow::Steer { text, .. } => Some(("Steer", text)),
        TranscriptRow::Assistant { text } => Some(("Assistant", text)),
        TranscriptRow::Notice(notice)
            if notice.placement() != product_sessions::NoticePlacement::SessionLog =>
        {
            Some(("Note", &notice.text))
        }
        _ => None,
    }
}

/// First meaningful transcript snippet for session lists / resume picker.
pub(crate) fn session_preview_line(session: &product_sessions::SessionRecord) -> String {
    if let Some(title) = session
        .title_override
        .as_deref()
        .or(session.title.as_deref())
        .filter(|t| !t.is_empty())
    {
        return title.chars().take(48).collect();
    }
    for row in session.transcript.iter() {
        let (product_sessions::TranscriptRow::User { text, .. }
        | product_sessions::TranscriptRow::Assistant { text }) = row
        else {
            continue;
        };
        let body = text.trim();
        if !body.is_empty() {
            return body.chars().take(48).collect();
        }
    }
    "(empty)".to_owned()
}

pub(crate) fn cli_human_output() -> cli_output::HumanOutput {
    cli_output::HumanOutput::resolve(
        cli_output::TerminalMode::of_stdout(),
        env::var_os("NO_COLOR").as_deref(),
        cli_output::DetailLevel::Summary,
    )
}

pub(crate) fn product_account_label(account: Option<&str>) -> String {
    account.map_or_else(
        || "—".to_owned(),
        |account| {
            if account
                .parse::<plakous_hosted::account::AccountRef>()
                .is_ok_and(|selection| selection.is_named())
            {
                account.to_owned()
            } else {
                cli_output::ProviderLabel::new(account).to_string()
            }
        },
    )
}

pub(crate) fn format_unix_seconds(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|timestamp| {
            timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| seconds.to_string())
}

pub(crate) fn format_unix_millis(milliseconds: u64) -> String {
    let nanos = i128::from(milliseconds).saturating_mul(1_000_000);
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|timestamp| {
            timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| milliseconds.to_string())
}

pub(crate) fn title_case_token(value: &str) -> String {
    let words = value.replace(['-', '_'], " ");
    let mut chars = words.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

pub(crate) fn event_table_row(value: &serde_json::Value) -> [String; 4] {
    let time = value
        .get("ts")
        .and_then(serde_json::Value::as_u64)
        .map_or_else(|| "—".to_owned(), format_unix_millis);
    let sequence = value
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .map_or_else(|| "—".to_owned(), |value| value.to_string());
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let details = value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "ts" | "seq" | "kind"))
                .take(4)
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned);
                    format!("{key}={value}")
                })
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .filter(|details| !details.is_empty())
        .unwrap_or_else(|| "—".to_owned());
    [time, sequence, kind, details]
}
