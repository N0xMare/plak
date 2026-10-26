//! Whether a template can run: credentials, tier, and catalog.

use super::policy::AgentTemplate;

/// Credential half of readiness. The catalog projection carries
/// only a `ready` bit; expiry is an auth-slot fact a caller passes directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialReadiness {
    Ready,
    NotLoggedIn,
    Expired,
    NoApiKey,
}

/// Closed, operator-grade readiness disposition for one template. Never embeds
/// a token, a key prefix, or a raw provider error body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateReadiness {
    Ready,
    NotReady(TemplateNotReady),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateNotReady {
    NotInBuild,
    NotLoggedIn,
    CredentialExpired,
    NoApiKey,
    ModelNotAdmitted,
    EffortNotAdmitted,
    TierNotAdvertised,
}

impl TemplateNotReady {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotInBuild => "not in this build",
            Self::NotLoggedIn => "not logged in",
            Self::CredentialExpired => "credential expired",
            Self::NoApiKey => "no API key",
            Self::ModelNotAdmitted => "model not admitted",
            Self::EffortNotAdmitted => "effort not admitted",
            Self::TierNotAdvertised => "tier not advertised",
        }
    }
}

impl TemplateReadiness {
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Ready => None,
            Self::NotReady(reason) => Some(reason.reason()),
        }
    }
}

/// One template projected for the Agents / Models tabs. Typed view only; the
/// TUI computes none of this and never reads `config.toml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateReadinessRow {
    pub name: String,
    pub template: AgentTemplate,
    pub readiness: TemplateReadiness,
}

/// Pure over one credential disposition plus the exact Ready catalog. Never
/// called from a draw path and never cached across a credential change.
#[must_use]
pub fn template_readiness(
    template: &AgentTemplate,
    credential: CredentialReadiness,
    catalog: &[plakous_host_protocol::views::ProviderSection],
) -> TemplateReadiness {
    if (template.composition == Some(super::ProductCompositionSelection::PyPtc)
        && crate::build_features::require_feature("python").is_err())
        || (template.tools.mcp_servers().next().is_some()
            && crate::build_features::require_feature("mcp").is_err())
    {
        return TemplateReadiness::NotReady(TemplateNotReady::NotInBuild);
    }
    match credential {
        CredentialReadiness::NotLoggedIn => {
            return TemplateReadiness::NotReady(TemplateNotReady::NotLoggedIn);
        }
        CredentialReadiness::Expired => {
            return TemplateReadiness::NotReady(TemplateNotReady::CredentialExpired);
        }
        CredentialReadiness::NoApiKey => {
            return TemplateReadiness::NotReady(TemplateNotReady::NoApiKey);
        }
        CredentialReadiness::Ready => {}
    }
    let Some(entry) = catalog
        .iter()
        .find(|section| section.ready && section.account == template.account)
        .and_then(|section| {
            section
                .entries
                .iter()
                .find(|entry| entry.id == template.model)
        })
    else {
        return TemplateReadiness::NotReady(TemplateNotReady::ModelNotAdmitted);
    };
    if entry.unavailable_reason.is_some() {
        return TemplateReadiness::NotReady(TemplateNotReady::ModelNotAdmitted);
    }
    let effort_admitted = match template.effort.as_deref() {
        None => true,
        Some(effort) => entry.effort_options.iter().any(|option| option == effort),
    };
    if !effort_admitted {
        return TemplateReadiness::NotReady(TemplateNotReady::EffortNotAdmitted);
    }
    let tier_admitted = match template.extra.as_deref() {
        None => true,
        Some(extra) => entry.extras.iter().any(|candidate| candidate.id == extra),
    };
    if !tier_admitted {
        return TemplateReadiness::NotReady(TemplateNotReady::TierNotAdvertised);
    }
    TemplateReadiness::Ready
}

/// Catalog-derived credential disposition. `Expired` is an auth-slot fact the
/// catalog projection cannot carry; a caller that knows it passes it directly.
#[must_use]
pub fn catalog_credential_readiness(
    account: &str,
    catalog: &[plakous_host_protocol::views::ProviderSection],
) -> CredentialReadiness {
    let ready = catalog
        .iter()
        .any(|section| section.ready && section.account == account);
    if ready {
        CredentialReadiness::Ready
    } else if account == "openrouter" {
        CredentialReadiness::NoApiKey
    } else {
        CredentialReadiness::NotLoggedIn
    }
}
