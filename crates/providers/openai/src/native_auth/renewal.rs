//! Route fingerprints and catalog receipt renewal.

use super::{
    AccountBinding, Arc, CodexCatalogCapacityFacts, CodexCatalogSource, CodexCatalogToolMode,
    CodexReasoningEffortSet, CodexSubscriptionCredentialManager, CredentialFence, CredentialStatus,
    Digest, DigestDomain, Duration, Instant, Mutex, NativeCodexAuthError, NonZeroU32,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OpenAiRoute, OpenAiTarget, RefreshForce,
    ResolvedModelDescriptor, RuntimeModelAuthorityError, StorageMode, Timestamp,
    WitnessedDescriptor,
};

fn storage_binding(storage: StorageMode) -> &'static [u8] {
    match storage {
        StorageMode::ProviderStored => b"provider-stored",
        StorageMode::Stateless => b"stateless",
    }
}

/// One-way digest of the stored login selection epoch. A refreshed generation
/// keeps it; any new login, even of the same account, changes it.
pub(super) fn credential_selection_epoch(
    account_fingerprint: Digest,
    credential_authority_binding: Digest,
    login_selection_epoch: &str,
) -> Result<Digest, NativeCodexAuthError> {
    let account = account_fingerprint.identity_bytes();
    let authority = credential_authority_binding.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        2,
        &[
            b"plak-native-codex-credential-selection-epoch-v2",
            OPENAI_CODEX_COMPATIBILITY_PROFILE_ID.as_bytes(),
            &account,
            &authority,
            login_selection_epoch.as_bytes(),
        ],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Hash each independent Codex route, account, capacity, effort, catalog and wire-mode fact into the resolution identity."
)]
pub(super) fn resolution_route_fingerprint(
    route: &OpenAiRoute,
    descriptor: &ResolvedModelDescriptor,
    account_fingerprint: Digest,
    credential_authority_binding: Digest,
    capacity_facts: CodexCatalogCapacityFacts,
    reasoning_efforts: CodexReasoningEffortSet,
    catalog_enum_fingerprint: Digest,
    responses_lite: bool,
) -> Result<Digest, NativeCodexAuthError> {
    let account = account_fingerprint.identity_bytes();
    let authority = credential_authority_binding.identity_bytes();
    let has_context_window = [u8::from(capacity_facts.context_window().is_some())];
    let context_window = capacity_facts
        .context_window()
        .map_or(0, NonZeroU32::get)
        .to_be_bytes();
    let has_max_context_window = [u8::from(capacity_facts.max_context_window().is_some())];
    let max_context_window = capacity_facts
        .max_context_window()
        .map_or(0, NonZeroU32::get)
        .to_be_bytes();
    let effective_context_window_percent =
        [capacity_facts.effective_context_window_percent().get()];
    let has_auto_compact_token_limit = [u8::from(
        capacity_facts.auto_compact_token_limit().is_some(),
    )];
    let auto_compact_token_limit = capacity_facts
        .auto_compact_token_limit()
        .map_or(0, NonZeroU32::get)
        .to_be_bytes();
    let reasoning_efforts = [reasoning_efforts.identity_byte()];
    let catalog_enum_fingerprint = catalog_enum_fingerprint.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        4,
        &[
            b"plak-native-codex-resolution-route-v4",
            route.id().as_str().as_bytes(),
            route.origin().as_str().as_bytes(),
            route.responses_path().as_bytes(),
            route.dialect().protocol().as_str().as_bytes(),
            route.dialect().revision().as_bytes(),
            storage_binding(route.storage()),
            &account,
            &authority,
            descriptor.requested_model().as_str().as_bytes(),
            descriptor.profile().as_str().as_bytes(),
            crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION.as_bytes(),
            crate::config::OPENAI_CODEX_CATALOG_SOURCE_REVISION.as_bytes(),
            &has_context_window,
            &context_window,
            &has_max_context_window,
            &max_context_window,
            &effective_context_window_percent,
            &has_auto_compact_token_limit,
            &auto_compact_token_limit,
            &reasoning_efforts,
            &catalog_enum_fingerprint,
            &[u8::from(responses_lite)],
        ],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

/// The exact login selection a receipt renewal may refresh and fetch under.
#[derive(Clone, Copy)]
pub(super) struct ExpectedSelection<'a> {
    pub(super) account: &'a AccountBinding,
    pub(super) selection_epoch: Digest,
}

impl ExpectedSelection<'_> {
    pub(super) fn check(
        &self,
        account: &AccountBinding,
        selection_epoch: Digest,
    ) -> Result<(), NativeCodexAuthError> {
        if account != self.account {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        if selection_epoch != self.selection_epoch {
            return Err(NativeCodexAuthError::SelectionChanged);
        }
        Ok(())
    }
}

/// A catalog fetch that renews one receipt: bound to its login selection, and
/// stamped no later than the earliest check it answers (see [`CodexRenewalFloor`]).
pub(super) struct CatalogRenewal<'a> {
    pub(super) selection: ExpectedSelection<'a>,
    pub(super) floor: &'a CodexRenewalFloor,
}

impl CatalogRenewal<'_> {
    pub(super) fn check(
        &self,
        account: &AccountBinding,
        selection_epoch: Digest,
    ) -> Result<(), NativeCodexAuthError> {
        self.selection.check(account, selection_epoch)
    }

    /// Callers read their check time before the refresh and catalog round
    /// trip, so the renewed receipt claims observation at the request start or
    /// the earliest waiting check, whichever is earlier, in whole seconds.
    pub(super) fn observed_at(
        &self,
        requested_at: Timestamp,
    ) -> Result<Timestamp, NativeCodexAuthError> {
        let earliest = self
            .floor
            .earliest()
            .map_or(requested_at, |floor| floor.min(requested_at));
        Timestamp::new(earliest.seconds(), 0).map_err(|_| NativeCodexAuthError::Catalog)
    }
}

/// The earliest `checked_at` among the revalidations one renewal answers.
#[derive(Debug, Default)]
pub(crate) struct CodexRenewalFloor(Mutex<Option<Timestamp>>);

impl CodexRenewalFloor {
    pub(crate) fn note(&self, checked_at: Timestamp) {
        if let Ok(mut floor) = self.0.lock() {
            *floor = Some(floor.map_or(checked_at, |earliest| earliest.min(checked_at)));
        }
    }

    fn earliest(&self) -> Option<Timestamp> {
        self.0.lock().ok().and_then(|floor| *floor)
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut floor) = self.0.lock() {
            *floor = None;
        }
    }
}

/// The behavior-bearing facts of one admitted catalog row. A renewed receipt
/// must carry the same row, or it installs nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CodexRowSemantics {
    pub(super) descriptor: Digest,
    pub(super) capacity_facts: CodexCatalogCapacityFacts,
    pub(super) reasoning_efforts: CodexReasoningEffortSet,
    pub(super) tool_mode: Option<CodexCatalogToolMode>,
    pub(super) responses_lite: bool,
    pub(super) content_filter_guidance: Option<String>,
    pub(super) catalog_enum_fingerprint: Digest,
    pub(super) catalog_receipt: Digest,
}

impl CodexRowSemantics {
    pub(super) fn of(issued: &WitnessedDescriptor) -> Result<Self, NativeCodexAuthError> {
        Ok(Self {
            descriptor: issued
                .descriptor
                .semantic_binding_digest()
                .map_err(|_| NativeCodexAuthError::Catalog)?,
            capacity_facts: issued.capacity_facts,
            reasoning_efforts: issued.reasoning_efforts,
            tool_mode: issued.tool_mode,
            responses_lite: issued.responses_lite,
            content_filter_guidance: issued.content_filter_guidance.clone(),
            catalog_enum_fingerprint: issued.catalog_enum_fingerprint,
            catalog_receipt: issued.catalog_receipt,
        })
    }
}

/// What a running Codex model needs to renew its receipt: the shared catalog
/// source and the exact admitted row.
#[derive(Clone)]
pub(crate) struct CodexReceiptRenewal {
    pub(super) source: CodexCatalogSource,
    pub(super) model_id: String,
    pub(super) semantics: CodexRowSemantics,
}

impl CodexReceiptRenewal {
    pub(crate) async fn ready_selection(
        &self,
        stale: &OpenAiTarget,
    ) -> Result<Digest, RuntimeModelAuthorityError> {
        let expected = stale
            .native_codex_credential_selection()
            .ok_or(RuntimeModelAuthorityError::Invalid)?
            .0
            .clone();
        let renewal = self.clone();
        let fence = tokio::task::spawn_blocking(move || renewal.read_ready_fence(&expected))
            .await
            .map_err(|_| RuntimeModelAuthorityError::Unavailable)??;
        let lease = self
            .source
            .manager
            .acquire_ready_lease(fence)
            .await
            .map_err(renewal_error)?;
        let stored = super::parse_stored(lease.payload()).map_err(renewal_error)?;
        credential_selection_epoch(
            stale.account_binding(),
            stale.credential_authority_binding(),
            &stored.selection_epoch,
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }

    fn read_ready_fence(
        &self,
        expected: &CredentialFence,
    ) -> Result<CredentialFence, RuntimeModelAuthorityError> {
        if self
            .source
            .manager
            .selected_account()
            .map_err(renewal_error)?
            != *expected.account_binding()
        {
            return Err(RuntimeModelAuthorityError::AccountChanged);
        }
        let fence = self
            .source
            .manager
            .store
            .load()
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?
            .ok_or(RuntimeModelAuthorityError::AwaitingCredential)?;
        if fence.status() != CredentialStatus::Ready {
            if matches!(
                self.source
                    .manager
                    .store
                    .offline_lifecycle()
                    .map_err(|_| RuntimeModelAuthorityError::Invalid)?,
                plakous_auth::OfflineCredentialLifecycle::Refreshing
                    | plakous_auth::OfflineCredentialLifecycle::Ready
            ) {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            return Err(RuntimeModelAuthorityError::AwaitingCredential);
        }
        if fence.account_binding() != expected.account_binding() {
            return Err(RuntimeModelAuthorityError::AccountChanged);
        }
        if fence.generation() < expected.generation() {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        Ok(fence)
    }

    pub(crate) async fn ensure_credential_ready(
        &self,
        stale: &OpenAiTarget,
    ) -> Result<(), RuntimeModelAuthorityError> {
        let current = self.ready_selection(stale).await?;
        let expected = stale
            .native_codex_credential_selection()
            .ok_or(RuntimeModelAuthorityError::Invalid)?
            .1;
        if current != expected {
            return Err(RuntimeModelAuthorityError::AwaitingCredential);
        }
        Ok(())
    }

    /// Builds a new receipt for `stale`'s admitted descriptor: one claimed
    /// refresh when due and one authenticated catalog fetch, both bound to the
    /// stale receipt's account and login selection; installs nothing unless
    /// route identity and the admitted row are unchanged and the generation
    /// did not decrease. The new receipt never outlives `admitted_until`.
    pub(crate) async fn renew(
        &self,
        stale: &OpenAiTarget,
        admitted_until: Timestamp,
        floor: &CodexRenewalFloor,
    ) -> Result<OpenAiTarget, RuntimeModelAuthorityError> {
        let selection = stale
            .native_codex_credential_selection()
            .ok_or(RuntimeModelAuthorityError::Invalid)?
            .1;
        self.renew_selection(stale, admitted_until, floor, selection)
            .await
    }

    pub(crate) async fn renew_selection(
        &self,
        stale: &OpenAiTarget,
        admitted_until: Timestamp,
        floor: &CodexRenewalFloor,
        selection_epoch: Digest,
    ) -> Result<OpenAiTarget, RuntimeModelAuthorityError> {
        let source = &self.source;
        let expected_fence = stale
            .native_codex_credential_selection()
            .map(|(fence, _)| fence.clone())
            .ok_or(RuntimeModelAuthorityError::Invalid)?;
        if stale.validate_route(&source.route).is_err()
            || stale.account_binding() != source.manager.route_account_digest()
            || stale.credential_authority_binding() != source.manager.authority_binding
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        refresh_detached(
            Arc::clone(&source.manager),
            expected_fence.account_binding().clone(),
            selection_epoch,
        )
        .await
        .map_err(renewal_error)?;
        let renewal = CatalogRenewal {
            selection: ExpectedSelection {
                account: expected_fence.account_binding(),
                selection_epoch,
            },
            floor,
        };
        let resolved = source
            .fetch_catalog(Some(&renewal))
            .await
            .map_err(renewal_error)?;
        validate_catalog_fence(&source.manager, &resolved.credential_fence)
            .map_err(renewal_error)?;
        if resolved.credential_fence.account_binding() != expected_fence.account_binding()
            || resolved.credential_fence.generation() < expected_fence.generation()
            || resolved.ambiguous_slugs.contains(&self.model_id)
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        let issued = resolved
            .descriptors
            .get(&self.model_id)
            .ok_or(RuntimeModelAuthorityError::ForeignAuthority)?;
        if issued.witness.credential_selection_epoch() != selection_epoch
            || CodexRowSemantics::of(issued).map_err(|_| RuntimeModelAuthorityError::Invalid)?
                != self.semantics
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority);
        }
        let witness = source
            .manager
            .issue_catalog_witness(
                stale.descriptor(),
                &resolved.credential_fence,
                selection_epoch,
                issued.observed_at,
                issued.expires_at.min(admitted_until),
                issued.responses_lite,
            )
            .map_err(|_| RuntimeModelAuthorityError::ForeignAuthority)?;
        OpenAiTarget::new_native_catalog_target(
            source.route.clone(),
            stale.descriptor().clone(),
            witness,
            Arc::clone(&source.clock),
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }
}

/// Runs a renewal's claimed refresh (when due) as its own task, bound to one
/// login selection. Revalidation callers may be dropped (a compaction timeout,
/// shutdown, an aborted driver); the claimed exchange still completes or fails
/// within its 30 s refresh deadline and records its outcome, so dropping a
/// waiting caller never strands a started claim as an ambiguous quarantine.
async fn refresh_detached(
    manager: Arc<CodexSubscriptionCredentialManager>,
    account: AccountBinding,
    selection_epoch: Digest,
) -> Result<(), NativeCodexAuthError> {
    tokio::spawn(async move {
        let _lifecycle = manager.lifecycle.lock().await;
        manager
            .refresh_locked_with_force(
                Instant::now() + Duration::from_secs(30),
                None,
                RefreshForce::IfDue,
                Some(ExpectedSelection {
                    account: &account,
                    selection_epoch,
                }),
            )
            .await
    })
    .await
    .map_err(|_| NativeCodexAuthError::Coordination)?
}

fn renewal_error(error: NativeCodexAuthError) -> RuntimeModelAuthorityError {
    match error {
        NativeCodexAuthError::AccountChanged => RuntimeModelAuthorityError::AccountChanged,
        NativeCodexAuthError::SelectionChanged => RuntimeModelAuthorityError::AwaitingCredential,
        NativeCodexAuthError::LoginRequired
        | NativeCodexAuthError::ProfileChanged
        | NativeCodexAuthError::WrittenByNewerBuild => RuntimeModelAuthorityError::ForeignAuthority,
        NativeCodexAuthError::Unavailable => RuntimeModelAuthorityError::Unavailable,
        _ => RuntimeModelAuthorityError::Stale,
    }
}

pub(super) fn validate_catalog_fence(
    manager: &CodexSubscriptionCredentialManager,
    expected: &CredentialFence,
) -> Result<(), NativeCodexAuthError> {
    let current = manager
        .store
        .load()
        .map_err(|_| NativeCodexAuthError::Store)?
        .ok_or(NativeCodexAuthError::Unavailable)?;
    if current.status() != CredentialStatus::Ready {
        return Err(NativeCodexAuthError::Unavailable);
    }
    if current.account_binding() != &manager.selected_account()? {
        return Err(NativeCodexAuthError::AccountChanged);
    }
    if current != *expected {
        return Err(NativeCodexAuthError::Catalog);
    }
    Ok(())
}
