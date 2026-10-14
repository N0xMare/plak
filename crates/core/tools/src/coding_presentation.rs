//! Exact coding-presentation catalogs over canonical coding capabilities.
//!
//! Process projections remain the process-only compiled view. This catalog adds
//! the remaining request-visible coding tools. The pi presentation is the one
//! generic vocabulary, for every route without a provider-familiar pack of its
//! own (`OpenRouter` and custom integrations).

use std::sync::Arc;

use plakous_model::{CompiledProviderConfiguration, IntegrationClass};

use crate::capabilities::CODEX_PATCH_ENVELOPE_CANONICAL_NAME;
use crate::capabilities::{
    MERGE_PLAN_CAPABILITY_NAME, PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_POLL_NAME,
    PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_WRITE_NAME,
};
use crate::{ToolRuntimeError, ToolRuntimeErrorKind};

/// The model-visible coding vocabulary: which names, descriptions, and schemas
/// a model is shown.
///
/// This is *not* a route. The route tuple (protocol, route, profile, family,
/// model) selects a presentation through [`CodingPresentation::admit`], and the
/// wire dialect is a third, independent axis. Every `OpenRouter` protocol and
/// every custom integration share [`CodingPresentation::Pi`] while differing on
/// both other axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodingPresentation {
    /// The generic vocabulary, pi 1.1.0's: `read`, `bash`, `edit`, `write`,
    /// plus `wait` and `stop` over its background processes.
    Pi,
    /// Codex vocabulary: `exec_command`, `write_stdin`, Codex-envelope `apply_patch`, `update_plan`.
    Codex,
    /// Grok vocabulary: grok-build wire-aligned Direct tools.
    Grok,
    /// Claude vocabulary: Claude Code's native `Bash`, `Edit`, `Read`, `TaskStop`, `Write`.
    Claude,
}

impl CodingPresentation {
    /// Canonical `request.tools()` names that participate in this catalog.
    ///
    /// These are covered executor names, not exposed costume names. Codex
    /// `apply_patch` covers `patch_envelope`; Grok `todo_write` covers
    /// `merge_plan`. The stable-prefix seal fails closed if a projection
    /// covers a name that is absent here.
    #[must_use]
    pub const fn configured_canonical_names(self) -> &'static [&'static str] {
        match self {
            Self::Pi => &[
                PROCESS_SESSION_CAPABILITY_NAME,
                PROCESS_SESSION_POLL_NAME,
                PROCESS_SESSION_TERMINATE_NAME,
                "apply_patch",
                "read_file",
            ],
            Self::Codex => &[
                PROCESS_SESSION_CAPABILITY_NAME,
                PROCESS_SESSION_WRITE_NAME,
                PROCESS_SESSION_POLL_NAME,
                CODEX_PATCH_ENVELOPE_CANONICAL_NAME,
                "update_plan",
            ],
            Self::Grok => &[
                PROCESS_SESSION_CAPABILITY_NAME,
                PROCESS_SESSION_POLL_NAME,
                PROCESS_SESSION_TERMINATE_NAME,
                "apply_patch",
                "read_file",
                "search_repository",
                MERGE_PLAN_CAPABILITY_NAME,
            ],
            Self::Claude => &[
                PROCESS_SESSION_CAPABILITY_NAME,
                PROCESS_SESSION_POLL_NAME,
                PROCESS_SESSION_TERMINATE_NAME,
                "apply_patch",
                "read_file",
                "search_repository",
                "update_plan",
            ],
        }
    }
}

/// One enrolled route → model-vocabulary rule.
///
/// The rule *shape* is neutral; the route strings are the caller's. This crate
/// spells no provider protocol, route, or profile: the composition root that
/// knows which accounts exist supplies the catalog.
#[derive(Clone, Debug)]
pub struct PresentationRoute {
    integration_class: IntegrationClass,
    protocol: Option<Arc<str>>,
    route: Option<Arc<str>>,
    profile: Option<Arc<str>>,
    family: Option<Arc<str>>,
    /// Literal models this rule admits; `None` admits every model on the route.
    models: Option<fn(&str) -> bool>,
    presentation: CodingPresentation,
}

impl PresentationRoute {
    /// One exact standard route tuple, optionally pinned to a model family,
    /// and the literal models it admits.
    #[must_use]
    pub fn standard(
        protocol: &str,
        route: &str,
        profile: &str,
        family: Option<&str>,
        models: fn(&str) -> bool,
        presentation: CodingPresentation,
    ) -> Self {
        Self {
            integration_class: IntegrationClass::Standard,
            protocol: Some(Arc::from(protocol)),
            route: Some(Arc::from(route)),
            profile: Some(Arc::from(profile)),
            family: family.map(Arc::from),
            models: Some(models),
            presentation,
        }
    }

    /// Every custom integration: a route the composition root attached itself,
    /// with no provider-familiar vocabulary of its own.
    #[must_use]
    pub fn custom_integrations(presentation: CodingPresentation) -> Self {
        Self {
            integration_class: IntegrationClass::Custom,
            protocol: None,
            route: None,
            profile: None,
            family: None,
            models: None,
            presentation,
        }
    }

    fn admits(&self, tuple: &PresentationRouteTuple<'_>) -> bool {
        let axis = |rule: Option<&Arc<str>>, actual: &str| {
            rule.is_none_or(|expected| expected.as_ref() == actual)
        };
        if self.integration_class != tuple.integration_class
            || !axis(self.protocol.as_ref(), tuple.protocol)
            || !axis(self.route.as_ref(), tuple.route)
            || !axis(self.profile.as_ref(), tuple.profile)
        {
            return false;
        }
        // A tuple taken before compilation carries no family and no effective
        // model; the enrolled route already fixes both, so an absent axis is
        // not a mismatch.
        if let (Some(expected), Some(actual)) = (self.family.as_ref(), tuple.family)
            && expected.as_ref() != actual
        {
            return false;
        }
        self.models.is_none_or(|admits| {
            admits(tuple.requested_model)
                && tuple
                    .effective_model
                    .is_none_or(|effective| effective == tuple.requested_model)
        })
    }
}

/// The ordered route→vocabulary catalog, first match wins.
#[derive(Clone, Debug, Default)]
pub struct PresentationRoutes {
    routes: Vec<PresentationRoute>,
}

impl PresentationRoutes {
    /// Builds the catalog in match order.
    #[must_use]
    pub fn new(routes: impl IntoIterator<Item = PresentationRoute>) -> Self {
        Self {
            routes: routes.into_iter().collect(),
        }
    }

    /// Admits one route tuple to its model-visible vocabulary.
    ///
    /// # Errors
    ///
    /// Rejects every tuple without an exact enrolled rule.
    pub fn admit(
        &self,
        tuple: &PresentationRouteTuple<'_>,
    ) -> Result<CodingPresentation, ToolRuntimeError> {
        self.routes
            .iter()
            .find(|route| route.admits(tuple))
            .map(|route| route.presentation)
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "provider configuration has no exact process-presentation candidate",
                )
            })
    }
}

/// The route axes one admission decision reads.
#[derive(Clone, Copy, Debug)]
pub struct PresentationRouteTuple<'a> {
    integration_class: IntegrationClass,
    protocol: &'a str,
    route: &'a str,
    profile: &'a str,
    family: Option<&'a str>,
    requested_model: &'a str,
    effective_model: Option<&'a str>,
}

impl<'a> PresentationRouteTuple<'a> {
    /// The tuple of a route that has been selected but not yet compiled: no
    /// model family and no effective model exist yet.
    #[must_use]
    pub const fn selected(
        integration_class: IntegrationClass,
        protocol: &'a str,
        route: &'a str,
        profile: &'a str,
        requested_model: &'a str,
    ) -> Self {
        Self {
            integration_class,
            protocol,
            route,
            profile,
            family: None,
            requested_model,
            effective_model: None,
        }
    }

    /// The tuple of one compiled provider configuration.
    ///
    /// # Errors
    ///
    /// Rejects a configuration with no exact model family.
    pub fn compiled(
        configuration: &'a CompiledProviderConfiguration,
    ) -> Result<Self, ToolRuntimeError> {
        let effective = configuration.effective();
        let descriptor = effective.descriptor();
        let family = descriptor.model_family().ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "process-presentation candidate requires an exact model family",
            )
        })?;
        Ok(Self {
            integration_class: descriptor.integration_class(),
            protocol: descriptor.protocol().as_str(),
            route: descriptor.route().as_str(),
            profile: descriptor.profile().as_str(),
            family: Some(family.as_str()),
            requested_model: descriptor.requested_model().as_str(),
            effective_model: Some(effective.effective_model().as_str()),
        })
    }
}

impl CodingPresentation {
    /// Admits one compiled provider configuration to its model-visible coding
    /// vocabulary.
    ///
    /// This is the one route→presentation site. Every route reaches it —
    /// native OAuth, custom-integration, and a route selected before it is
    /// compiled — and the rule table is catalog data the caller owns, so this
    /// crate names no provider.
    ///
    /// # Errors
    ///
    /// Rejects a missing model family and every tuple without an exact
    /// enrolled rule.
    pub fn admit(
        configuration: &CompiledProviderConfiguration,
        routes: &PresentationRoutes,
    ) -> Result<Self, ToolRuntimeError> {
        routes.admit(&PresentationRouteTuple::compiled(configuration)?)
    }
}
