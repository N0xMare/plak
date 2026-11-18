//! The vocabulary a hosted session is bound with: which composition the model
//! may use, which `OpenRouter` wire carries it, and the exact target the
//! session was constructed against. Session-binding facts, not configuration:
//! `config.rs` parses `config.toml` into them and the session record persists
//! them, but neither owns their meaning.
//!
//! The operator-facing selections are the host protocol's own
//! [`plakous_host_protocol::CompositionSelection`] and
//! [`plakous_host_protocol::ProtocolSelection`]; the product types here add
//! the CLI/config parse rules and the resolution the wire binding records.

use plakous::subscriptions::NativeProvider;
use serde::{Deserialize, Serialize};

/// Product-owned user selection. `Auto` is a construction-time choice, never
/// a runtime fallback list.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolSelection {
    /// Resolve once under the versioned product rule, not at each request.
    #[default]
    Auto,
    /// Explicit chat-completions wire semantics.
    Chat,
    /// Explicit Responses wire semantics.
    Responses,
    /// Explicit Messages wire semantics.
    Messages,
}

impl ProtocolSelection {
    /// Stable lowercase operator and persisted token, without alias expansion.
    #[allow(dead_code, reason = "consumed by product target-selection wiring")]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }

    /// Parse the closed lowercase token set without trimming or fallback.
    ///
    /// # Errors
    /// Refuses every token other than `auto`, `chat`, `responses` or `messages`.
    #[allow(dead_code, reason = "consumed by product target-selection wiring")]
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "chat" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            "messages" => Ok(Self::Messages),
            _ => Err("protocol must be auto|chat|responses|messages".to_owned()),
        }
    }
}

/// Exact wire family after product resolution.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedProtocol {
    /// Chat-completions request and stream contract.
    Chat,
    /// Responses request and stream contract.
    Responses,
    /// Messages request and stream contract.
    Messages,
}

/// The `[root] agent` template name and the tuple it binds, captured when a
/// session is hosted so the runtime names the template only when the session
/// is actually running it (spec 02 §5.5).
///
/// A binding fact, not configuration: `config.rs` parses `[root] agent` into
/// one and the hosted session compares its bound tuple against it. It lives
/// here so `runtime/**` can hold it without reading the product's TOML schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootTemplateFact {
    /// Template name as `[root] agent` spells it.
    pub name: String,
    /// Account the template binds.
    pub account: String,
    /// Exact model id the template binds.
    pub model: String,
    /// The optional tools the template selects.
    pub tools: crate::tool_plan::TemplateToolSelection,
    /// The canonical `[tools.*]` / `[mcp_servers.*]` tables those tools
    /// reach, keyed `tools.<name>` / `mcp_servers.<id>`.
    pub tool_sections: std::collections::BTreeMap<String, String>,
}

/// Whether a session bound to `account` runs a template naming `template`:
/// the template names a provider kind and the session holds a full account
/// of that kind (spec 02 §4.4.1), so the kinds are compared.
#[must_use]
pub fn account_runs_template(template: &str, account: &str) -> bool {
    match (
        template.parse::<crate::account::AccountRef>(),
        account.parse::<crate::account::AccountRef>(),
    ) {
        (Ok(template), Ok(account)) => template.vertical() == account.vertical(),
        _ => template == account,
    }
}

impl RootTemplateFact {
    /// Whether a session bound to (`account`, `model`) runs this template.
    #[must_use]
    pub fn binds(&self, account: Option<&str>, model: Option<&str>) -> bool {
        account.is_some_and(|account| account_runs_template(&self.account, account))
            && model == Some(self.model.as_str())
    }

    /// The root's tool selection: the `[root] agent` template's list when
    /// the session runs exactly that template's tuple, and none for a root
    /// bound by explicit `--account` / `--model` or with no `[root] agent`.
    #[must_use]
    pub fn root_tool_selection(
        configured: Option<&Self>,
        account: Option<&str>,
        model: Option<&str>,
    ) -> crate::tool_plan::NodeToolSelection {
        configured
            .filter(|fact| fact.binds(account, model))
            .map_or_default(|fact| crate::tool_plan::NodeToolSelection {
                selection: fact.tools.clone(),
                tool_sections: fact.tool_sections.clone(),
                mcp_tools: None,
            })
    }
}

/// Identity of the one product wire-resolution rule used by binding resolution.
pub const WIRE_RESOLUTION_REVISION: &str = "plak-openrouter-wire-resolution-v1";

/// Reconstruction-complete selected and resolved wire binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireBinding {
    /// Original operator choice, retained even when automatic resolution ran.
    pub selection: ProtocolSelection,
    /// One exact wire family used for reconstruction and execution.
    pub resolved: ResolvedProtocol,
    /// Rule identity that must equal this build's supported resolution rule.
    pub resolution_revision: String,
}

impl WireBinding {
    /// Freeze the current resolution rule: `Auto` resolves to chat, and explicit
    /// choices retain their wire. No network discovery or credential lookup runs.
    #[allow(dead_code, reason = "consumed by product target-selection wiring")]
    #[must_use]
    pub fn resolve(selection: ProtocolSelection) -> Self {
        let resolved = match selection {
            ProtocolSelection::Auto | ProtocolSelection::Chat => ResolvedProtocol::Chat,
            ProtocolSelection::Responses => ResolvedProtocol::Responses,
            ProtocolSelection::Messages => ResolvedProtocol::Messages,
        };
        Self {
            selection,
            resolved,
            resolution_revision: WIRE_RESOLUTION_REVISION.to_owned(),
        }
    }

    /// Check that both the resolution rule and its result can be reconstructed.
    ///
    /// # Errors
    /// Refuses an unsupported revision or a result contradicting the selection.
    pub fn validate(&self) -> Result<(), String> {
        if self.resolution_revision != WIRE_RESOLUTION_REVISION {
            return Err("wire binding has an unsupported resolution revision".to_owned());
        }
        if Self::resolve(self.selection).resolved != self.resolved {
            return Err("wire binding contradicts its selected protocol".to_owned());
        }
        Ok(())
    }
}

/// Product-owned `OpenRouter` endpoint/fallback policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OpenRouterRoutingSelection {
    /// Let the gateway choose its backend; the host adds no endpoint fallback list.
    GatewayManaged,
}

/// Separately billed, exact-origin API account selected by the product.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformProvider {
    /// `OpenAI` API-key account, independent of a `ChatGPT` subscription.
    OpenAi,
    /// Anthropic API-key account, independent of a Claude subscription.
    Anthropic,
    /// xAI API-key account, independent of a Grok subscription.
    Xai,
}

impl PlatformProvider {
    /// Canonical default-account prefix for this exact API route.
    #[must_use]
    pub const fn account(self) -> &'static str {
        crate::capabilities::for_platform(self).account
    }

    /// Refusal for an explicit `native-ptc` on an API model that enrolls no
    /// native programmatic tool calling.
    #[must_use]
    pub const fn native_ptc_refusal(self) -> &'static str {
        crate::capabilities::for_platform(self).native_ptc_refusal
    }

    /// Parse a default or named direct-API account; gateway, subscription and
    /// invalid account identities return `None` rather than being coerced.
    #[must_use]
    pub fn for_account(account: &str) -> Option<Self> {
        crate::capabilities::for_account(account)
            .and_then(crate::capabilities::RouteCapability::platform_provider)
    }

    /// The route's fixed wire family, independent of optional runtime features.
    #[must_use]
    pub const fn protocol(self) -> ResolvedProtocol {
        match crate::capabilities::for_platform(self).protocol {
            Some(protocol) => protocol,
            None => unreachable!(),
        }
    }
}

/// Resolved API route used by a hosted root or child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiTarget {
    /// Gateway selection with an already resolved wire and exact account.
    OpenRouter {
        /// Wire frozen at binding construction, not selected per request.
        wire: ResolvedProtocol,
        /// Exact gateway credential scope, including a named suffix when present.
        account: crate::account::AccountRef,
    },
    /// Direct API selection, with no gateway or subscription fallback.
    Platform {
        /// Provider whose catalog and authentication contract govern the model.
        provider: PlatformProvider,
        /// Exact credential scope that must agree with the provider.
        account: crate::account::AccountRef,
    },
}

impl ApiTarget {
    /// Canonical account reference, preserving any named-account suffix.
    #[must_use]
    pub fn account(&self) -> &str {
        self.selection().as_str()
    }

    /// Parsed exact account identity, without credential resolution.
    #[must_use]
    pub fn selection(&self) -> &crate::account::AccountRef {
        match self {
            Self::OpenRouter { account, .. } | Self::Platform { account, .. } => account,
        }
    }

    /// Exact request/stream wire; direct routes use their fixed protocol.
    #[must_use]
    pub const fn protocol(&self) -> ResolvedProtocol {
        match self {
            Self::OpenRouter { wire, .. } => *wire,
            Self::Platform { provider, .. } => provider.protocol(),
        }
    }
}

/// Exact product target persisted on every session record.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetSelection {
    /// Native subscription target using its ordinary service tier.
    #[default]
    Native,
    /// Native Codex subscription target using the exact `priority` service tier.
    NativePriority,
    /// Gateway target whose selected wire and policy are reconstruction facts.
    OpenRouter {
        /// Original and resolved protocol under a pinned rule revision.
        wire: WireBinding,
        /// Gateway backend-routing policy, not a host fallback list.
        routing: OpenRouterRoutingSelection,
    },
    /// Direct API target, separately billed and authenticated from subscriptions.
    Platform {
        /// Provider identity, retained even when its runtime feature is omitted.
        provider: PlatformProvider,
    },
}

impl TargetSelection {
    /// Whether construction uses API-key rather than subscription authority.
    #[must_use]
    pub const fn is_api(&self) -> bool {
        matches!(self, Self::Platform { .. } | Self::OpenRouter { .. })
    }

    /// Resolve an API target only after validating the exact account's route.
    /// Native targets return `None`; the account is never rewritten.
    ///
    /// # Errors
    /// Returns an account-target mismatch when the selected route disagrees.
    pub fn api_target(
        &self,
        account: &crate::account::AccountRef,
    ) -> Result<Option<ApiTarget>, crate::capabilities::AccountAdmissionError> {
        self.validate_account(account)?;
        Ok(match self {
            Self::OpenRouter { wire, .. } => Some(ApiTarget::OpenRouter {
                wire: wire.resolved,
                account: account.clone(),
            }),
            Self::Platform { provider } => Some(ApiTarget::Platform {
                provider: *provider,
                account: account.clone(),
            }),
            Self::Native | Self::NativePriority => None,
        })
    }

    /// Check route agreement without resolving credentials or build admission.
    /// Priority native service is valid only for the Codex subscription route.
    ///
    /// # Errors
    /// Returns an account-target mismatch; no alternate account is tried.
    pub fn validate_account(
        &self,
        account: &crate::account::AccountRef,
    ) -> Result<(), crate::capabilities::AccountAdmissionError> {
        use crate::account::Vertical;
        let matches = match self {
            Self::Platform { provider } => {
                account.vertical().capability().platform_provider() == Some(*provider)
            }
            Self::OpenRouter { .. } => account.vertical() == Vertical::OpenRouter,
            Self::Native => !account.vertical().is_api(),
            Self::NativePriority => account.vertical() == Vertical::Codex,
        };
        if matches {
            Ok(())
        } else {
            Err(
                crate::capabilities::AccountAdmissionError::AccountTargetMismatch {
                    account: account.to_string(),
                },
            )
        }
    }

    /// Validate reconstruction facts without changing the target or probing auth.
    ///
    /// # Errors
    /// Refuses an unsupported or contradictory gateway wire binding.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Native | Self::NativePriority | Self::Platform { .. } => Ok(()),
            Self::OpenRouter { wire, routing: _ } => {
                wire.validate()?;
                Ok(())
            }
        }
    }
}

/// Which authority resolves a bound session's model.
///
/// Every attach — the client's deferred preparation, `plakd agent`, the attached
/// one-shot verbs, `plakd run --session` — used to re-derive this from a
/// `matches!(target, OpenRouter { .. })` plus an `account == "openrouter"`
/// string test plus an [`oauth_provider_for_account`] lookup, and the three
/// spellings could disagree. [`TargetSelection::route`] is the one decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachRoute<'a> {
    /// An API key with a distinct billing and credential authority from OAuth.
    Platform {
        /// Exact direct API provider; subscription identity is not interchangeable.
        provider: PlatformProvider,
    },
    /// The `OpenRouter` gateway on the `openrouter` account.
    OpenRouter {
        /// Saved wire choice whose revision is validated before construction.
        wire: &'a WireBinding,
        /// Saved gateway routing policy, not a local endpoint fallback list.
        routing: &'a OpenRouterRoutingSelection,
    },
    /// A native OAuth subscription, optionally on the priority service tier.
    NativeOauth {
        /// Native subscription identity selected by the exact account.
        provider: NativeProvider,
        /// Whether this target explicitly requests the priority service tier.
        priority: bool,
    },
}

/// Why an account and a target do not name a route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteRefusal {
    /// An `OpenRouter` target on some other account, or the reverse.
    AccountMismatch,
    /// A native target on an account that holds no OAuth subscription.
    NotOauth,
}

impl RouteRefusal {
    /// The message the attach paths have always printed for this refusal.
    #[must_use]
    pub fn message(&self, account: &str) -> String {
        match self {
            Self::AccountMismatch => "target/account binding is inconsistent".to_owned(),
            Self::NotOauth => {
                let accounts = crate::capabilities::ROUTES
                    .iter()
                    .filter(|route| route.native_provider().is_some())
                    .map(|route| route.account)
                    .collect::<Vec<_>>()
                    .join("|");
                format!(
                    "account `{account}` requires an exact API target or a supported OAuth account ({accounts})"
                )
            }
        }
    }
}

/// The notice account for a compiled native provider.
///
/// String aliases stay on [`plakous_host_protocol::notice_remedy::AccountTarget::from_account`]
/// for persisted and foreign identifiers. A value that is already a
/// [`NativeProvider`] does not go through that table.
#[must_use]
pub const fn account_target(
    provider: NativeProvider,
) -> plakous_host_protocol::notice_remedy::AccountTarget {
    use plakous_host_protocol::notice_remedy::AccountTarget;
    match provider {
        NativeProvider::Codex => AccountTarget::Codex,
        NativeProvider::Xai => AccountTarget::Grok,
        NativeProvider::Claude => AccountTarget::Claude,
        _ => AccountTarget::Unspecified,
    }
}

/// Map product account id to native OAuth provider (if any).
///
/// The account vocabulary is wider than the provider set: `supergrok` and
/// `xai-oauth` are the same subscription as `grok`, and `openrouter` (or an
/// API-key account) has none.
#[must_use]
pub fn oauth_provider_for_account(account: &str) -> Option<NativeProvider> {
    match crate::capabilities::for_account(account) {
        Some(route) => route.native_provider(),
        None => None,
    }
}

/// Admits `model` on `account` only when the account's catalog lists it: the
/// provider's enrollment table on an OAuth subscription, the usable API
/// catalog on a platform account. `OpenRouter` ids are dynamic gateway
/// selections with no product catalog, so its own discovery decides. An
/// account with neither is refused rather than guessed at.
///
/// # Errors
/// The refusal names the account and the catalog the model is absent from,
/// with the command that lists the catalog.
pub fn admit_account_model(account: &str, model: &str) -> crate::CliResult<()> {
    let (catalog, listing) = if let Some(provider) = oauth_provider_for_account(account) {
        if provider.enrolls(model) {
            return Ok(());
        }
        let route = crate::capabilities::for_account(account)
            .ok_or_else(|| RouteRefusal::NotOauth.message(account))?;
        (route.catalog_label, account)
    } else if let Some(provider) = PlatformProvider::for_account(account) {
        if crate::platform::platform_model(provider, model).is_some() {
            return Ok(());
        }
        let route = crate::capabilities::for_platform(provider);
        (route.catalog_label, account)
    } else if account
        .parse::<crate::account::AccountRef>()
        .is_ok_and(|account| account.vertical() == crate::account::Vertical::OpenRouter)
    {
        return Ok(());
    } else {
        return Err(RouteRefusal::NotOauth.message(account).into());
    };
    Err(crate::CliFailure::new(format!(
        "model `{model}` is not in the {catalog} catalog of account `{account}`; inspect the account's available models"
    )).with_advice(plakous_host_protocol::refusal::RefusalAdvice::UnknownModel { model: model.to_owned(), account: listing.to_owned(), catalog: catalog.to_owned() }))
}

/// Refusal for an explicit `native-ptc` on a subscription row that enrolls no
/// native programmatic tool calling. Only Codex Code Mode is enrolled, and only
/// on `CodeModeOnly` rows.
#[must_use]
pub const fn native_ptc_refusal(provider: NativeProvider) -> &'static str {
    match crate::capabilities::for_native(provider) {
        Some(route) => route.native_ptc_refusal,
        None => unreachable!(),
    }
}

/// Refusal for an explicit `native-ptc` on `OpenRouter`, which enrolls no
/// native programmatic tool calling.
pub const OPENROUTER_NATIVE_PTC_REFUSAL: &str = crate::capabilities::OPENROUTER.native_ptc_refusal;

impl TargetSelection {
    /// The route this target takes on `account`, or why it takes none.
    ///
    /// # Errors
    /// [`RouteRefusal::AccountMismatch`] when the target and the account
    /// disagree about the gateway; [`RouteRefusal::NotOauth`] when a native
    /// target names an account with no OAuth subscription.
    pub fn route<'a>(&'a self, account: &str) -> Result<AttachRoute<'a>, RouteRefusal> {
        let selected: crate::account::AccountRef = account.parse().map_err(|_| {
            if self.is_api() {
                RouteRefusal::AccountMismatch
            } else {
                RouteRefusal::NotOauth
            }
        })?;
        self.validate_account(&selected).map_err(|_| {
            if self.is_api() {
                RouteRefusal::AccountMismatch
            } else {
                RouteRefusal::NotOauth
            }
        })?;
        match self {
            Self::Platform { provider } => {
                if selected.vertical().capability().platform_provider() == Some(*provider) {
                    Ok(AttachRoute::Platform {
                        provider: *provider,
                    })
                } else {
                    Err(RouteRefusal::AccountMismatch)
                }
            }
            Self::OpenRouter { wire, routing } => {
                if selected.vertical() == crate::account::Vertical::OpenRouter {
                    Ok(AttachRoute::OpenRouter { wire, routing })
                } else {
                    Err(RouteRefusal::AccountMismatch)
                }
            }
            // A native target on any account without an OAuth subscription —
            // `openrouter` included — is `NotOauth`, the refusal the attach
            // paths have always reported for it.
            Self::Native | Self::NativePriority => selected
                .vertical()
                .capability()
                .native_provider()
                .map_or(Err(RouteRefusal::NotOauth), |provider| {
                    Ok(AttachRoute::NativeOauth {
                        provider,
                        priority: matches!(self, Self::NativePriority),
                    })
                }),
        }
    }

    /// Whether this target routes through the `OpenRouter` gateway.
    #[must_use]
    pub const fn is_openrouter(&self) -> bool {
        matches!(self, Self::OpenRouter { .. })
    }
}

/// Plak-owned choice of how the model may compose canonical product tools.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompositionSelection {
    /// Present canonical tools directly, without an evaluator wrapper.
    Direct,
    /// The versioned Plakous-recommended tool presentation for the bound
    /// provider/model, resolved once at attach and pinned for the session.
    #[default]
    #[serde(rename = "plakous-default")]
    FollowCatalog,
    /// The vertical's own programmatic tool calling system. Only Codex Code
    /// Mode (`exec`/`wait`) is enrolled: Codex `CodeModeOnly` rows and `OpenAI`
    /// API models enrolled as `CodeModeOnly`.
    NativePtc,
    /// Harness JavaScript PTC (`exec_js`/`wait_js`), on every vertical.
    JsPtc,
    /// Harness Python PTC (`exec_py`/`wait_py`), on every vertical.
    PyPtc,
    /// Root-only typed Python SDK (`run_py`).
    PySdk,
}

impl CompositionSelection {
    /// Whether this selection needs the Python evaluator worker.
    #[must_use]
    pub const fn requires_python_worker(self) -> bool {
        matches!(self, Self::PyPtc | Self::PySdk)
    }

    pub(crate) fn admit_role(self, spawned_child: bool) -> crate::CliResult<()> {
        if self == Self::PySdk && spawned_child {
            return Err(crate::CliFailure::new(
                "py-sdk is root-only and has no child result channel",
            ));
        }
        Ok(())
    }

    /// The program runtime this selection runs under; `None` is Direct.
    pub(crate) const fn runtime(self) -> Option<plakous::tools::CompositionRuntime> {
        use plakous::tools::CompositionRuntime;
        match self {
            Self::Direct | Self::FollowCatalog => None,
            Self::NativePtc => Some(CompositionRuntime::CodexNative),
            Self::JsPtc => Some(CompositionRuntime::ProductJavaScript),
            Self::PyPtc => Some(CompositionRuntime::PythonPtc),
            Self::PySdk => Some(CompositionRuntime::PythonHarnessSdk),
        }
    }

    pub(crate) const fn outer_marker_names(self) -> Option<&'static [&'static str]> {
        match self.runtime() {
            Some(runtime) => Some(runtime.marker_names()),
            None => None,
        }
    }

    /// Stable explicit operator/config token, not the resolved runtime profile.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::FollowCatalog => "plakous-default",
            Self::NativePtc => "native-ptc",
            Self::JsPtc => "js-ptc",
            Self::PyPtc => "py-ptc",
            Self::PySdk => "py-sdk",
        }
    }

    /// Closed CLI and explicit operator tokens.
    ///
    /// # Errors
    /// Refuses unknown tokens rather than interpreting aliases or choosing a default.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "direct" => Ok(Self::Direct),
            "plakous-default" => Ok(Self::FollowCatalog),
            "native-ptc" => Ok(Self::NativePtc),
            "js-ptc" => Ok(Self::JsPtc),
            "py-ptc" => Ok(Self::PyPtc),
            "py-sdk" => Ok(Self::PySdk),
            _ => Err(
                "composition must be direct|plakous-default|native-ptc|js-ptc|py-ptc|py-sdk"
                    .to_owned(),
            ),
        }
    }

    /// Config and session persistence spell the same closed token set.
    ///
    /// # Errors
    /// Returns the same unknown-token refusal as [`Self::parse`].
    pub fn parse_config(value: &str) -> Result<Self, String> {
        Self::parse(value)
    }
}

impl From<CompositionSelection> for plakous_host_protocol::CompositionSelection {
    fn from(value: CompositionSelection) -> Self {
        match value {
            CompositionSelection::Direct => Self::Direct,
            CompositionSelection::FollowCatalog => Self::FollowCatalog,
            CompositionSelection::NativePtc => Self::NativePtc,
            CompositionSelection::JsPtc => Self::JsPtc,
            CompositionSelection::PyPtc => Self::PyPtc,
            CompositionSelection::PySdk => Self::PySdk,
        }
    }
}

impl From<plakous_host_protocol::CompositionSelection> for CompositionSelection {
    fn from(value: plakous_host_protocol::CompositionSelection) -> Self {
        match value {
            plakous_host_protocol::CompositionSelection::Direct => Self::Direct,
            plakous_host_protocol::CompositionSelection::FollowCatalog => Self::FollowCatalog,
            plakous_host_protocol::CompositionSelection::NativePtc => Self::NativePtc,
            plakous_host_protocol::CompositionSelection::JsPtc => Self::JsPtc,
            plakous_host_protocol::CompositionSelection::PyPtc => Self::PyPtc,
            plakous_host_protocol::CompositionSelection::PySdk => Self::PySdk,
        }
    }
}

impl From<plakous_host_protocol::ProtocolSelection> for ProtocolSelection {
    fn from(value: plakous_host_protocol::ProtocolSelection) -> Self {
        match value {
            plakous_host_protocol::ProtocolSelection::Auto => Self::Auto,
            plakous_host_protocol::ProtocolSelection::Chat => Self::Chat,
            plakous_host_protocol::ProtocolSelection::Responses => Self::Responses,
            plakous_host_protocol::ProtocolSelection::Messages => Self::Messages,
        }
    }
}

impl From<ProtocolSelection> for plakous_host_protocol::ProtocolSelection {
    fn from(value: ProtocolSelection) -> Self {
        match value {
            ProtocolSelection::Auto => Self::Auto,
            ProtocolSelection::Chat => Self::Chat,
            ProtocolSelection::Responses => Self::Responses,
            ProtocolSelection::Messages => Self::Messages,
        }
    }
}

#[cfg(test)]
mod tests;
