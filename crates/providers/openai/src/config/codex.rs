use super::*;

/// Serializable, secret-free direct Codex subscription route configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSubscriptionRouteConfig {
    origin: EndpointOrigin,
}

impl CodexSubscriptionRouteConfig {
    /// Creates a disabled-by-default direct subscription route configuration.
    #[must_use]
    pub const fn new(origin: EndpointOrigin) -> Self {
        Self { origin }
    }

    /// Reports that configuration alone is not routable.
    #[must_use]
    pub const fn routability(&self) -> Routability {
        Routability::NonRoutable
    }

    /// Attempts to resolve without an injected OAuth source and fails closed.
    ///
    /// # Errors
    ///
    /// Always returns [`ConfigError::SubscriptionOAuthRequired`].
    pub fn resolve_disabled(&self) -> Result<OpenAiRoute, ConfigError> {
        Err(ConfigError::SubscriptionOAuthRequired)
    }

    /// Resolves an official-origin route with an account-bound Codex authority.
    ///
    /// # Errors
    ///
    /// Returns an error unless the configured origin is exactly the official
    /// `ChatGPT` origin. No credential source is called during resolution.
    /// Resolves an official-origin route directly from one explicit Codex store.
    ///
    /// The public facade exposes this constructor instead of the lower-level
    /// credential source, keeping raw current-access generations out of the
    /// facade API.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe store/source error or the same origin error as
    /// [`Self::resolve`].
    /// Resolves the immutable native compatibility route with a Plak-owned manager.
    ///
    /// # Errors
    ///
    /// Rejects every origin except the pinned `ChatGPT` origin and rejects a
    /// manager whose exact compatibility notice is no longer accepted/current.
    #[cfg(feature = "native-auth")]
    pub(crate) fn resolve_native(
        self,
        manager: Arc<CodexSubscriptionCredentialManager>,
    ) -> Result<OpenAiRoute, ConfigError> {
        if self.origin.as_str() != OPENAI_CHATGPT_ORIGIN {
            return Err(ConfigError::NonOfficialCodexSubscriptionOrigin);
        }
        manager
            .validate_route_resolution()
            .map_err(|_| ConfigError::SubscriptionCredentialUnavailable)?;
        let authority: Arc<dyn NativeCodexCredentialAuthority> = manager;
        Ok(OpenAiRoute::new(
            OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID,
            ResponsesDialect::codex_subscription(),
            self.origin,
            StorageMode::Stateless,
            OpenAiRouteConformance::codex_compatibility(),
            RouteAuth::NativeCodexSubscription { authority },
        ))
    }
}
