//! The credential manager for one explicit Codex selected-account slot.

use super::{
    AUTH_REQUEST_CAP, AccountBinding, Arc, AsyncMutex, AuthProfileId, AuthSingleFlight, Client,
    Clock, CodexCompatibilityAcceptance, CodexCredentialStatus,
    CodexSubscriptionIntegrationProfile, CredentialFence, CredentialRefreshObserver,
    CredentialSlotId, CredentialStatus, Digest, DigestDomain, HeaderMap, HeaderName, HeaderValue,
    Instant, JwksCache, JwksResolvePolicy, MAX_JWKS_NEGATIVE_KEYS, Method, Mutex,
    NativeCodexAuthError, NativeCodexCatalogWitness, OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
    PROFILE_KILLED, Path, PathBuf, PrivateCredentialStore, PrivateStoreError, ProfileEndpoints,
    ResolvedModelDescriptor, RsaJwk, SelectedAccount, SubscriptionRequestGate,
    SubscriptionUsageCredentialState, SubscriptionUsageRead, SubscriptionUsageUnavailable,
    SystemClock, Timestamp, USER_AGENT, execute_bounded, fmt, local_credential_status,
    parse_stored, require_json_media_type, sensitive_header_value, usage_credential_refusal,
    validate_profile, validate_profile_policy_at,
};

/// Plak-owned credential manager for one explicit selected-account slot.
pub struct CodexSubscriptionCredentialManager {
    pub(super) clock: Arc<dyn Clock>,
    pub(super) store: Arc<PrivateCredentialStore>,
    pub(super) client: Client,
    pub(super) endpoints: ProfileEndpoints,
    pub(super) acceptance: CodexCompatibilityAcceptance,
    pub(super) selected: Mutex<SelectedAccount>,
    pub(super) lifecycle: AsyncMutex<()>,
    pub(super) flights: AuthSingleFlight,
    pub(super) jwks: JwksCache<RsaJwk>,
    pub(super) authority_binding: Digest,
    pub(super) request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
    pub(super) refresh_observer: Option<Arc<dyn CredentialRefreshObserver>>,
}

impl CodexSubscriptionCredentialManager {
    /// Opens one application-selected private slot. No ambient path, home,
    /// environment, external login, or process credential is consulted.
    ///
    /// # Errors
    ///
    /// Rejects a relative/unsafe path, profile drift, expiry, kill switch, or
    /// malformed existing record.
    pub fn new(
        slot_path: impl Into<PathBuf>,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
    ) -> Result<Self, NativeCodexAuthError> {
        Self::new_with_endpoints(
            &slot_path.into(),
            client,
            acceptance,
            ProfileEndpoints::production(),
            Arc::new(SystemClock),
        )
    }

    #[cfg(any(test, feature = "test-clock"))]
    #[doc(hidden)]
    pub fn new_with_clock(
        slot_path: impl Into<PathBuf>,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, NativeCodexAuthError> {
        Self::new_with_endpoints(
            &slot_path.into(),
            client,
            acceptance,
            ProfileEndpoints::production(),
            clock,
        )
    }

    pub(super) fn new_with_endpoints(
        slot_path: &Path,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
        endpoints: ProfileEndpoints,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, NativeCodexAuthError> {
        let mut manager = Self::new_with_endpoints_and_policy(
            slot_path,
            client,
            acceptance,
            endpoints,
            clock.now().seconds(),
            PROFILE_KILLED,
        )?;
        manager.clock = clock;
        Ok(manager)
    }

    pub(super) fn new_with_endpoints_and_policy(
        slot_path: &Path,
        client: Client,
        acceptance: CodexCompatibilityAcceptance,
        endpoints: ProfileEndpoints,
        now: u64,
        killed: bool,
    ) -> Result<Self, NativeCodexAuthError> {
        validate_profile_policy_at(acceptance.accepted_at, now, killed)?;
        let auth_profile = AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .map_err(|_| NativeCodexAuthError::Validation)?;
        let slot =
            CredentialSlotId::new("primary").map_err(|_| NativeCodexAuthError::Validation)?;
        let store = Arc::new(
            PrivateCredentialStore::open_slot(slot_path, auth_profile, slot)
                .map_err(|_| NativeCodexAuthError::Store)?,
        );
        let existing = store.load().map_err(|error| match error {
            PrivateStoreError::WrittenByNewerBuild => NativeCodexAuthError::WrittenByNewerBuild,
            PrivateStoreError::ForeignProfile => NativeCodexAuthError::ProfileChanged,
            _ => NativeCodexAuthError::Store,
        })?;
        let selected = SelectedAccount {
            binding: existing
                .as_ref()
                .map(|fence| fence.account_binding().clone()),
        };
        let authority_binding = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-authority",
                OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
                slot_path.as_os_str().as_encoded_bytes(),
            ],
        )
        .map_err(|_| NativeCodexAuthError::Validation)?;
        Ok(Self {
            store,
            client,
            endpoints,
            clock: Arc::new(SystemClock),
            acceptance,
            selected: Mutex::new(selected),
            lifecycle: AsyncMutex::new(()),
            flights: AuthSingleFlight::new(),
            jwks: JwksCache::new(JwksResolvePolicy::codex(), MAX_JWKS_NEGATIVE_KEYS),
            authority_binding,
            request_gate: None,
            refresh_observer: None,
        })
    }

    /// Binds callbacks on an ephemeral port and optionally serves test catalogs
    /// at an explicit fixture origin. Never enabled by production edges.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_fixture_endpoints_for_tests(
        mut self,
        catalog_origin: Option<crate::EndpointOrigin>,
    ) -> Self {
        self.endpoints.loopback_ports = &[0];
        if let Some(origin) = catalog_origin {
            self.endpoints.chatgpt_origin = origin;
        }
        self
    }

    /// Attaches the classified physical-request gate required for inference.
    ///
    /// Leaving the gate absent keeps maintenance and catalog workflows usable,
    /// but every physical subscription inference dispatch fails closed.
    #[must_use]
    pub fn with_subscription_request_gate(
        mut self,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Self {
        self.request_gate = Some(gate);
        self
    }

    /// Attaches a content-free claimed-refresh observer (product telemetry).
    #[must_use]
    pub fn with_credential_refresh_observer(
        mut self,
        observer: Arc<dyn CredentialRefreshObserver>,
    ) -> Self {
        self.refresh_observer = Some(observer);
        self
    }

    /// Returns immutable credential-free profile facts.
    #[must_use]
    pub const fn profile(&self) -> CodexSubscriptionIntegrationProfile {
        CodexSubscriptionIntegrationProfile
    }

    /// Returns the exact explicit slot path selected by the application.
    #[must_use]
    pub fn slot_path(&self) -> &Path {
        self.store.path()
    }

    /// Returns credential-free lifecycle status.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe or malformed storage.
    pub fn status(&self) -> Result<CodexCredentialStatus, NativeCodexAuthError> {
        local_credential_status(&self.store)
    }

    /// Stored access-token expiry (Unix seconds) of the Ready credential.
    ///
    /// # Errors
    ///
    /// Fails for a missing, non-Ready, or unreadable record.
    pub fn access_expires_at(&self) -> Result<u64, NativeCodexAuthError> {
        let fence = self
            .store
            .load()
            .map_err(|_| NativeCodexAuthError::Store)?
            .ok_or(NativeCodexAuthError::Unavailable)?;
        let lease = self
            .store
            .acquire_ready(&fence)
            .map_err(|_| NativeCodexAuthError::Unavailable)?;
        Ok(parse_stored(lease.payload())?.access_expires_at)
    }

    /// Performs exactly one read-only Codex subscription usage request.
    ///
    /// This control-plane observation does not acquire an inference permit,
    /// dispatch a model request, retry, redirect, or fall back to another auth
    /// route. The selected Ready generation remains leased through the complete
    /// bounded response and is rechecked before the snapshot is returned.
    ///
    /// It never starts a refresh: Codex refresh tokens rotate, so a claimed
    /// refresh dropped mid-exchange (the hosted usage read is bounded by a
    /// timeout) would leave a durable Unknown fence that only a new login
    /// clears. An expired access token is reported as `RefreshRequired`
    /// without network I/O; the next turn refreshes it.
    pub async fn subscription_usage(&self) -> SubscriptionUsageRead {
        let deadline = Instant::now() + AUTH_REQUEST_CAP;
        let unavailable = |reason| SubscriptionUsageRead::Unavailable {
            observed_at: Timestamp::new(self.clock.now().seconds(), 0)
                .unwrap_or_else(|_| unreachable!("system epoch is a valid timestamp")),
            reason,
        };
        if self.validate_profile_admission().is_err() {
            return unavailable(SubscriptionUsageUnavailable::Unqualified);
        }
        // Every refusal before the HTTP exchange is local and is named as such:
        // a held lifecycle or store lease is `Busy`, a non-Ready record is its
        // exact `CredentialState`, and unsafe storage is `LocalState`. Only the
        // bounded exchange below may report `Transport`.
        let Ok(_lifecycle) = self.lifecycle.try_lock() else {
            return unavailable(SubscriptionUsageUnavailable::Busy);
        };
        let fence = match self.try_load_fence().await {
            Ok(fence) => fence,
            Err(NativeCodexAuthError::Unavailable) => {
                return unavailable(SubscriptionUsageUnavailable::NotConnected);
            }
            Err(NativeCodexAuthError::StoreBusy) => {
                return unavailable(SubscriptionUsageUnavailable::Busy);
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        if fence.status() == CredentialStatus::RefreshOutcomeUnknown {
            match self.refresh_outcome_settled(fence.clone()).await {
                Ok(true) => {}
                Ok(false) => return unavailable(SubscriptionUsageUnavailable::Busy),
                Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
            }
        }
        if let Some(reason) = usage_credential_refusal(fence.status()) {
            return unavailable(reason);
        }
        let lease = match self.try_acquire_ready_lease(fence.clone()).await {
            Ok(Some(lease)) => lease,
            Ok(None) => return unavailable(SubscriptionUsageUnavailable::Busy),
            Err(_) => return unavailable(SubscriptionUsageUnavailable::LocalState),
        };
        let credential = match parse_stored(lease.payload()) {
            Ok(credential) => credential,
            Err(NativeCodexAuthError::WrittenByNewerBuild) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::WrittenByNewerBuild,
                ));
            }
            Err(_) => {
                return unavailable(SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::LoginRequired,
                ));
            }
        };
        if credential.access_expires_at <= self.clock.now().seconds() {
            return unavailable(SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired,
            ));
        }
        if lease.fence() != fence
            || fence.account_binding().as_str() != credential.account_id
            || self.selected_account().as_ref() != Ok(fence.account_binding())
        {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("application/json"),
        );
        let mut bearer = Vec::with_capacity(7 + credential.access_token.len());
        bearer.extend_from_slice(b"Bearer ");
        bearer.extend_from_slice(credential.access_token.as_bytes());
        let Ok(authorization) = sensitive_header_value(bearer) else {
            return unavailable(SubscriptionUsageUnavailable::InvalidResponse);
        };
        let Ok(account) = sensitive_header_value(credential.account_id.as_bytes().to_vec()) else {
            return unavailable(SubscriptionUsageUnavailable::AccountChanged);
        };
        headers.insert(HeaderName::from_static("authorization"), authorization);
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(USER_AGENT),
        );
        let reply = match execute_bounded(
            &self.client,
            &self.endpoints.chatgpt_origin,
            Method::GET,
            crate::subscription_usage::CODEX_SUBSCRIPTION_USAGE_PATH,
            headers,
            Vec::new(),
            deadline,
        )
        .await
        {
            Ok(reply) => reply,
            Err(NativeCodexAuthError::Transport(cause)) => {
                return unavailable(SubscriptionUsageUnavailable::Transport(cause));
            }
            Err(_) => return unavailable(SubscriptionUsageUnavailable::Transport(None)),
        };
        let observed_at = Timestamp::new(self.clock.now().seconds(), 0)
            .unwrap_or_else(|_| unreachable!("system epoch is a valid timestamp"));
        let reason = match reply.status.as_u16() {
            401 | 403 => Some(SubscriptionUsageUnavailable::Unauthorized),
            429 => Some(SubscriptionUsageUnavailable::RateLimited),
            200 => None,
            _ => Some(SubscriptionUsageUnavailable::ProviderUnavailable),
        };
        if let Some(reason) = reason {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason,
            };
        }
        if require_json_media_type(&reply).is_err() {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            };
        }
        // The lease retains the exact Ready record and its cross-process
        // shared lock through this observation. Reopening the credential file
        // here would recursively contend on that same inode; the lifecycle
        // guard and selected-account check cover this manager while the lease
        // fences replacement, refresh, logout, and revoke elsewhere.
        if lease.fence() != fence || self.selected_account().as_ref() != Ok(fence.account_binding())
        {
            return SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::AccountChanged,
            };
        }
        match crate::decode_codex_subscription_usage(&reply.body, observed_at) {
            Ok(snapshot) if snapshot.contains_ended_window() => {
                SubscriptionUsageRead::Unavailable {
                    observed_at,
                    reason: SubscriptionUsageUnavailable::Stale,
                }
            }
            Ok(snapshot) => SubscriptionUsageRead::Snapshot(snapshot),
            Err(_) => SubscriptionUsageRead::Unavailable {
                observed_at,
                reason: SubscriptionUsageUnavailable::InvalidResponse,
            },
        }
    }

    pub(crate) fn validate_profile_admission(&self) -> Result<(), NativeCodexAuthError> {
        validate_profile(&self.acceptance, self.clock.now().seconds())
    }

    pub(crate) fn validate_route_resolution(&self) -> Result<(), NativeCodexAuthError> {
        self.validate_profile_admission()?;
        let _selected = self.selected_account()?;
        Ok(())
    }

    pub(super) fn selected_account(&self) -> Result<AccountBinding, NativeCodexAuthError> {
        self.selected
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .binding
            .clone()
            .ok_or(NativeCodexAuthError::Unavailable)
    }

    pub(super) fn install_selected_account(
        &self,
        account: AccountBinding,
    ) -> Result<(), NativeCodexAuthError> {
        self.selected
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .binding = Some(account);
        Ok(())
    }

    pub(super) fn clear_selected_account(&self) -> Result<(), NativeCodexAuthError> {
        self.selected
            .lock()
            .map_err(|_| NativeCodexAuthError::Coordination)?
            .binding = None;
        Ok(())
    }

    pub(super) fn route_account_digest(&self) -> Digest {
        let selected = self.selected_account().ok();
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-account",
                OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
                selected
                    .as_ref()
                    .map_or(b"unselected".as_slice(), |value| value.as_str().as_bytes()),
            ],
        )
        .unwrap_or_else(|error| unreachable!("static digest version is valid: {error}"))
    }

    pub(super) fn catalog_witness_binding(
        &self,
        descriptor: &ResolvedModelDescriptor,
        credential_fence: &CredentialFence,
        credential_selection_epoch: Digest,
        observed_at: Timestamp,
        expires_at: Timestamp,
        responses_lite: bool,
    ) -> Result<Digest, NativeCodexAuthError> {
        let descriptor_binding = descriptor
            .binding_digest()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let account_binding = self.route_account_digest();
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-catalog-witness",
                self.authority_binding.bytes(),
                account_binding.bytes(),
                OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
                credential_selection_epoch.bytes(),
                &credential_fence.generation().get().to_be_bytes(),
                &observed_at.seconds().to_be_bytes(),
                &observed_at.nanoseconds().to_be_bytes(),
                &expires_at.seconds().to_be_bytes(),
                &expires_at.nanoseconds().to_be_bytes(),
                descriptor.requested_model().as_str().as_bytes(),
                descriptor_binding.bytes(),
                &[u8::from(responses_lite)],
            ],
        )
        .map_err(|_| NativeCodexAuthError::Catalog)
    }

    pub(super) fn issue_catalog_witness(
        &self,
        descriptor: &ResolvedModelDescriptor,
        credential_fence: &CredentialFence,
        selection_epoch: Digest,
        observed_at: Timestamp,
        expires_at: Timestamp,
        responses_lite: bool,
    ) -> Result<NativeCodexCatalogWitness, NativeCodexAuthError> {
        if credential_fence.status() != CredentialStatus::Ready
            || credential_fence.profile_id().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || credential_fence.account_binding() != &self.selected_account()?
        {
            return Err(NativeCodexAuthError::Catalog);
        }
        Ok(NativeCodexCatalogWitness::new(
            self.catalog_witness_binding(
                descriptor,
                credential_fence,
                selection_epoch,
                observed_at,
                expires_at,
                responses_lite,
            )?,
            credential_fence.clone(),
            selection_epoch,
            observed_at,
            expires_at,
            responses_lite,
        ))
    }
}

impl fmt::Debug for CodexSubscriptionCredentialManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexSubscriptionCredentialManager")
            .field("profile", &OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
            .field("slot", &"<explicit private path>")
            .field("credential", &"<redacted>")
            .finish()
    }
}
