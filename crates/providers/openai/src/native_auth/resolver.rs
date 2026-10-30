//! The provider-local catalog and model resolver for the native Codex route.

use super::{
    Arc, CODEX_NATIVE_COMPACTION_NAMESPACE, Capability, Client, Clock, CodexCatalogSource,
    CodexModelCatalog, CodexModelResolutionEvidence, CodexReceiptRenewal, CodexRowSemantics,
    CodexSubscriptionCredentialManager, CodexSubscriptionRouteConfig, CredentialFence,
    EndpointOrigin, FactSource, LanguageModel, Limits, Mutex, NamespaceId, NativeCodexAuthError,
    OPENAI_CHATGPT_ORIGIN, OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
    OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiLanguageModel, OpenAiRoute, OpenAiTarget,
    ResolvedCatalog, StandardOpenAiHttpConfig, Support, fmt, resolution_route_fingerprint,
    validate_catalog_fence,
};

/// Provider-local catalog and model resolver for the native Codex route.
pub struct CodexModelResolver {
    pub(super) catalog_retries: bool,
    pub(super) manager: Arc<CodexSubscriptionCredentialManager>,
    pub(super) route: OpenAiRoute,
    pub(super) client: Client,
    inference_observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    pub(super) config: StandardOpenAiHttpConfig,
    pub(super) catalog: Mutex<Option<ResolvedCatalog>>,
    pub(super) clock: Arc<dyn Clock>,
}

impl CodexModelResolver {
    fn validate_route_account(&self) -> Result<(), NativeCodexAuthError> {
        let route_account = self
            .route
            .account_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        if route_account != self.manager.route_account_digest() {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        Ok(())
    }

    pub(super) fn validate_catalog_fence(
        &self,
        expected: &CredentialFence,
    ) -> Result<(), NativeCodexAuthError> {
        validate_catalog_fence(&self.manager, expected)
    }

    /// The exact catalog inputs this resolver's models renew their receipts with.
    pub(super) fn catalog_source(&self) -> CodexCatalogSource {
        CodexCatalogSource {
            catalog_retries: self.catalog_retries,
            manager: Arc::clone(&self.manager),
            route: self.route.clone(),
            client: self.client.clone(),
            clock: Arc::clone(&self.clock),
        }
    }

    pub(super) async fn fetch_catalog(&self) -> Result<ResolvedCatalog, NativeCodexAuthError> {
        self.catalog_source().fetch_catalog(None).await
    }

    /// Binds a ready native manager to the exact `ChatGPT` route without network I/O.
    ///
    /// # Errors
    ///
    /// Rejects a missing selected account, stale profile acceptance, or an
    /// invalid exact native route. Credential readiness is checked before each
    /// catalog request and inference preflight.
    pub fn new(
        manager: Arc<CodexSubscriptionCredentialManager>,
        client: Client,
        config: StandardOpenAiHttpConfig,
    ) -> Result<Self, NativeCodexAuthError> {
        let origin = EndpointOrigin::parse(OPENAI_CHATGPT_ORIGIN)
            .map_err(|_| NativeCodexAuthError::Validation)?;
        let route = CodexSubscriptionRouteConfig::new(origin)
            .resolve_native(Arc::clone(&manager))
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        let clock = Arc::clone(&manager.clock);
        Ok(Self {
            catalog_retries: true,
            manager,
            route,
            client,
            inference_observer: None,
            config,
            catalog: Mutex::new(None),
            clock,
        })
    }

    /// Limits catalog resolution to one physical GET while a session is attached.
    #[must_use]
    pub fn without_catalog_retries(mut self) -> Self {
        self.catalog_retries = false;
        self
    }

    /// Observes only inference exchanges, never catalog or credential traffic.
    /// `None` leaves the caller-supplied client configuration unchanged.
    /// Transport identity and dispatch policy remain unchanged.
    #[must_use]
    pub fn with_inference_observer(
        mut self,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Self {
        self.inference_observer = observer;
        self
    }

    /// Replaces the clock used for catalog provenance and resolved-target expiry.
    #[must_use]
    #[cfg(any(test, feature = "test-clock"))]
    #[doc(hidden)]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    #[cfg(test)]
    pub(super) fn new_for_test(
        manager: Arc<CodexSubscriptionCredentialManager>,
        client: Client,
        config: StandardOpenAiHttpConfig,
    ) -> Result<Self, NativeCodexAuthError> {
        let route = crate::config::native_codex_transport_conformance_route(
            manager.endpoints.chatgpt_origin.clone(),
            Arc::clone(&manager),
        )
        .map_err(|_| NativeCodexAuthError::Unavailable)?;
        let clock = Arc::clone(&manager.clock);
        Ok(Self {
            catalog_retries: true,
            manager,
            route,
            client,
            inference_observer: None,
            config,
            catalog: Mutex::new(None),
            clock,
        })
    }

    /// Refreshes credentials before one authenticated catalog request and
    /// atomically replaces the credential-free resolver snapshot.
    ///
    /// # Errors
    ///
    /// Rejects unavailable credentials, stale profile acceptance, non-JSON or
    /// malformed catalog data, and exact store/account races.
    pub async fn refresh_catalog(&self) -> Result<CodexModelCatalog, NativeCodexAuthError> {
        if let Err(error) = self.validate_route_account() {
            *self
                .catalog
                .lock()
                .map_err(|_| NativeCodexAuthError::Coordination)? = None;
            return Err(error);
        }
        let resolved = self.fetch_catalog().await?;
        self.manager.validate_profile_admission()?;
        if let Err(error) = self.validate_route_account() {
            *self
                .catalog
                .lock()
                .map_err(|_| NativeCodexAuthError::Coordination)? = None;
            return Err(error);
        }
        self.validate_catalog_fence(&resolved.credential_fence)?;
        let public = resolved.public.clone();
        *self
            .catalog
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)? = Some(resolved);
        Ok(public)
    }

    /// Resolves a previously discovered exact slug without network access.
    ///
    /// # Errors
    ///
    /// Rejects missing, foreign, or expired evidence.
    pub fn resolve(&self, model_id: &str) -> Result<Arc<dyn LanguageModel>, NativeCodexAuthError> {
        self.resolve_with_evidence(model_id).map(|(model, _)| model)
    }

    /// Atomically resolves a model and its credential-free exact-resolution evidence.
    ///
    /// The evidence and returned model are built from the same catalog
    /// descriptor, route, authenticated credential fence, and adapter revision.
    /// No network request is made.
    ///
    /// # Errors
    ///
    /// Rejects missing or expired catalog evidence, a changed account or
    /// credential generation, and any foreign route, descriptor, or witness.
    pub fn resolve_with_evidence(
        &self,
        model_id: &str,
    ) -> Result<(Arc<dyn LanguageModel>, CodexModelResolutionEvidence), NativeCodexAuthError> {
        let (model, evidence) = self.resolve_exact(model_id)?;
        let model: Arc<dyn LanguageModel> = model;
        Ok((model, evidence))
    }

    /// Resolves the model as [`Self::resolve_with_evidence`] does, with the
    /// hosted web search side-request executor and the image generation
    /// executor on the same route, login, model and client.
    ///
    /// # Errors
    ///
    /// Same failures as [`Self::resolve_with_evidence`].
    pub fn resolve_with_retrieval(
        &self,
        model_id: &str,
    ) -> Result<
        (
            Arc<dyn LanguageModel>,
            CodexModelResolutionEvidence,
            crate::CodexRetrieval,
            crate::CodexImages,
        ),
        NativeCodexAuthError,
    > {
        let (model, evidence) = self.resolve_exact(model_id)?;
        let retrieval = model.codex_retrieval();
        let images = model.codex_images();
        let model: Arc<dyn LanguageModel> = model;
        Ok((model, evidence, retrieval, images))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Resolve the exact Codex catalog row and bind its capability, context and credential evidence to one target."
    )]
    fn resolve_exact(
        &self,
        model_id: &str,
    ) -> Result<(Arc<OpenAiLanguageModel>, CodexModelResolutionEvidence), NativeCodexAuthError>
    {
        self.validate_route_account()?;
        let catalog = self
            .catalog
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?;
        let catalog = catalog.as_ref().ok_or(NativeCodexAuthError::Catalog)?;
        self.validate_catalog_fence(&catalog.credential_fence)?;
        if catalog.ambiguous_slugs.contains(model_id) {
            return Err(NativeCodexAuthError::Catalog);
        }
        let issued = catalog
            .descriptors
            .get(model_id)
            .ok_or(NativeCodexAuthError::Catalog)?
            .clone();
        let renewal = CodexReceiptRenewal {
            source: self.catalog_source(),
            model_id: model_id.to_owned(),
            semantics: CodexRowSemantics::of(&issued)?,
        };
        let credential_selection_epoch = issued.witness.credential_selection_epoch();
        let descriptor = issued.descriptor;
        let capacity_facts = issued.capacity_facts;
        let reasoning_efforts = issued.reasoning_efforts;
        let tool_mode = issued.tool_mode;
        let responses_lite = issued.responses_lite;
        let catalog_enum_fingerprint = issued.catalog_enum_fingerprint;
        let now = self.clock.now();
        let compaction_namespace = NamespaceId::new(CODEX_NATIVE_COMPACTION_NAMESPACE)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        if descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || descriptor.requested_model().as_str() != model_id
            || issued.observed_at > now
            || issued.expires_at <= now
            || descriptor
                .capabilities()
                .features()
                .iter()
                .any(|(capability, fact)| {
                    if *capability == Capability::ParallelTools {
                        fact.provenance().source() != FactSource::BuiltIn
                            || fact.provenance().expires_at().is_some()
                    } else {
                        fact.provenance().source() != FactSource::RouteEvidence
                            || fact.provenance().is_expired(now)
                    }
                })
            || !descriptor
                .capabilities()
                .native_namespace(&compaction_namespace)
                .is_some_and(|fact| {
                    fact.support() == Support::Native
                        && fact.provenance().source() == FactSource::RouteEvidence
                        && !fact.provenance().is_expired(now)
                })
        {
            return Err(NativeCodexAuthError::Catalog);
        }
        let account_fingerprint = self
            .route
            .account_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let credential_authority_binding = self
            .route
            .credential_authority_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let evidence = CodexModelResolutionEvidence {
            evidence_revision: 5,
            account_fingerprint,
            credential_authority_binding,
            selected_credential_generation: catalog.credential_fence.generation().get(),
            credential_selection_epoch,
            route_fingerprint: resolution_route_fingerprint(
                &self.route,
                &descriptor,
                account_fingerprint,
                credential_authority_binding,
                capacity_facts,
                reasoning_efforts,
                catalog_enum_fingerprint,
                responses_lite,
            )?,
            descriptor: descriptor.clone(),
            descriptor_digest: descriptor
                .binding_digest()
                .map_err(|_| NativeCodexAuthError::Catalog)?,
            catalog_receipt: issued.catalog_receipt,
            capacity_facts,
            reasoning_efforts,
            tool_mode,
            responses_lite,
            content_filter_guidance: issued.content_filter_guidance,
            catalog_enum_fingerprint,
            adapter_revision: OPENAI_RESPONSES_ADAPTER_REVISION,
            catalog_observed_at: issued.observed_at,
            catalog_expires_at: issued.expires_at,
            profile_revision: OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        };
        let target = OpenAiTarget::new_native_catalog_target(
            self.route.clone(),
            descriptor,
            issued.witness,
            Arc::clone(&self.clock),
        )
        .map_err(|_| NativeCodexAuthError::Catalog)?;
        // Auth keeps the 30s REST stall. Inference SSE (including compaction)
        // waits the streaming idle bound between frames: a 30s body idle
        // killed Codex auto-compact at ~32s while a later manual compact
        // needed 81s. Rebuild from the caller client so test TLS and
        // inherited observers stay attached.
        let mut inference_client = self
            .client
            .with_limits(Limits::for_streaming_inference())
            .map_err(|_| NativeCodexAuthError::Catalog)?
            .pooled();
        if let Some(observer) = &self.inference_observer {
            inference_client = inference_client.with_observer(Some(Arc::clone(observer)));
        }
        let model = OpenAiLanguageModel::new_codex(target, inference_client, self.config, renewal)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        Ok((Arc::new(model), evidence))
    }
}

impl fmt::Debug for CodexModelResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexModelResolver")
            .field("profile", &OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .field("catalog", &"<credential-free exact snapshot>")
            .field("clock", &"<injected clock>")
            .finish()
    }
}
