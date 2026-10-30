//! The app-owned manager for one explicit xAI selected-account slot.

use super::{
    Arc, AsyncMutex, AtomicU64, AuthEndpoints, AuthTransportError, BrowserLoginFlow, Bytes,
    COMPATIBILITY_EXPIRES_UNIX, CallbackBinding, CatalogReceiptCore, Client, CredentialGeneration,
    CredentialRefreshObserver, CredentialSlotId, CredentialStatus, DeviceResponse, Duration,
    GrokCodingProfile, HEADER_AUTHORIZATION, HEADER_GROK_CLIENT_MODE, HEADER_GROK_CLIENT_VERSION,
    HEADER_X_USER_ID, HEADER_XAI_TOKEN_AUTH, HeaderMap, HeaderValue, Instant, JwksCache,
    JwksResolvePolicy, LOGIN_TIMEOUT, LanguageModel, MAX_AUTH_BODY_BYTES,
    MAX_DEVICE_INTERVAL_SECONDS, MAX_JWKS_KEYS, ManagerCore, Method, ModelFamilyId, ModelId, Mutex,
    Origin, PLAKOUS_XAI_CLIENT_MODE, PathBuf, PkceS256, PrivateCredentialStore, PrivateStoreError,
    Request, SUPERGROK_CLIENT_COMPATIBILITY_VERSION, SecretString, StandardXaiHttpConfig,
    StoredTokens, SubscriptionRequestGate, SubscriptionUsageCredentialState, SubscriptionUsageRead,
    SubscriptionUsageUnavailable, Timestamp, XAI_GROK_COMPATIBILITY_PROFILE, XAI_LOOPBACK_REDIRECT,
    XAI_OAUTH, XAI_OAUTH_SCOPE, XAI_PUBLIC_CLIENT_ID, XAI_RESPONSES_ADAPTER_REVISION,
    XAI_TOKEN_AUTH_VALUE, XaiBrowserLogin, XaiCompatibilityAcceptance, XaiDeviceLogin, XaiDialect,
    XaiGrokSubscriptionRoute, XaiLanguageModel, XaiModel, XaiModelResolutionEvidence,
    XaiNativeAuthError, XaiNativeCatalogReceipt, XaiRemoteBrowserLogin, XaiResolvedModel,
    XaiSubscriptionIntegrationProfile, XaiSubscriptionStatus, auth_profile_id, authorization_url,
    bearer_header, bind_loopback_listener, decode_json, fmt, is_subscription_coding_model_id,
    native_route_authority, network_failure, refuse_retired_payload, sensitive_header_value,
    system_unix_seconds, until_deadline, urlencoded_form, usage_credential_refusal,
    validate_acceptance,
};
#[cfg(test)]
use super::{COMPATIBILITY_REVIEWED_UNIX, refuse_pinned_test_loopback};

/// App-owned manager for one explicit xAI selected-account slot.
///
/// Credential bytes are intentionally not part of the public surface.
///
/// ```compile_fail
/// # use plakous_xai::XaiNativeAuthManager;
/// fn expose(manager: &XaiNativeAuthManager) {
///     let _ = manager.access_token();
/// }
/// ```
pub struct XaiNativeAuthManager {
    pub(super) core: Arc<ManagerCore>,
}

impl XaiNativeAuthManager {
    /// Opens one explicit private selected-slot record.
    ///
    /// No environment, home-directory, owner store, proxy, or endpoint
    /// discovery is consulted.
    ///
    /// # Errors
    ///
    /// Fails for an unaccepted/expired profile or unsafe explicit store path.
    pub fn open_selected_slot(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
        client: Client,
        acceptance: XaiCompatibilityAcceptance,
    ) -> Result<Self, XaiNativeAuthError> {
        validate_acceptance(&acceptance, system_unix_seconds()?)?;
        let profile = auth_profile_id()?;
        let store = PrivateCredentialStore::open_slot(path, profile, slot)?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                catalog_retries: true,
                store,
                client,
                acceptance,
                endpoints: AuthEndpoints::production(),
                compatibility_expires_unix: AtomicU64::new(COMPATIBILITY_EXPIRES_UNIX),
                jwks: JwksCache::new(JwksResolvePolicy::xai(), MAX_JWKS_KEYS),
                refresh_flight: AsyncMutex::new(()),
                catalog: Mutex::new(None),
                request_gate: None,
                refresh_observer: None,
                catalog_observer: None,
                #[cfg(test)]
                resolution_pause: Mutex::new(None),
            }),
        })
    }

    /// Attaches the classified physical-request gate required for inference.
    ///
    /// Leaving the gate absent keeps maintenance and catalog workflows usable,
    /// but every physical subscription inference dispatch fails closed.
    ///
    /// # Errors
    ///
    /// Returns an internal binding error if this manager has already been shared.
    pub fn with_subscription_request_gate(
        mut self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<Self, XaiNativeAuthError> {
        let core = Arc::get_mut(&mut self.core).ok_or(XaiNativeAuthError::Request)?;
        core.request_gate = Some(gate);
        Ok(self)
    }

    /// Attaches a content-free claimed-refresh observer (product telemetry).
    ///
    /// # Errors
    ///
    /// Returns an internal binding error if this manager has already been shared.
    pub fn with_credential_refresh_observer(
        mut self,
        observer: Arc<dyn CredentialRefreshObserver>,
    ) -> Result<Self, XaiNativeAuthError> {
        let core = Arc::get_mut(&mut self.core).ok_or(XaiNativeAuthError::Request)?;
        core.refresh_observer = Some(observer);
        Ok(self)
    }

    /// Limits catalog resolution to one physical GET while a session is attached.
    ///
    /// # Errors
    /// Refuses configuration changes after the manager has been shared.
    pub fn without_catalog_retries(mut self) -> Result<Self, XaiNativeAuthError> {
        let core = Arc::get_mut(&mut self.core).ok_or(XaiNativeAuthError::Request)?;
        core.catalog_retries = false;
        Ok(self)
    }

    /// Observes catalog GETs only, never credential or usage exchanges.
    ///
    /// # Errors
    /// Returns an internal binding error if this manager has already been shared.
    pub fn with_catalog_observer(
        mut self,
        observer: Option<Arc<dyn plakous_http::ExchangeObserver>>,
    ) -> Result<Self, XaiNativeAuthError> {
        let core = Arc::get_mut(&mut self.core).ok_or(XaiNativeAuthError::Request)?;
        core.catalog_observer = observer;
        Ok(self)
    }

    /// Returns the immutable credential-free integration profile.
    #[must_use]
    pub const fn profile(&self) -> XaiSubscriptionIntegrationProfile {
        XaiSubscriptionIntegrationProfile
    }

    /// Returns the current credential-free status.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed or unsafe storage.
    pub fn status(&self) -> Result<Option<XaiSubscriptionStatus>, XaiNativeAuthError> {
        let Some(fence) = self.core.store.load()? else {
            return Ok(None);
        };
        refuse_retired_payload(&self.core.store, &fence)?;
        Ok(Some(XaiSubscriptionStatus {
            generation: fence.generation(),
            status: fence.status(),
            account: fence.account_binding().clone(),
        }))
    }

    /// Performs exactly one read-only Grok subscription usage request.
    ///
    /// This observation acquires no inference permit and grants no catalog,
    /// admission, retry, billing, or inference authority. The exact selected
    /// Ready generation remains leased through the bounded response and decode.
    ///
    /// It never starts a refresh: the hosted usage read is bounded by a timeout
    /// that may drop it, and a refresh dropped after a rotated token was issued
    /// would leave the store on a refresh token the provider may have retired.
    /// An expired access token is reported as `RefreshRequired` without network
    /// I/O; the next turn refreshes it.
    #[allow(
        clippy::too_many_lines,
        reason = "Resolve the selected Grok receipt and retain its diagnostics while classifying the bounded usage response."
    )]
    pub async fn subscription_usage(&self) -> SubscriptionUsageRead {
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(system_unix_seconds().unwrap_or(0), 0)
                .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp")),
            reason,
        };
        if self.core.ensure_active().is_err() {
            return unavailable(SubscriptionUsageUnavailable::Unqualified);
        }
        // Every refusal before the HTTP exchange is local and is named as such:
        // a held refresh flight or store lease is `Busy`, a non-Ready record is
        // its exact `CredentialState`, and unsafe storage or a dead clock is
        // `LocalState`. Only the bounded exchange below may report `Transport`.
        let Some(_lifecycle) = self.core.refresh_flight.try_lock() else {
            return unavailable(SubscriptionUsageUnavailable::Busy);
        };
        let fence = match self.core.store.try_load() {
            Ok(Some(fence)) => fence,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::NotConnected),
            Err(PrivateStoreError::StoreBusy) => {
                return unavailable(SubscriptionUsageUnavailable::Busy);
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        // A durable Unknown record whose claim another process still holds is
        // a refresh in flight, not a settled quarantine.
        if fence.status() == CredentialStatus::RefreshOutcomeUnknown {
            match self.core.store.refresh_outcome_settled(&fence) {
                Ok(true) => {}
                Ok(false) => return unavailable(SubscriptionUsageUnavailable::Busy),
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            }
        }
        if let Some(reason) = usage_credential_refusal(fence.status()) {
            return unavailable(reason);
        }
        let lease = match self.core.store.try_acquire_ready(&fence) {
            Ok(Some(lease)) => lease,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        let stored: StoredTokens = match serde_json::from_slice(lease.payload().expose_secret()) {
            Ok(stored) => stored,
            Err(_) => return unavailable(SubscriptionUsageUnavailable::InvalidResponse),
        };
        match stored.validate(fence.account_binding()) {
            Ok(()) => {}
            Err(XaiNativeAuthError::StoredCredentialFormatRetired) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::LoginRequired,
                ));
            }
            Err(XaiNativeAuthError::StoredCredentialWrittenByNewerBuild) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::WrittenByNewerBuild,
                ));
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::AccountChanged),
        }
        let Ok(now) = system_unix_seconds() else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        if stored.expires_at <= now {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        if lease.fence() != fence || stored.subject != fence.account_binding().as_str() {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }

        let mut headers = HeaderMap::new();
        let Ok(authorization) = bearer_header(&stored.access_token) else {
            return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
        };
        let Ok(subject) = sensitive_header_value(stored.subject.as_bytes().to_vec()) else {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        };
        headers.insert(HEADER_AUTHORIZATION, authorization);
        headers.insert(HEADER_X_USER_ID, subject);
        headers.insert(
            HEADER_XAI_TOKEN_AUTH,
            HeaderValue::from_static(XAI_TOKEN_AUTH_VALUE),
        );
        headers.insert(
            HEADER_GROK_CLIENT_VERSION,
            HeaderValue::from_static(SUPERGROK_CLIENT_COMPATIBILITY_VERSION),
        );
        headers.insert(
            HEADER_GROK_CLIENT_MODE,
            HeaderValue::from_static(PLAKOUS_XAI_CLIENT_MODE),
        );
        let usage_endpoint = self.core.endpoints.usage();
        let Ok(request) = Request::new(
            Origin::parse(&usage_endpoint.origin)
                .unwrap_or_else(|_| unreachable!("the reviewed usage origin is valid")),
            Method::GET,
            &usage_endpoint.path,
            headers,
            Bytes::new(),
            self.core.client.limits(),
        ) else {
            return unavailable(SubscriptionUsageUnavailable::LocalState);
        };
        let (status, is_json, body) = match self.core.execute(request).await {
            Ok(reply) => reply,
            Err(AuthTransportError::TooLarge) => {
                return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
            }
            Err(AuthTransportError::Failure(failure)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(network_failure(
                    failure,
                )));
            }
        };
        let observed_at = Timestamp::new(system_unix_seconds().unwrap_or(now), 0)
            .unwrap_or_else(|_| unreachable!("Unix epoch is a valid timestamp"));
        let reason = match status.as_u16() {
            200 => None,
            401 | 403 => Some(SubscriptionUsageUnavailable::Unauthorized),
            429 => Some(SubscriptionUsageUnavailable::RateLimited),
            _ => Some(SubscriptionUsageUnavailable::ProviderUnavailable),
        };
        if let Some(reason) = reason {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason,
            };
        }
        if !is_json {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            };
        }
        if lease.fence() != fence {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::AccountChanged,
            };
        }
        match crate::decode_grok_subscription_usage(&body, observed_at) {
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(snapshot),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    /// Begins PKCE/state/nonce browser login on the fixed loopback redirect.
    ///
    /// # Errors
    ///
    /// Fails if the compatibility profile is inactive, randomness is
    /// unavailable, or the fixed loopback socket cannot be exclusively bound.
    pub async fn begin_browser_login(&self) -> Result<XaiBrowserLogin, XaiNativeAuthError> {
        self.core.ensure_active()?;
        #[cfg(test)]
        refuse_pinned_test_loopback(self.core.endpoints.loopback_ports);
        let (listener, port) = bind_loopback_listener(self.core.endpoints.loopback_ports)
            .await
            .map_err(|_| XaiNativeAuthError::LoopbackUnavailable)?;
        let flow = self.begin_browser_login_flow(format!(
            "http://127.0.0.1:{port}{}",
            XAI_OAUTH.browser_callback_path
        ))?;
        flow.ensure_active()?;
        Ok(XaiBrowserLogin { flow, listener })
    }

    /// Begins a browser login that can be completed by pasting the full fixed
    /// loopback redirect URL on a different machine or in a different process.
    ///
    /// This method never binds or listens on the callback port. The xAI
    /// authorization request still uses the pinned
    /// `XAI_LOOPBACK_REDIRECT`, and completion accepts only that exact
    /// absolute callback origin, port, and path.
    ///
    /// # Errors
    ///
    /// Fails if the compatibility profile is inactive, secure randomness is
    /// unavailable, or the selected-slot baseline cannot be read safely.
    pub fn begin_remote_browser_login(&self) -> Result<XaiRemoteBrowserLogin, XaiNativeAuthError> {
        Ok(XaiRemoteBrowserLogin {
            flow: self.begin_browser_login_flow(XAI_LOOPBACK_REDIRECT.to_owned())?,
        })
    }

    /// Begins an RFC 8628 device authorization flow.
    ///
    /// # Errors
    ///
    /// Fails on an inactive profile or a strict device response violation.
    pub async fn begin_device_login(&self) -> Result<XaiDeviceLogin, XaiNativeAuthError> {
        self.core.ensure_active()?;
        let authorization_deadline = self.core.flow_deadline(LOGIN_TIMEOUT)?;
        let baseline = self.core.store.load()?;
        let form = urlencoded_form(&[
            ("client_id", XAI_PUBLIC_CLIENT_ID),
            ("scope", XAI_OAUTH_SCOPE),
        ]);
        let response = until_deadline(
            authorization_deadline,
            || XaiNativeAuthError::LoginExpired,
            self.core.send_form(self.core.endpoints.device(), form),
        )
        .await?;
        let response_received_at = Instant::now();
        self.core.ensure_flow_active(authorization_deadline)?;
        let mut device: DeviceResponse = decode_json(&response, MAX_AUTH_BODY_BYTES)?;
        device.validate()?;
        let interval = Duration::from_secs(
            device
                .interval
                .unwrap_or(5)
                .clamp(1, MAX_DEVICE_INTERVAL_SECONDS),
        );
        let expires = Duration::from_secs(device.expires_in);
        let provider_deadline = response_received_at
            .checked_add(expires)
            .ok_or(XaiNativeAuthError::LoginExpired)?;
        let deadline = provider_deadline.min(self.core.flow_deadline(expires)?);
        self.core.ensure_flow_active(deadline)?;
        Ok(XaiDeviceLogin {
            core: Arc::clone(&self.core),
            baseline,
            device_code: SecretString::from(std::mem::take(&mut device.device_code)),
            user_code: std::mem::take(&mut device.user_code),
            verification_uri: std::mem::take(&mut device.verification_uri),
            verification_uri_complete: device.verification_uri_complete.take(),
            interval,
            deadline,
        })
    }

    /// Creates a freshly account-bound native subscription route.
    ///
    /// # Errors
    ///
    /// Requires a current Ready selected login and active compatibility profile.
    pub fn subscription_route(&self) -> Result<XaiGrokSubscriptionRoute, XaiNativeAuthError> {
        self.core.ensure_active()?;
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(XaiNativeAuthError::NotReady);
        }
        let stored = self.core.read_stored(&fence)?;
        let receipt = self
            .core
            .active_catalog_receipt(&fence, &stored, system_unix_seconds()?)?;
        self.subscription_route_from_receipt(&receipt)
    }

    fn subscription_route_from_receipt(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<XaiGrokSubscriptionRoute, XaiNativeAuthError> {
        let parts = native_route_authority(&self.core, receipt)?;
        XaiGrokSubscriptionRoute::from_native_authority(parts.authority, parts.admission)
            .map_err(Into::into)
    }

    /// Fetches and validates a fresh authenticated account-scoped model catalog.
    ///
    /// # Errors
    ///
    /// Fails unless the selected credential is Ready and the strict catalog
    /// admits at least one SuperGrok coding model identity.
    pub async fn refresh_catalog(&self) -> Result<XaiNativeCatalogReceipt, XaiNativeAuthError> {
        Ok(self.core.refresh_catalog_core().await?.public)
    }

    /// Resolves the caller's exact admitted direct Responses model.
    ///
    /// The resulting descriptor carries the immutable compatibility profile and
    /// conservative route-evidenced capabilities.
    ///
    /// # Errors
    ///
    /// Rejects non-coding identities before credential or network access.
    /// Catalog marketing aliases never expand: only exact catalog `id` values
    /// admit after authenticated discovery. Then fails unless route,
    /// compatibility, store, catalog, and model identities are current.
    pub async fn resolve_model(
        &self,
        requested_model: &str,
        client: Client,
    ) -> Result<Arc<dyn LanguageModel>, XaiNativeAuthError> {
        Ok(self
            .resolve_model_with_evidence(requested_model, client, StandardXaiHttpConfig::default())
            .await?
            .0)
    }

    /// Resolves one exact model and its atomically corresponding evidence.
    ///
    /// The returned model and evidence are constructed while retaining a shared
    /// lease for the exact catalog-authenticating credential fence and the lock
    /// for that exact cached catalog receipt. A completed relogin, generation
    /// change, catalog mutation, or expiry before this snapshot is acquired
    /// therefore fails closed instead of returning mixed evidence.
    ///
    /// # Errors
    ///
    /// Rejects non-coding identities before credential or network access.
    /// After authenticated catalog discovery, rejects alias/marketing names
    /// (not present as exact `id`), stale credential fences, replaced catalog
    /// receipts, inactive profiles, or inconsistent route/descriptor identity.
    pub async fn resolve_model_with_evidence(
        &self,
        requested_model: &str,
        client: Client,
        config: StandardXaiHttpConfig,
    ) -> Result<(Arc<dyn LanguageModel>, XaiModelResolutionEvidence), XaiNativeAuthError> {
        let (model, evidence) = self
            .resolve_exact_model(requested_model, client, config)
            .await?;
        let model: Arc<dyn LanguageModel> = model;
        Ok((model, evidence))
    }

    /// Resolves the model as [`Self::resolve_model_with_evidence`] does,
    /// with the hosted web and X search side-request executor on the same
    /// route, login, model and client.
    ///
    /// # Errors
    ///
    /// Same failures as [`Self::resolve_model_with_evidence`].
    pub async fn resolve_model_with_retrieval(
        &self,
        requested_model: &str,
        client: Client,
        config: StandardXaiHttpConfig,
    ) -> Result<
        (
            Arc<dyn LanguageModel>,
            XaiModelResolutionEvidence,
            crate::XaiRetrieval,
            crate::XaiImages,
        ),
        XaiNativeAuthError,
    > {
        let (model, evidence) = self
            .resolve_exact_model(requested_model, client, config)
            .await?;
        let retrieval = model.retrieval();
        let images = model.images();
        let model: Arc<dyn LanguageModel> = model;
        Ok((model, evidence, retrieval, images))
    }

    async fn resolve_exact_model(
        &self,
        requested_model: &str,
        client: Client,
        config: StandardXaiHttpConfig,
    ) -> Result<(Arc<XaiLanguageModel>, XaiModelResolutionEvidence), XaiNativeAuthError> {
        let requested = ModelId::new(requested_model)?;
        if !is_subscription_coding_model_id(requested.as_str()) {
            return Err(XaiNativeAuthError::RequestedModelNotAdmitted);
        }
        self.core.ensure_active()?;
        let receipt = self.core.refresh_catalog_core().await?;
        #[cfg(test)]
        self.core.pause_resolution_after_catalog().await?;
        let catalog_entry = receipt
            .public
            .entry(&requested)
            .ok_or(XaiNativeAuthError::RequestedModelNotAdmitted)?;
        let catalog_diagnostics = catalog_entry.diagnostics();
        let resolution_lease = self.core.acquire_resolution_lease(&receipt)?;
        let catalog = self
            .core
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
        if catalog.as_ref() != Some(&receipt) {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let route = self.subscription_route_from_receipt(&receipt)?;
        let resolved = XaiResolvedModel::new(
            route,
            XaiModel::new(requested, Some(ModelFamilyId::new("responses-family")?)),
            GrokCodingProfile::pinned_compatibility(),
            receipt.public.observed_at,
        )?;
        let identity = resolved
            .route()
            .identity()
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?;
        if identity.credential_authority() != receipt.authority_id
            || resolved.route().dialect() != XaiDialect::GrokSubscriptionResponses2026_07
            || resolved.descriptor().profile().as_str() != XAI_GROK_COMPATIBILITY_PROFILE
        {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let evidence = XaiModelResolutionEvidence {
            evidence_revision: 2,
            account_fingerprint: identity.account_binding(),
            credential_authority_id: identity.credential_authority().to_owned(),
            credential_authority_binding: identity.credential_authority_binding(),
            selected_credential_generation: receipt.fence.generation().get(),
            credential_selection_epoch: receipt.selection_epoch.clone(),
            route_fingerprint: identity.fingerprint().to_owned(),
            descriptor: resolved.descriptor().clone(),
            descriptor_digest: resolved.descriptor().binding_digest()?,
            dialect: resolved.route().dialect(),
            adapter_revision: XAI_RESPONSES_ADAPTER_REVISION.to_owned(),
            catalog_observed_at: receipt.public.observed_at,
            catalog_expires_at: receipt.public.expires_at,
            catalog_diagnostics,
            runtime_admission_witness: resolved
                .route()
                .runtime_admission_witness(
                    resolved.descriptor(),
                    resolved.model().remote_id(),
                    resolved.descriptor().profile(),
                )
                .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?,
            profile_revision: XAI_GROK_COMPATIBILITY_PROFILE.to_owned(),
        };
        let model = XaiLanguageModel::with_config(resolved, client, config)?;
        self.core.ensure_resolution_receipt_current(&receipt)?;
        drop(catalog);
        drop(resolution_lease);
        Ok((Arc::new(model), evidence))
    }

    /// Refreshes the selected login when its exact stored expiry enters skew.
    ///
    /// # Errors
    ///
    /// Ambiguous, invalid, account-changing, or persistence failures leave the
    /// durable credential quarantined.
    pub async fn refresh_if_needed(&self) -> Result<(), XaiNativeAuthError> {
        self.core.refresh_if_needed(None, None, None).await
    }

    /// Claims one refresh grant regardless of the stored expiry (operator
    /// `refresh`, or a usage read the provider rejected with 401/403).
    ///
    /// # Errors
    ///
    /// Same fail-closed failures as [`Self::refresh_if_needed`].
    pub async fn refresh_now(&self) -> Result<(), XaiNativeAuthError> {
        self.core.refresh_with_force(None, None, None, true).await
    }

    /// Stored access-token expiry (Unix seconds) of the Ready credential.
    ///
    /// # Errors
    ///
    /// Fails for a missing, non-Ready, or unreadable record.
    pub fn access_expires_at(&self) -> Result<u64, XaiNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        Ok(self.core.read_stored(&fence)?.expires_at)
    }

    /// Replaces the current credential with a local revoked tombstone.
    ///
    /// # Errors
    ///
    /// Rejects stale or missing state.
    pub fn logout(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        Ok(self.core.store.logout(fence.generation())?)
    }

    /// Revokes the refresh token at the fixed issuer and then writes a local tombstone.
    ///
    /// # Errors
    ///
    /// Any non-200 or ambiguous remote result leaves the selected slot quarantined.
    pub async fn revoke(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        self.core.revoke().await
    }

    fn begin_browser_login_flow(
        &self,
        redirect_uri: String,
    ) -> Result<BrowserLoginFlow, XaiNativeAuthError> {
        self.core.ensure_active()?;
        let deadline = self.core.flow_deadline(LOGIN_TIMEOUT)?;
        let baseline = self.core.store.load()?;
        let pkce = PkceS256::generate()?;
        let binding = CallbackBinding::generate()?;
        let authorization_url =
            authorization_url(&self.core.endpoints, &redirect_uri, &pkce, &binding)?;
        self.core.ensure_flow_active(deadline)?;
        Ok(BrowserLoginFlow {
            core: Arc::clone(&self.core),
            baseline,
            redirect_uri,
            pkce,
            binding,
            authorization_url,
            deadline,
        })
    }

    #[cfg(test)]
    pub(super) fn open_test_slot(
        path: impl Into<PathBuf>,
        client: Client,
        issuer_origin: &str,
    ) -> Result<Self, XaiNativeAuthError> {
        let profile = auth_profile_id()?;
        let slot = CredentialSlotId::new("test-selected")?;
        let store = PrivateCredentialStore::open_slot(path, profile, slot)?;
        Ok(Self {
            core: Arc::new(ManagerCore {
                catalog_retries: true,
                store,
                client,
                acceptance: XaiCompatibilityAcceptance {
                    accepted_at: COMPATIBILITY_REVIEWED_UNIX,
                },
                endpoints: AuthEndpoints::test(issuer_origin),
                compatibility_expires_unix: AtomicU64::new(COMPATIBILITY_EXPIRES_UNIX),
                jwks: JwksCache::new(JwksResolvePolicy::xai(), MAX_JWKS_KEYS),
                refresh_flight: AsyncMutex::new(()),
                catalog: Mutex::new(None),
                request_gate: None,
                refresh_observer: None,
                catalog_observer: None,
                resolution_pause: Mutex::new(None),
            }),
        })
    }

    #[cfg(test)]
    pub(super) fn test_subscription_route(
        &self,
        origin: crate::XaiOrigin,
    ) -> Result<XaiGrokSubscriptionRoute, XaiNativeAuthError> {
        let fence = self
            .core
            .store
            .load()?
            .ok_or(XaiNativeAuthError::NotLoggedIn)?;
        let stored = self.core.read_stored(&fence)?;
        let receipt = self
            .core
            .active_catalog_receipt(&fence, &stored, system_unix_seconds()?)?;
        let parts = native_route_authority(&self.core, &receipt)?;
        XaiGrokSubscriptionRoute::from_native_authority_with_origin(
            parts.authority,
            parts.admission,
            origin,
        )
        .map_err(Into::into)
    }
}

impl fmt::Debug for XaiNativeAuthManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiNativeAuthManager")
            .field("store", &self.core.store)
            .field("profile", &XAI_GROK_COMPATIBILITY_PROFILE)
            .finish_non_exhaustive()
    }
}
