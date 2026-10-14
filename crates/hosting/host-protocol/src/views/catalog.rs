//! Ready-catalog rows: which accounts are ready and which exact models each
//! one offers, plus the credential readiness the Providers view paints.

/// One catalog row from `plak models <account>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelEntry {
    /// Exact model id.
    pub id: String,
    /// Capability tags for the right column.
    pub tag: String,
    /// Whether the model accepts reasoning effort.
    pub reasoning: bool,
    /// Facade-admitted effort labels, in provider-authored presentation order.
    pub effort_options: Vec<String>,
    /// Product-interpreted optional controls for this exact model.
    pub extras: Vec<ExtraState>,
    /// Product qualification refusal for a catalog-present row.
    pub unavailable_reason: Option<String>,
}

/// One provider section in the picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSection {
    /// Product account id (`codex` | `grok` | …).
    pub account: String,
    /// Display label.
    pub label: String,
    /// Credential is ready / key present.
    pub ready: bool,
    /// Models for this account.
    pub entries: Vec<ModelEntry>,
}

/// Extra option state (for example Codex `priority` / Fast).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtraState {
    /// Exact option id (`priority`).
    pub id: String,
    /// Display label.
    pub label: String,
    /// On/off.
    pub enabled: bool,
}

/// The exact Ready catalog the host discovered: one section per account, in
/// product order. One shape for the picker, the host configuration, and the
/// JSONL face.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadyCatalog {
    /// Account sections in product order.
    pub accounts: Vec<ProviderSection>,
}

impl ReadyCatalog {
    /// Wrap already-projected account sections.
    #[must_use]
    pub const fn new(accounts: Vec<ProviderSection>) -> Self {
        Self { accounts }
    }
}

impl From<Vec<ProviderSection>> for ReadyCatalog {
    fn from(accounts: Vec<ProviderSection>) -> Self {
        Self::new(accounts)
    }
}

/// How a provider authenticates at the product layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthKind {
    /// Subscription OAuth (`plak login`).
    Oauth,
    /// API key file / env (`plak key set`).
    ApiKey,
}

impl AuthKind {
    /// Short right-column tag for the row.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Oauth => "subscription",
            Self::ApiKey => "API key",
        }
    }
}

/// Readiness of one vertical's credential (spec 02 §5.5).
///
/// A vertical that is not [`ProviderState::Ready`] is never painted as
/// connected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderState {
    /// Credential is present and revalidated.
    Ready,
    /// A credential exists but cannot be used until it is refreshed or
    /// re-authorized (refresh required, revoked, or a compatibility receipt
    /// that must be accepted again).
    Expired,
    /// Credential exists but refresh outcome is unknown (quarantined after an
    /// ambiguous claimed refresh).
    RefreshUnknown,
    /// OAuth vertical with no stored credential.
    NotLoggedIn,
    /// API-key vertical with no key in the environment or the file store.
    NoKey,
    /// The credential store could not be read; readiness is unknown.
    Unavailable,
}

impl ProviderState {
    /// Operator-facing state label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Expired => "expired",
            Self::RefreshUnknown => "refresh outcome unknown",
            Self::NotLoggedIn => "not logged in",
            Self::NoKey => "no key",
            Self::Unavailable => "unavailable",
        }
    }

    /// Whether the vertical may be painted as connected.
    #[must_use]
    pub const fn ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Leading readiness chip.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        if self.ready() { "●" } else { "○" }
    }
}

/// One vertical's readiness, as projected by the product host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDef {
    /// Product account id (`codex`, `openrouter`, …).
    pub id: String,
    /// Row label.
    pub label: String,
    /// Auth route for this vertical.
    pub kind: AuthKind,
    /// Host-computed readiness.
    pub state: ProviderState,
    /// Redaction-safe nuance behind the state (`environment`, `catalog
    /// unavailable`, …). Empty when the state says everything.
    pub detail: String,
    /// Account label when the stored credential supplies one. OAuth
    /// credentials expose no offline identity, so this is the API-key
    /// fingerprint or the winning environment variable, never key material.
    pub account_label: Option<String>,
}
