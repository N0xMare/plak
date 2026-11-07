//! The shared manager core: admission, refresh, catalog and resolution.

#[cfg(test)]
use super::ensure_compatibility;
use super::{
    ACCESS_REFRESH_LEAD_SECONDS, AccountBinding, Arc, AuthTransportError, Bytes, CATALOG_PATH,
    CATALOG_TTL_SECONDS, COMPATIBILITY_EXPIRES_UNIX, CatalogReceiptCore, CatalogResponse,
    CredentialFence, CredentialGeneration, CredentialRecord, CredentialRefreshDetail,
    CredentialStatus, Duration, Endpoint, ExchangeControl, Failure, HEADER_ACCEPT,
    HEADER_AUTHORIZATION, HeaderValue, Instant, LoginHttpStage, MAX_AUTH_BODY_BYTES,
    MAX_TOKEN_BYTES, ManagerCore, Method, Ordering, Origin, PkceS256, REQUEST_TIMEOUT,
    ReadyCredentialLease, Request, SecretString, SensitiveBytes, StatusCode, StoredTokens,
    Timestamp, TokenResponse, XAI_GROK_COMPATIBILITY_PROFILE, XAI_PUBLIC_CLIENT_ID,
    XaiNativeAuthError, XaiNativeCatalogReceipt, XaiRefreshCancelGuard, XaiRefreshHooks,
    XaiRevokeHooks, XaiSubscriptionStatus, Zeroizing, access_needs_refresh,
    authorization_code_token_form, bearer_header, credential_record, decode_json, fixed_headers,
    headers_are_json, insert_subscription_catalog_headers, network_failure, remaining_until,
    route_credential_authority_id, run_claimed_refresh, run_claimed_revoke, status_error,
    system_unix_elapsed, system_unix_seconds, urlencoded_form, valid_admitted_catalog_entries,
    valid_login_selection_epoch, validate_acceptance_until,
};
use plakous_auth::oauth_orchestrator::pull_auth_body;

impl ManagerCore {
    pub(super) async fn validate_login_discovery(&self) -> Result<(), XaiNativeAuthError> {
        // Only fixture constructors replace the compiled auth origin with
        // loopback. No URL served by discovery ever reaches dispatch.
        let endpoint = Endpoint {
            origin: self.endpoints.token().origin.clone(),
            path: url::Url::parse(
                super::XAI_OAUTH
                    .oidc_discovery_url
                    .ok_or(XaiNativeAuthError::Request)?,
            )
            .map_err(|_| XaiNativeAuthError::Request)?
            .path()
            .to_owned(),
        };
        let bytes = self
            .get_login_json(&endpoint, None, LoginHttpStage::AccountVerification)
            .await?;
        let document: serde_json::Value = decode_json(&bytes, MAX_AUTH_BODY_BYTES)?;
        plakous_auth::oauth_workflow::validate_subscription_oidc(super::XAI_OAUTH, &document)
            .map_err(XaiNativeAuthError::CompatibilityDrift)
    }

    pub(super) fn ensure_active(&self) -> Result<(), XaiNativeAuthError> {
        validate_acceptance_until(
            &self.acceptance,
            system_unix_seconds()?,
            self.compatibility_expires_unix(),
        )
    }

    pub(super) fn compatibility_expires_unix(&self) -> u64 {
        self.compatibility_expires_unix.load(Ordering::Relaxed)
    }

    pub(super) fn ensure_flow_active(&self, deadline: Instant) -> Result<(), XaiNativeAuthError> {
        self.ensure_active()?;
        remaining_until(deadline, Instant::now()).ok_or(XaiNativeAuthError::LoginExpired)?;
        Ok(())
    }

    pub(super) fn flow_deadline(&self, maximum: Duration) -> Result<Instant, XaiNativeAuthError> {
        self.ensure_active()?;
        let monotonic_now = Instant::now();
        let now = system_unix_elapsed()?;
        let expiry = Duration::from_secs(self.compatibility_expires_unix());
        let remaining = expiry
            .checked_sub(now)
            .filter(|remaining| !remaining.is_zero())
            .ok_or(XaiNativeAuthError::CompatibilityInactive)?;
        monotonic_now
            .checked_add(maximum.min(remaining))
            .ok_or(XaiNativeAuthError::LoginExpired)
    }

    pub(super) async fn exchange_code(
        &self,
        code: Zeroizing<String>,
        redirect_uri: &str,
        pkce: &PkceS256,
        nonce: Option<&str>,
        baseline: Option<&CredentialFence>,
        deadline: Instant,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.ensure_flow_active(deadline)?;
        self.validate_login_discovery().await?;
        self.ensure_flow_active(deadline)?;
        let form = urlencoded_form(&authorization_code_token_form(
            code.as_str(),
            redirect_uri,
            XAI_PUBLIC_CLIENT_ID,
            pkce.verifier().expose_secret(),
        ));
        let response = self
            .send_login_token_form(self.endpoints.token(), form)
            .await?;
        self.ensure_flow_active(deadline)?;
        let tokens: TokenResponse = decode_json(&response, MAX_TOKEN_BYTES)
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        let stored = self
            .validate_tokens(tokens, nonce, None, None, None, Some(deadline))
            .await?;
        self.ensure_flow_active(deadline)?;
        self.persist_login(baseline, &stored, Some(deadline))
    }

    pub(super) fn ensure_login_validation_active(
        &self,
        flow_deadline: Option<Instant>,
    ) -> Result<(), XaiNativeAuthError> {
        if let Some(deadline) = flow_deadline {
            self.ensure_flow_active(deadline)
        } else {
            self.ensure_active()
        }
    }

    pub(super) async fn refresh_catalog_core(
        &self,
    ) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        self.refresh_catalog_with(None).await
    }

    /// Fetches the authenticated catalog. A renewal binds both the refresh
    /// and the fetch to its exact selection and caps the receipt's
    /// observation at its callers' check time (see [`CatalogRenewal`]).
    pub(super) async fn refresh_catalog_with(
        &self,
        renewal: Option<&CatalogRenewal<'_>>,
    ) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        self.ensure_active()?;
        match self
            .refresh_if_needed(
                renewal.map(|renewal| renewal.account),
                renewal.map(|renewal| renewal.selection_epoch),
                renewal.map(|renewal| renewal.authority_id),
            )
            .await
        {
            Ok(()) => {}
            Err(XaiNativeAuthError::NotReady) => {
                let fence = self.store.load()?.ok_or(XaiNativeAuthError::NotLoggedIn)?;
                if fence.status() != CredentialStatus::Ready {
                    return Err(XaiNativeAuthError::NotReady);
                }
            }
            Err(error) => return Err(error),
        }
        let fence = self.store.load()?.ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(XaiNativeAuthError::NotReady);
        }
        let lease = self.store.acquire_ready(&fence)?;
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(fence.account_binding())?;
        if let Some(renewal) = renewal
            && (fence.account_binding() != renewal.account
                || stored.selection_epoch != renewal.selection_epoch
                || route_credential_authority_id(&self.store, self.source_scope.as_deref())
                    != renewal.authority_id)
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let requested_at = system_unix_seconds()?;
        let response = self
            .get(
                self.endpoints.catalog(),
                Some(&stored.access_token),
                Some(fence.account_binding()),
                renewal.is_none() && self.catalog_retries,
            )
            .await?;
        let catalog: CatalogResponse = decode_json(&response, MAX_AUTH_BODY_BYTES)?;
        let admitted_entries = catalog.admitted_coding_entries()?;
        let now = system_unix_seconds()?;
        let observed = renewal.map_or(now, |renewal| renewal.observed_seconds(requested_at));
        let expires = observed
            .checked_add(CATALOG_TTL_SECONDS)
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?
            .min(COMPATIBILITY_EXPIRES_UNIX);
        if expires <= now {
            return Err(XaiNativeAuthError::CompatibilityInactive);
        }
        let public = XaiNativeCatalogReceipt {
            entries: admitted_entries,
            account: fence.account_binding().clone(),
            observed_at: Timestamp::new(observed, 0)?,
            expires_at: Timestamp::new(expires, 0)?,
        };
        let receipt = CatalogReceiptCore {
            public: public.clone(),
            fence: fence.clone(),
            selection_epoch: stored.selection_epoch.clone(),
            authority_id: route_credential_authority_id(&self.store, self.source_scope.as_deref()),
        };
        *self
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)? = Some(receipt.clone());
        drop(lease);
        Ok(receipt)
    }

    pub(super) fn acquire_resolution_lease(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<ReadyCredentialLease, XaiNativeAuthError> {
        self.ensure_resolution_receipt_current(receipt)?;
        let current = self.store.load()?.ok_or(XaiNativeAuthError::NotLoggedIn)?;
        if current != receipt.fence || current.status() != CredentialStatus::Ready {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let lease = self
            .store
            .acquire_ready(&receipt.fence)
            .map_err(|_| XaiNativeAuthError::SelectionChanged)?;
        if lease.fence() != receipt.fence {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(receipt.fence.account_binding())?;
        if stored.selection_epoch != receipt.selection_epoch
            || receipt.authority_id
                != route_credential_authority_id(&self.store, self.source_scope.as_deref())
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        Ok(lease)
    }

    pub(super) fn ensure_resolution_receipt_current(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<(), XaiNativeAuthError> {
        self.ensure_active()?;
        let now = system_unix_seconds()?;
        if !valid_admitted_catalog_entries(&receipt.public.entries)
            || receipt.fence.status() != CredentialStatus::Ready
            || receipt.fence.profile_id().as_str() != XAI_GROK_COMPATIBILITY_PROFILE
            || receipt.public.account != *receipt.fence.account_binding()
            || receipt.public.observed_at.seconds() > now
            || receipt.public.expires_at.seconds() <= now
            || receipt.public.expires_at.seconds() > COMPATIBILITY_EXPIRES_UNIX
            || !valid_login_selection_epoch(&receipt.selection_epoch)
            || receipt.authority_id
                != route_credential_authority_id(&self.store, self.source_scope.as_deref())
        {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn pause_resolution_after_catalog(&self) -> Result<(), XaiNativeAuthError> {
        let pause = self
            .resolution_pause
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
            .clone();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.resume.notified().await;
        }
        Ok(())
    }

    pub(super) fn active_catalog_receipt(
        &self,
        fence: &CredentialFence,
        stored: &StoredTokens,
        now: u64,
    ) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        let receipt = self
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?
            .clone()
            .ok_or(XaiNativeAuthError::CatalogUnavailable)?;
        if !valid_admitted_catalog_entries(&receipt.public.entries)
            || receipt.public.account != *fence.account_binding()
            || receipt.fence != *fence
            || receipt.selection_epoch != stored.selection_epoch
            || receipt.authority_id
                != route_credential_authority_id(&self.store, self.source_scope.as_deref())
            || receipt.public.observed_at.seconds() > now
            || receipt.public.expires_at.seconds() <= now
            || receipt.public.expires_at.seconds() > COMPATIBILITY_EXPIRES_UNIX
        {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(receipt)
    }

    pub(super) fn persist_login(
        &self,
        baseline: Option<&CredentialFence>,
        stored: &StoredTokens,
        flow_deadline: Option<Instant>,
    ) -> Result<XaiSubscriptionStatus, XaiNativeAuthError> {
        self.ensure_login_validation_active(flow_deadline)?;
        let account = AccountBinding::new(stored.subject.clone())?;
        let generation = match baseline {
            Some(fence) => fence.generation().next()?,
            None => CredentialGeneration::new(1)?,
        };
        let record = credential_record(account.clone(), generation, stored)?;
        self.ensure_login_validation_active(flow_deadline)?;
        let fence = if let Some(expected) = baseline {
            self.core_replace_selected(expected, record)
                .map_err(|_| XaiNativeAuthError::LoginPersistence)?
        } else {
            self.store
                .initialize_selected(&record)
                .map_err(|_| XaiNativeAuthError::LoginPersistence)?;
            record.fence()
        };
        Ok(XaiSubscriptionStatus {
            generation: fence.generation(),
            status: fence.status(),
            account,
        })
    }

    fn core_replace_selected(
        &self,
        expected: &CredentialFence,
        record: CredentialRecord,
    ) -> Result<CredentialFence, XaiNativeAuthError> {
        self.store
            .replace_selected_login(expected, record)
            .map_err(Into::into)
    }

    pub(super) async fn refresh_if_needed(
        &self,
        expected_account: Option<&AccountBinding>,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
    ) -> Result<(), XaiNativeAuthError> {
        self.refresh_with_force(
            expected_account,
            expected_selection_epoch,
            expected_authority,
            false,
        )
        .await
    }

    /// Runs the claimed refresh ladder; `force` claims a new generation even
    /// when the stored expiry is not yet inside the refresh lead (the provider
    /// rejected the access token with HTTP 401).
    pub(super) async fn refresh_with_force(
        &self,
        expected_account: Option<&AccountBinding>,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
        force: bool,
    ) -> Result<(), XaiNativeAuthError> {
        let flight = self.refresh_flight.lock().await;
        let mut hooks = XaiRefreshHooks {
            cancel_guard: XaiRefreshCancelGuard::new(&self.store, self.refresh_observer.as_deref()),
            core: self,
            expected_account,
            expected_selection_epoch,
            expected_authority,
            _flight: flight,
            request_permit: None,
            started: Instant::now(),
            force,
            detail: CredentialRefreshDetail::default(),
        };
        let result = run_claimed_refresh(&self.store, &self.client, &mut hooks).await;
        hooks.cancel_guard.finished();
        result
    }

    pub(super) fn validate_expected_selection(
        &self,
        fence: &CredentialFence,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
    ) -> Result<(), XaiNativeAuthError> {
        let Some(expected_epoch) = expected_selection_epoch else {
            return Ok(());
        };
        let stored = self.read_stored(fence)?;
        if stored.selection_epoch != expected_epoch
            || expected_authority
                != Some(
                    route_credential_authority_id(&self.store, self.source_scope.as_deref())
                        .as_str(),
                )
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        Ok(())
    }

    pub(super) fn ensure_route_current(
        &self,
        expected_receipt: &CatalogReceiptCore,
        witness_failures: bool,
    ) -> Result<(), XaiNativeAuthError> {
        self.ensure_active().inspect_err(|_| {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.catalog");
            }
        })?;
        let fence = self
            .store
            .load()
            .inspect_err(|_| {
                if witness_failures {
                    crate::route::log_authority_rejection("native.route_current.fence_read");
                }
            })?
            .ok_or_else(|| {
                if witness_failures {
                    crate::route::log_authority_rejection("native.route_current.fence_missing");
                }
                XaiNativeAuthError::NotLoggedIn
            })?;
        if fence.status() != CredentialStatus::Ready {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.fence_status");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence.generation() != expected_receipt.fence.generation() {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.generation");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence.account_binding() != &expected_receipt.public.account {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.account");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence != expected_receipt.fence {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.fence_binding");
            }
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let stored = self.read_stored(&expected_receipt.fence).inspect_err(|_| {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.stored_selection");
            }
        })?;
        self.validate_expected_selection(
            &fence,
            Some(&expected_receipt.selection_epoch),
            Some(&expected_receipt.authority_id),
        )
        .inspect_err(|_| {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.stored_selection");
            }
        })?;
        let now = system_unix_seconds().inspect_err(|_| {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.clock");
            }
        })?;
        let active = self
            .active_catalog_receipt(&fence, &stored, now)
            .inspect_err(|_| {
                if witness_failures {
                    crate::route::log_authority_rejection("native.route_current.catalog");
                }
            })?;
        if active != *expected_receipt {
            if witness_failures {
                crate::route::log_authority_rejection("native.route_current.catalog");
            }
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    pub(super) fn ensure_route_receipt_current_while_leased(
        &self,
        expected_receipt: &CatalogReceiptCore,
    ) -> Result<(), XaiNativeAuthError> {
        // The caller retains the exact ReadyCredentialLease acquired for the
        // authorization header. Reopening the selected-slot file here would
        // recursively contend on that same inode and is unnecessary: the
        // shared lease itself fences relogin/rotation. Revalidate the complete
        // receipt identity and its live time bounds under the catalog lock.
        self.ensure_resolution_receipt_current(expected_receipt)?;
        let catalog = self
            .catalog
            .lock()
            .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?;
        if catalog.as_ref() != Some(expected_receipt) {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    pub(super) fn needs_refresh(
        &self,
        fence: &CredentialFence,
    ) -> Result<bool, XaiNativeAuthError> {
        if fence.status() != CredentialStatus::Ready {
            return Ok(true);
        }
        let stored = self.read_stored(fence)?;
        let now = system_unix_seconds()?;
        Ok(access_needs_refresh(
            stored.expires_at,
            now,
            ACCESS_REFRESH_LEAD_SECONDS,
        ))
    }

    pub(super) fn read_stored(
        &self,
        fence: &CredentialFence,
    ) -> Result<StoredTokens, XaiNativeAuthError> {
        let lease = self.store.acquire_ready(fence)?;
        let stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(fence.account_binding())?;
        Ok(stored)
    }

    pub(super) fn acquire_grant(
        &self,
        expected_receipt: &CatalogReceiptCore,
    ) -> Result<crate::XaiSubscriptionCredentialGrant, crate::XaiAuthError> {
        let fence = self
            .store
            .load()
            .map_err(|_| crate::XaiAuthError::CredentialSource)?
            .ok_or(crate::XaiAuthError::MissingCredential)?;
        if fence != expected_receipt.fence
            || fence.account_binding() != &expected_receipt.public.account
            || fence.status() != CredentialStatus::Ready
        {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let lease = self
            .store
            .acquire_ready(&expected_receipt.fence)
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        if lease.fence() != expected_receipt.fence {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let mut stored: StoredTokens = serde_json::from_slice(lease.payload().expose_secret())
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        stored
            .validate(&expected_receipt.public.account)
            .map_err(|_| crate::XaiAuthError::CredentialSource)?;
        if stored.selection_epoch != expected_receipt.selection_epoch
            || route_credential_authority_id(&self.store, self.source_scope.as_deref())
                != expected_receipt.authority_id
        {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let now = system_unix_seconds().map_err(|_| crate::XaiAuthError::CredentialSource)?;
        let active = self
            .active_catalog_receipt(&fence, &stored, now)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)?;
        if active != *expected_receipt {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        if access_needs_refresh(stored.expires_at, now, ACCESS_REFRESH_LEAD_SECONDS) {
            return Err(crate::XaiAuthError::MissingCredential);
        }
        let access = SecretString::from(std::mem::take(&mut stored.access_token));
        Ok(crate::XaiSubscriptionCredentialGrant::native(
            &access, lease,
        ))
    }

    pub(super) async fn revoke(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        self.ensure_active()?;
        self.revoke_current().await
    }

    #[cfg(test)]
    pub(super) async fn revoke_at(
        &self,
        now: u64,
    ) -> Result<CredentialGeneration, XaiNativeAuthError> {
        ensure_compatibility(now)?;
        self.revoke_current().await
    }

    async fn revoke_current(&self) -> Result<CredentialGeneration, XaiNativeAuthError> {
        let flight = self.refresh_flight.lock().await;
        let mut hooks = XaiRevokeHooks {
            core: self,
            _flight: flight,
        };
        run_claimed_revoke(&self.store, &self.client, &mut hooks).await
    }

    pub(super) async fn send_form(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let (status, response) = self.send_form_raw(endpoint, body).await?;
        if !status.is_success() {
            return Err(status_error(status));
        }
        Ok(response)
    }

    async fn send_login_token_form(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let (status, response) = self.send_login_token_form_raw(endpoint, body).await?;
        if !status.is_success() {
            return Err(XaiNativeAuthError::LoginTokenExchange);
        }
        Ok(response)
    }

    pub(super) async fn send_login_token_form_raw(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<(StatusCode, Zeroizing<Vec<u8>>), XaiNativeAuthError> {
        let request = self
            .form_request(endpoint, body)
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        let (status, _is_json, response) = self
            .execute(request)
            .await
            .map_err(|_| XaiNativeAuthError::LoginTokenExchange)?;
        // Token responses: media-type is optional for provider compatibility;
        // body JSON is still fail-closed via decode.
        Ok((status, response))
    }

    async fn send_form_raw(
        &self,
        endpoint: &Endpoint,
        body: Zeroizing<String>,
    ) -> Result<(StatusCode, Zeroizing<Vec<u8>>), XaiNativeAuthError> {
        let request = self.form_request(endpoint, body)?;
        let (status, _, response) = self.execute(request).await?;
        Ok((status, response))
    }

    pub(super) fn form_request(
        &self,
        endpoint: &Endpoint,
        mut body: Zeroizing<String>,
    ) -> Result<Request, XaiNativeAuthError> {
        let mut headers = fixed_headers("application/x-www-form-urlencoded");
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        Request::new(
            Origin::parse(&endpoint.origin)?,
            Method::POST,
            &endpoint.path,
            headers,
            SensitiveBytes::new(std::mem::take(&mut *body).into_bytes()).into_bytes(),
            self.client.limits(),
        )
        .map_err(Into::into)
    }

    pub(super) async fn get(
        &self,
        endpoint: &Endpoint,
        bearer_token: Option<&str>,
        catalog_subject: Option<&AccountBinding>,
        retry: bool,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let mut headers = fixed_headers("application/json");
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(token) = bearer_token {
            headers.insert(HEADER_AUTHORIZATION, bearer_header(token)?);
        }
        if endpoint.path == CATALOG_PATH {
            insert_subscription_catalog_headers(
                &mut headers,
                catalog_subject.ok_or(XaiNativeAuthError::CatalogUnavailable)?,
            )?;
        } else if catalog_subject.is_some() {
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        let request = Request::new(
            Origin::parse(&endpoint.origin)?,
            Method::GET,
            &endpoint.path,
            headers,
            Bytes::new(),
            self.client.limits(),
        )?;
        let (status, is_json, response) = if endpoint.path == CATALOG_PATH {
            let client = if let Some(observer) = &self.catalog_observer {
                self.client
                    .clone()
                    .with_observer(Some(Arc::clone(observer)))
            } else {
                self.client.clone()
            };
            let mut dispatch = plakous_auth::subscription_limit::CatalogRequestDispatch::new(
                self.request_gate.clone(),
            );
            let reply = plakous_exchange::lookup::get_with_dispatch(
                &client,
                request,
                if retry {
                    plakous_exchange::lookup::LookupBudget::from_deadline(
                        Instant::now() + REQUEST_TIMEOUT,
                    )
                } else {
                    plakous_exchange::lookup::LookupBudget::single_attempt(
                        Instant::now() + REQUEST_TIMEOUT,
                    )
                },
                MAX_AUTH_BODY_BYTES,
                &mut |failure| {
                    dispatch
                        .observe(failure.map(Failure::transmission))
                        .map_err(|_| plakous_exchange::lookup::LookupError::DispatchDenied)
                },
            )
            .await
            .map_err(|error| match error {
                plakous_exchange::lookup::LookupError::Transport(failure) => {
                    XaiNativeAuthError::Transport(network_failure(failure))
                }
                plakous_exchange::lookup::LookupError::Exhausted { .. } => {
                    XaiNativeAuthError::Lookup(error)
                }
                _ => XaiNativeAuthError::CatalogUnavailable,
            })?;
            (reply.status, headers_are_json(&reply.headers), reply.body)
        } else {
            self.execute(request).await?
        };
        if endpoint.path == CATALOG_PATH
            && let Some(reason) = super::dispatch::version_gate(status.as_u16(), &response)
        {
            return Err(XaiNativeAuthError::CompatibilityDrift(reason));
        }
        if status != StatusCode::OK {
            return Err(status_error(status));
        }
        if !is_json {
            return Err(XaiNativeAuthError::Transport(None));
        }
        Ok(response)
    }

    pub(super) async fn get_login_json(
        &self,
        endpoint: &Endpoint,
        bearer_token: Option<&str>,
        stage: LoginHttpStage,
    ) -> Result<Zeroizing<Vec<u8>>, XaiNativeAuthError> {
        let mut headers = fixed_headers("application/json");
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(token) = bearer_token {
            headers.insert(
                HEADER_AUTHORIZATION,
                bearer_header(token).map_err(|_| stage.error())?,
            );
        }
        let origin = Origin::parse(&endpoint.origin).map_err(|_| stage.error())?;
        let request = Request::new(
            origin,
            Method::GET,
            &endpoint.path,
            headers,
            Bytes::new(),
            self.client.limits(),
        )
        .map_err(|_| stage.error())?;
        let (status, is_json, response) = self.execute(request).await.map_err(|_| stage.error())?;
        if !status.is_success() || !is_json {
            return Err(stage.error());
        }
        Ok(response)
    }

    pub(super) async fn execute(
        &self,
        request: Request,
    ) -> Result<(StatusCode, bool, Zeroizing<Vec<u8>>), AuthTransportError> {
        let control = ExchangeControl::new(Instant::now() + REQUEST_TIMEOUT);
        let response = self
            .client
            .execute(request, control)
            .await
            .map_err(AuthTransportError::Failure)?;
        let status = response.status();
        let is_json = headers_are_json(response.headers());
        let bytes = pull_auth_body(response.into_body(), MAX_AUTH_BODY_BYTES).await?;
        Ok((status, is_json, bytes))
    }
}

/// A renewal's catalog fetch: the exact selection the refresh and the fetch
/// must still name, and the floor of its callers' check times.
///
/// A renewed receipt answers revalidations that read their clock before the
/// refresh and catalog round trip, so it claims observation at the request
/// start or at the earliest waiting `checked_at`, whichever is earlier, in
/// whole seconds. Its expiry runs from that stamp, so the window is never
/// extended; credential freshness stays bounded by the stored access expiry.
pub(super) struct CatalogRenewal<'a> {
    pub(super) account: &'a AccountBinding,
    pub(super) selection_epoch: &'a str,
    pub(super) authority_id: &'a str,
    pub(super) floor: &'a crate::XaiRenewalFloor,
}

impl CatalogRenewal<'_> {
    fn observed_seconds(&self, requested_at: u64) -> u64 {
        self.floor
            .earliest()
            .map_or(requested_at, |floor| floor.seconds().min(requested_at))
    }
}
