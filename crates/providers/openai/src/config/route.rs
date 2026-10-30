use super::*;

impl OpenAiRoute {
    pub(crate) fn bind_platform_generation(
        mut self,
        grant: OpenAiApiKeyGenerationGrant,
    ) -> Result<Self, ConfigError> {
        let bind = |source: Digest| {
            Digest::sha256(
                DigestDomain::ModelBinding,
                2,
                &[
                    b"openai-platform-key-owned-binding",
                    source.bytes(),
                    grant.fingerprint().as_bytes(),
                ],
            )
            .map_err(|_| ConfigError::LocalFixtureContext)
        };
        self.account_binding = bind(self.account_binding)?;
        self.credential_authority_binding = bind(self.credential_authority_binding)?;
        Ok(self)
    }

    /// Returns the immutable route identity.
    #[must_use]
    pub const fn id(&self) -> &RouteId {
        &self.id
    }

    /// Returns the route's explicit protocol dialect.
    #[must_use]
    pub const fn dialect(&self) -> &ResponsesDialect {
        &self.dialect
    }

    /// Returns the exact origin to which credentials are bound.
    #[must_use]
    pub const fn origin(&self) -> &EndpointOrigin {
        &self.origin
    }

    /// Returns the effective storage mode.
    #[must_use]
    pub const fn storage(&self) -> StorageMode {
        self.storage
    }

    /// Returns the fixed transport resource path for this route.
    #[must_use]
    pub const fn responses_path(&self) -> &'static str {
        match &self.auth {
            RouteAuth::Platform { .. } => OPENAI_PLATFORM_RESPONSES_PATH,
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => OPENAI_CODEX_RESPONSES_PATH,
        }
    }

    /// Returns the explicit Platform project-header disposition.
    #[must_use]
    pub const fn platform_project(&self) -> Option<&PlatformProjectPolicy> {
        match &self.auth {
            RouteAuth::Platform { project, .. } => Some(project),
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => None,
        }
    }

    /// Returns the versioned route-specific conformance matrix.
    #[must_use]
    pub const fn conformance(&self) -> OpenAiRouteConformance {
        self.conformance
    }

    /// Reports whether this resolved route can dispatch.
    #[must_use]
    pub const fn routability(&self) -> Routability {
        Routability::Routable
    }

    /// Returns the route's credential/billing access mode.
    #[must_use]
    pub const fn access_mode(&self) -> AccessMode {
        match &self.auth {
            RouteAuth::Platform {
                local_fixture: true,
                ..
            } => AccessMode::LocalFixture,
            RouteAuth::Platform { .. } => AccessMode::PlatformApi,
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => AccessMode::CodexSubscription,
        }
    }

    pub(crate) const fn is_local_platform_fixture(&self) -> bool {
        matches!(
            &self.auth,
            RouteAuth::Platform {
                local_fixture: true,
                ..
            }
        )
    }

    pub(crate) fn platform_generation_grant(
        &self,
    ) -> Result<OpenAiApiKeyGenerationGrant, AuthError> {
        match &self.auth {
            RouteAuth::Platform {
                reference, source, ..
            } => source.generation_grant(reference, &self.origin),
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => Err(AuthError::BindingMismatch),
        }
    }

    pub(crate) fn revalidate_platform_generation(
        &self,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<(), AuthError> {
        (self.platform_generation_grant()? == expected)
            .then_some(())
            .ok_or(AuthError::BindingMismatch)
    }

    pub(crate) fn platform_bearer_header(
        &self,
        expected: OpenAiApiKeyGenerationGrant,
    ) -> Result<plakous_http::HeaderValue, AuthError> {
        let (reference, source, local_fixture) = match &self.auth {
            RouteAuth::Platform {
                reference,
                source,
                local_fixture,
                ..
            } => (reference, source, local_fixture),
            #[cfg(feature = "native-auth")]
            RouteAuth::NativeCodexSubscription { .. } => return Err(AuthError::BindingMismatch),
        };
        if *local_fixture {
            return Err(AuthError::BindingMismatch);
        }
        let key = source.api_key_for_generation(reference, &self.origin, expected)?;
        let mut bytes = b"Bearer ".to_vec();
        bytes.extend_from_slice(key.expose().as_bytes());
        plakous_http::sensitive_header_value(bytes).map_err(|_| AuthError::SourceFailure)
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn is_native_codex(&self) -> bool {
        matches!(&self.auth, RouteAuth::NativeCodexSubscription { .. })
    }

    /// Computes a non-secret account/manager binding for replay isolation.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated digest cannot be created.
    pub fn account_binding(&self) -> Result<Digest, ConfigError> {
        Ok(self.account_binding)
    }

    /// Computes a non-secret binding for the exact credential authority.
    ///
    /// This is deliberately distinct from [`Self::account_binding`]: replacing
    /// an application resolver or OAuth manager cannot preserve an operation
    /// binding merely because it selects the same account label.
    ///
    /// # Errors
    ///
    /// Returns an error only if the domain-separated digest cannot be created.
    pub fn credential_authority_binding(&self) -> Result<Digest, ConfigError> {
        Ok(self.credential_authority_binding)
    }

    #[cfg(feature = "native-auth")]
    pub(crate) async fn native_codex_preflight(
        &self,
        request: NativeCodexPreflightRequest,
    ) -> Result<NativeCodexAuthorization, AuthError> {
        request.target().validate_route(self)?;
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => authority
                .preflight(request)
                .await
                .map_err(|_| AuthError::SourceFailure),
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }

    /// Forces one claimed refresh through the native Codex authority after an
    /// HTTP 401, so the single re-dispatch carries the renewed grant.
    #[cfg(feature = "native-auth")]
    pub(crate) async fn native_codex_refresh_after_rejection(
        &self,
        rejected: &CredentialFence,
        selection_epoch: Digest,
    ) -> Result<(), AuthError> {
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => {
                // The claimed exchange runs as its own task: a dispatch
                // cancelled (Esc) or past its deadline mid-exchange does not
                // abandon a started claim; it completes or fails within its
                // 30 s refresh deadline and records its outcome.
                let authority = Arc::clone(authority);
                let rejected = rejected.clone();
                tokio::spawn(async move {
                    authority
                        .refresh_after_rejection(&rejected, selection_epoch)
                        .await
                })
                .await
                .map_err(|_| AuthError::SourceFailure)?
            }
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn acquire_native_codex_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, AuthError> {
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => {
                authority.acquire_inference(input_token_upper_bound)
            }
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn bind_native_codex_authorization(
        &self,
        authorization: &mut NativeCodexAuthorization,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        authorization.target().validate_route(self)?;
        match &self.auth {
            RouteAuth::NativeCodexSubscription { authority } => {
                authority.bind(authorization, physical_request)
            }
            RouteAuth::Platform { .. } => Err(AuthError::BindingMismatch),
        }
    }
}

impl fmt::Debug for OpenAiRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiRoute")
            .field("id", &self.id)
            .field("dialect", &self.dialect)
            .field("origin", &self.origin)
            .field("storage", &self.storage)
            .field("conformance_revision", &self.conformance.revision())
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("auth", &"<redacted>")
            .finish()
    }
}
