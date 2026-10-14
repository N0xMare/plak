use super::{
    AtomicWriteStage, Deserialize, LOGIN_RECOVERY_QUARANTINE, LOGIN_RECOVERY_SCHEMA,
    LOGIN_RECOVERY_VERSION, LoginRecoveryEpoch, LoginRecoveryMarker, LoginRecoveryOperation,
    LoginRecoveryState, MAX_CREDENTIAL_RECORD_BYTES, NativeAuthStatus, NativeProvider,
    NativeSubscriptionError, Path, SubscriptionPaths, clear_quarantine, new_random_identity,
    process_quarantine_key, quarantine, quarantined, read_private_bounded,
    read_private_bounded_with_hook_and_limit, remove_private_file_durable, valid_generation,
    valid_nonce, write_private_atomic, write_private_atomic_with_hook,
};

impl LoginRecoveryMarker {
    fn armed(operation_nonce: String, consent_generation: String) -> Self {
        Self {
            schema: LOGIN_RECOVERY_SCHEMA.to_owned(),
            version: LOGIN_RECOVERY_VERSION,
            state: LoginRecoveryState::Armed,
            operation_nonce,
            consent_generation,
            credential_generation: None,
        }
    }

    pub(super) fn cleared(&self, credential_generation: u64) -> Self {
        Self {
            schema: self.schema.clone(),
            version: self.version,
            state: LoginRecoveryState::Cleared,
            operation_nonce: self.operation_nonce.clone(),
            consent_generation: self.consent_generation.clone(),
            credential_generation: Some(credential_generation),
        }
    }

    fn validate(&self) -> Result<(), NativeSubscriptionError> {
        let generation_shape = match self.state {
            LoginRecoveryState::Armed => self.credential_generation.is_none(),
            LoginRecoveryState::Cleared => {
                self.credential_generation.is_some_and(|value| value > 0)
            }
        };
        if self.schema != LOGIN_RECOVERY_SCHEMA
            || self.version != LOGIN_RECOVERY_VERSION
            || !valid_nonce(&self.operation_nonce)
            || !valid_generation(&self.consent_generation)
            || !generation_shape
        {
            return Err(NativeSubscriptionError::InvalidReceipt);
        }
        Ok(())
    }

    pub(super) fn to_bytes(&self) -> Result<Vec<u8>, NativeSubscriptionError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self).map_err(|_| NativeSubscriptionError::Internal)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

impl LoginRecoveryEpoch {
    fn from_marker(marker: &LoginRecoveryMarker) -> Self {
        Self {
            schema: LOGIN_RECOVERY_SCHEMA.to_owned(),
            version: LOGIN_RECOVERY_VERSION,
            state: marker.state,
            operation_nonce: marker.operation_nonce.clone(),
            consent_generation: marker.consent_generation.clone(),
            credential_generation: marker.credential_generation,
        }
    }

    fn validate(&self) -> Result<(), NativeSubscriptionError> {
        if self.schema != LOGIN_RECOVERY_SCHEMA
            || self.version != LOGIN_RECOVERY_VERSION
            || !valid_nonce(&self.operation_nonce)
            || !valid_generation(&self.consent_generation)
            || match self.state {
                LoginRecoveryState::Armed => self.credential_generation.is_some(),
                LoginRecoveryState::Cleared => {
                    self.credential_generation.is_none_or(|value| value == 0)
                }
            }
        {
            return Err(NativeSubscriptionError::InvalidReceipt);
        }
        Ok(())
    }

    fn matches_marker(&self, marker: &LoginRecoveryMarker) -> bool {
        self.state == marker.state
            && self.operation_nonce == marker.operation_nonce
            && self.consent_generation == marker.consent_generation
            && self.credential_generation == marker.credential_generation
    }

    pub(super) fn to_bytes(&self) -> Result<Vec<u8>, NativeSubscriptionError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec(self).map_err(|_| NativeSubscriptionError::Internal)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

pub(super) fn read_login_recovery_marker(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<Option<(LoginRecoveryMarker, Vec<u8>)>, NativeSubscriptionError> {
    match read_private_bounded(&paths.login_recovery_path(provider)) {
        Ok(bytes) => {
            let marker: LoginRecoveryMarker = serde_json::from_slice(&bytes)
                .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
            marker.validate()?;
            Ok(Some((marker, bytes)))
        }
        Err(NativeSubscriptionError::AcceptanceRequired) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn read_login_recovery_epoch(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<Option<(LoginRecoveryEpoch, Vec<u8>)>, NativeSubscriptionError> {
    match read_private_bounded(&paths.login_recovery_epoch_path(provider)) {
        Ok(bytes) => {
            let epoch: LoginRecoveryEpoch = serde_json::from_slice(&bytes)
                .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
            epoch.validate()?;
            Ok(Some((epoch, bytes)))
        }
        Err(NativeSubscriptionError::AcceptanceRequired) => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Deserialize)]
struct CredentialGenerationProbe {
    generation: u64,
}

pub(super) fn current_credential_generation(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<Option<u64>, NativeSubscriptionError> {
    let path = paths.provider_record(provider);
    let mut bytes =
        match read_private_bounded_with_hook_and_limit(&path, MAX_CREDENTIAL_RECORD_BYTES, || {}) {
            Ok(bytes) => bytes,
            Err(NativeSubscriptionError::AcceptanceRequired) => return Ok(None),
            Err(error) => return Err(error),
        };
    let result = serde_json::from_slice::<CredentialGenerationProbe>(&bytes)
        .map_err(|_| NativeSubscriptionError::CredentialUnavailable)
        .and_then(|probe| {
            if probe.generation == 0 {
                Err(NativeSubscriptionError::CredentialUnavailable)
            } else {
                Ok(Some(probe.generation))
            }
        });
    bytes.fill(0);
    result
}

pub(super) fn login_epoch_matches(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    marker: &LoginRecoveryMarker,
) -> Result<bool, NativeSubscriptionError> {
    read_login_recovery_epoch(paths, provider)
        .map(|epoch| epoch.is_some_and(|(epoch, _)| epoch.matches_marker(marker)))
}

pub(super) fn login_recovery_required(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<bool, NativeSubscriptionError> {
    let key = process_quarantine_key(paths, provider, paths.login_recovery_path(provider));
    if quarantined(&LOGIN_RECOVERY_QUARANTINE, &key)? {
        return Ok(true);
    }
    let marker = read_login_recovery_marker(paths, provider);
    let epoch = read_login_recovery_epoch(paths, provider);
    match (marker, epoch) {
        (Ok(Some((marker, _))), Ok(Some((epoch, _)))) if epoch.matches_marker(&marker) => {
            Ok(marker.state == LoginRecoveryState::Armed)
        }
        (Ok(None), Ok(None)) => Ok(false),
        (Err(NativeSubscriptionError::InvalidReceipt), _)
        | (_, Err(NativeSubscriptionError::InvalidReceipt))
        | (Ok(Some(_)), Ok(None | Some(_)))
        | (Ok(None), Ok(Some(_))) => {
            quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
            Ok(true)
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

pub(super) fn login_recovery_required_for_status(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    consent_generation: Option<&str>,
    credential: NativeAuthStatus,
) -> Result<bool, NativeSubscriptionError> {
    let path = paths.login_recovery_path(provider);
    let key = process_quarantine_key(paths, provider, path);
    // Process-local quarantine is sticky for ambiguous recovery failures
    // (partial clear, restore failure, marker/epoch mismatch). Do not auto-heal
    // from durable state alone — that would report Ready after UnsafeStorage
    // mid-clear. Re-pin after successful refresh runs only when not quarantined.
    if quarantined(&LOGIN_RECOVERY_QUARANTINE, &key)? {
        return Ok(true);
    }
    // A login this build cannot use (made by an earlier build) is still a
    // stored record: it gates and re-pins like a ready one, and the operator
    // sees login-required rather than a sticky process-local quarantine.
    let ready_capable = matches!(
        credential,
        NativeAuthStatus::Ready
            | NativeAuthStatus::RefreshRequired
            | NativeAuthStatus::Quarantined
            | NativeAuthStatus::LoginRequired
    );
    let marker = match (
        read_login_recovery_marker(paths, provider),
        read_login_recovery_epoch(paths, provider),
    ) {
        (Ok(Some((marker, bytes))), Ok(Some((epoch, _)))) if epoch.matches_marker(&marker) => {
            Some((marker, bytes))
        }
        (Ok(None), Ok(None)) => None,
        (Err(NativeSubscriptionError::InvalidReceipt), _)
        | (_, Err(NativeSubscriptionError::InvalidReceipt))
        | (Ok(Some(_)), Ok(None | Some(_)))
        | (Ok(None), Ok(Some(_))) => {
            quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
            return Ok(true);
        }
        (Err(error), _) | (_, Err(error)) => return Err(error),
    };
    let Some((marker, _)) = marker else {
        if ready_capable {
            quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
            return Ok(true);
        }
        return Ok(false);
    };
    if marker.state == LoginRecoveryState::Armed {
        return Ok(true);
    }
    let current_generation = current_credential_generation(paths, provider)?;
    let consent_matches = consent_generation == Some(marker.consent_generation.as_str());
    let epoch_matches = login_epoch_matches(paths, provider, &marker)?;
    // Exact generation match: normal ready path after login clear.
    if consent_matches && epoch_matches && current_generation == marker.credential_generation {
        return Ok(false);
    }
    // Legitimate refresh advances the durable credential generation without a
    // new login. Re-pin the cleared recovery marker to the new generation so a
    // successful refresh does not permanently quarantine the slot (
    // recovery gate remains fail-closed for armed/mismatched consent/epoch).
    if consent_matches
        && epoch_matches
        && matches!(
            credential,
            NativeAuthStatus::Ready
                | NativeAuthStatus::RefreshRequired
                | NativeAuthStatus::LoginRequired
        )
        && matches!(
            (current_generation, marker.credential_generation),
            (Some(current), Some(pinned)) if current > pinned
        )
    {
        if let Some(current) = current_generation {
            match repin_cleared_login_recovery(paths, provider, &marker, current) {
                Ok(()) => return Ok(false),
                Err(_) => {
                    quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
                    return Ok(true);
                }
            }
        }
    }
    // xAI durable Unknown after a claimed refresh is retryable with the stored
    // refresh token (xAI reconciliation). Matching consent/
    // epoch means this is not a foreign slot takeover. Codex/Claude stay
    // fail-closed.
    if provider == NativeProvider::Xai
        && consent_matches
        && epoch_matches
        && credential == NativeAuthStatus::Quarantined
        && matches!(
            (current_generation, marker.credential_generation),
            (Some(current), Some(pinned)) if current >= pinned
        )
    {
        return Ok(false);
    }
    quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
    Ok(true)
}

pub(super) fn arm_login_recovery(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    consent_generation: &str,
) -> Result<LoginRecoveryOperation, NativeSubscriptionError> {
    arm_login_recovery_with_write(paths, provider, consent_generation, |path, bytes| {
        write_private_atomic(path, bytes)
    })
}

pub(super) fn arm_login_recovery_with_write(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    consent_generation: &str,
    write: impl FnOnce(&Path, &[u8]) -> Result<(), NativeSubscriptionError>,
) -> Result<LoginRecoveryOperation, NativeSubscriptionError> {
    if !valid_generation(consent_generation) {
        return Err(NativeSubscriptionError::AcceptanceRequired);
    }
    let key = process_quarantine_key(paths, provider, paths.login_recovery_path(provider));
    if quarantined(&LOGIN_RECOVERY_QUARANTINE, &key)? {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    if login_recovery_required(paths, provider)? {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    let previous = read_login_recovery_marker(paths, provider)?.map(|(_, bytes)| bytes);
    let previous_epoch = read_login_recovery_epoch(paths, provider)?.map(|(_, bytes)| bytes);
    let credential_generation_before = current_credential_generation(paths, provider)?;
    let marker = LoginRecoveryMarker::armed(new_random_identity()?, consent_generation.to_owned());
    let bytes = marker.to_bytes()?;
    let epoch = LoginRecoveryEpoch::from_marker(&marker);
    let epoch_bytes = epoch.to_bytes()?;
    if let Err(error) =
        write_private_atomic(&paths.login_recovery_epoch_path(provider), &epoch_bytes)
    {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    if let Err(error) = write(&paths.login_recovery_path(provider), &bytes) {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    let current = read_login_recovery_marker(paths, provider)?
        .ok_or(NativeSubscriptionError::UnsafeStorage)?;
    let current_epoch = read_login_recovery_epoch(paths, provider)?
        .ok_or(NativeSubscriptionError::UnsafeStorage)?;
    if current.0 != marker
        || current.1 != bytes
        || current_epoch.0 != epoch
        || current_epoch.1 != epoch_bytes
        || !login_recovery_required(paths, provider)?
    {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(LoginRecoveryOperation {
        marker,
        previous,
        previous_epoch,
        credential_generation_before,
    })
}

pub(super) fn clear_login_recovery(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    operation: &LoginRecoveryOperation,
    credential_generation: u64,
) -> Result<(), NativeSubscriptionError> {
    clear_login_recovery_with_hook(
        paths,
        provider,
        operation,
        credential_generation,
        |_| Ok(()),
    )
}

#[cfg(unix)]
pub(super) fn clear_login_recovery_with_hook(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    operation: &LoginRecoveryOperation,
    credential_generation: u64,
    hook: impl FnMut(AtomicWriteStage) -> Result<(), NativeSubscriptionError>,
) -> Result<(), NativeSubscriptionError> {
    let path = paths.login_recovery_path(provider);
    let key = process_quarantine_key(paths, provider, path.clone());
    if quarantined(&LOGIN_RECOVERY_QUARANTINE, &key)? {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    let armed_bytes = operation.marker.to_bytes()?;
    let current = read_login_recovery_marker(paths, provider)?
        .ok_or(NativeSubscriptionError::CredentialUnavailable)?;
    if current.0 != operation.marker
        || current.1 != armed_bytes
        || !login_epoch_matches(paths, provider, &operation.marker)?
    {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    let cleared = operation.marker.cleared(credential_generation);
    let cleared_bytes = cleared.to_bytes()?;
    let cleared_epoch = LoginRecoveryEpoch::from_marker(&cleared);
    let cleared_epoch_bytes = cleared_epoch.to_bytes()?;
    if let Err(error) = write_private_atomic(
        &paths.login_recovery_epoch_path(provider),
        &cleared_epoch_bytes,
    ) {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    if let Err(error) = write_private_atomic_with_hook(&path, &cleared_bytes, hook) {
        let armed_epoch = LoginRecoveryEpoch::from_marker(&operation.marker);
        let armed_epoch_bytes = armed_epoch.to_bytes()?;
        let restored_marker = write_private_atomic(&path, &armed_bytes)
            .and_then(|()| read_login_recovery_marker(paths, provider))
            .is_ok_and(|current| current.is_some_and(|value| value.0 == operation.marker));
        let restored_epoch = write_private_atomic(
            &paths.login_recovery_epoch_path(provider),
            &armed_epoch_bytes,
        )
        .and_then(|()| read_login_recovery_epoch(paths, provider))
        .is_ok_and(|current| current.is_some_and(|value| value.0 == armed_epoch));
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        if !restored_marker || !restored_epoch {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        return Err(error);
    }
    let current = read_login_recovery_marker(paths, provider)?
        .ok_or(NativeSubscriptionError::UnsafeStorage)?;
    let current_epoch = read_login_recovery_epoch(paths, provider)?
        .ok_or(NativeSubscriptionError::UnsafeStorage)?;
    if current.0 != cleared
        || current.1 != cleared_bytes
        || current_epoch.0 != cleared_epoch
        || current_epoch.1 != cleared_epoch_bytes
        || current_credential_generation(paths, provider)? != Some(credential_generation)
        || !login_epoch_matches(paths, provider, &cleared)?
    {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn clear_login_recovery_with_hook(
    _paths: &SubscriptionPaths,
    _provider: NativeProvider,
    _operation: &LoginRecoveryOperation,
    _credential_generation: u64,
    _hook: impl FnMut(()) -> Result<(), NativeSubscriptionError>,
) -> Result<(), NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

pub(super) fn restore_login_recovery(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    operation: &LoginRecoveryOperation,
) -> Result<(), NativeSubscriptionError> {
    let path = paths.login_recovery_path(provider);
    let key = process_quarantine_key(paths, provider, path.clone());
    let armed = operation.marker.to_bytes()?;
    let current = read_login_recovery_marker(paths, provider)?
        .ok_or(NativeSubscriptionError::CredentialUnavailable)?;
    if current.0 != operation.marker
        || current.1 != armed
        || !login_epoch_matches(paths, provider, &operation.marker)?
    {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    let restored_marker = match &operation.previous {
        Some(bytes) => write_private_atomic(&path, bytes),
        None => remove_private_file_durable(&path),
    };
    if let Err(error) = restored_marker {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    let epoch_path = paths.login_recovery_epoch_path(provider);
    let restored_epoch = match &operation.previous_epoch {
        Some(bytes) => write_private_atomic(&epoch_path, bytes),
        None => remove_private_file_durable(&epoch_path),
    };
    if let Err(error) = restored_epoch {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    let current_marker = read_login_recovery_marker(paths, provider)?.map(|(_, bytes)| bytes);
    let current_epoch = read_login_recovery_epoch(paths, provider)?.map(|(_, bytes)| bytes);
    if current_marker.as_ref() != operation.previous.as_ref()
        || current_epoch.as_ref() != operation.previous_epoch.as_ref()
    {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(())
}

pub(super) fn retire_login_recovery(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
) -> Result<(), NativeSubscriptionError> {
    let path = paths.login_recovery_path(provider);
    let key = process_quarantine_key(paths, provider, path.clone());
    if let Err(error) = remove_private_file_durable(&path) {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    if let Err(error) = remove_private_file_durable(&paths.login_recovery_epoch_path(provider)) {
        quarantine(&LOGIN_RECOVERY_QUARANTINE, key)?;
        return Err(error);
    }
    clear_quarantine(&LOGIN_RECOVERY_QUARANTINE, &key)?;
    Ok(())
}

/// Re-pins a **cleared** login-recovery marker after a legitimate credential
/// generation advance (for example owner-managed refresh).
///
/// Does not clear an armed recovery or invent a marker when none exists.
#[cfg(unix)]
pub(super) fn repin_cleared_login_recovery(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    marker: &LoginRecoveryMarker,
    credential_generation: u64,
) -> Result<(), NativeSubscriptionError> {
    repin_cleared_login_recovery_with_hook(
        paths,
        provider,
        marker,
        credential_generation,
        || Ok(()),
    )
}

/// [`repin_cleared_login_recovery`] with a hook that runs after both marker
/// files are written and before the generation verification, so tests can
/// inject a concurrent legitimate refresh at the exact racy point.
#[cfg(unix)]
pub(super) fn repin_cleared_login_recovery_with_hook(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    marker: &LoginRecoveryMarker,
    credential_generation: u64,
    mut after_write: impl FnMut() -> Result<(), NativeSubscriptionError>,
) -> Result<(), NativeSubscriptionError> {
    if marker.state != LoginRecoveryState::Cleared || credential_generation == 0 {
        return Err(NativeSubscriptionError::CredentialUnavailable);
    }
    // Every facade caller already holds the provider's compatibility flock
    // (status, refresh, catalog, and dispatch admission all take it before
    // any gated open), and that flock is not reentrant, so it is not taken
    // again here. The writer that can still race this repin is a provider
    // manager's own claimed refresh, which advances the durable generation by
    // design; that advance is legitimate and is re-pinned, never quarantined.
    let path = paths.login_recovery_path(provider);
    let key = process_quarantine_key(paths, provider, path.clone());
    let mut pin = credential_generation;
    for _attempt in 0..2 {
        let repinned = marker.cleared(pin);
        let repinned_bytes = repinned.to_bytes()?;
        let repinned_epoch = LoginRecoveryEpoch::from_marker(&repinned);
        let repinned_epoch_bytes = repinned_epoch.to_bytes()?;
        write_private_atomic(
            &paths.login_recovery_epoch_path(provider),
            &repinned_epoch_bytes,
        )?;
        write_private_atomic(&path, &repinned_bytes)?;
        after_write()?;
        let current = read_login_recovery_marker(paths, provider)?
            .ok_or(NativeSubscriptionError::UnsafeStorage)?;
        let current_epoch = read_login_recovery_epoch(paths, provider)?
            .ok_or(NativeSubscriptionError::UnsafeStorage)?;
        if current.0 != repinned
            || current.1 != repinned_bytes
            || current_epoch.0 != repinned_epoch
            || current_epoch.1 != repinned_epoch_bytes
            || !login_epoch_matches(paths, provider, &repinned)?
        {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        match current_credential_generation(paths, provider)? {
            Some(observed) if observed == pin => {
                clear_quarantine(&LOGIN_RECOVERY_QUARANTINE, &key)?;
                return Ok(());
            }
            // A concurrent legitimate refresh advanced the durable generation
            // between the write and the verification: pin the newer value once.
            Some(observed) if observed > pin => pin = observed,
            _ => return Err(NativeSubscriptionError::UnsafeStorage),
        }
    }
    Err(NativeSubscriptionError::UnsafeStorage)
}

#[cfg(not(unix))]
pub(super) fn repin_cleared_login_recovery(
    _paths: &SubscriptionPaths,
    _provider: NativeProvider,
    _marker: &LoginRecoveryMarker,
    _credential_generation: u64,
) -> Result<(), NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}
