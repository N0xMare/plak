//! The one-use inference authorization that retains a Ready lease.

use super::{
    AuthError, CODEX_ORIGINATOR, CredentialFence, CredentialGenerationFingerprint, Digest,
    HeaderMap, HeaderName, HeaderValue, NativeCodexPreflightRequest, OpenAiTarget,
    ReadyCredentialLease, ResponsesPhysicalRequestBinding, StoredCredential, USER_AGENT, fmt,
    sensitive_header_value,
};
use plakous_auth::custody::CredentialField;

/// What an authorization holds its credential headers from.
pub(super) enum AuthorizationMaterial {
    /// The exact Ready lease and the record it read, under local custody.
    Local {
        lease: ReadyCredentialLease,
        credential: StoredCredential,
    },
    /// A custodian's fields for the expected fence and the login selection
    /// its facts named; the custodian checks the fence again at injection.
    Custodied {
        authorization: HeaderValue,
        account: HeaderValue,
        selection_epoch: Digest,
    },
}

/// Private one-use inference authorization retaining the exact Ready lease.
pub(crate) struct NativeCodexAuthorization {
    pub(super) material: AuthorizationMaterial,
    pub(super) expected_fence: CredentialFence,
    pub(super) expected_selection_epoch: Digest,
    pub(super) preflight: NativeCodexPreflightRequest,
    pub(super) physical_request: Option<ResponsesPhysicalRequestBinding>,
    pub(super) headers_consumed: bool,
}

impl NativeCodexAuthorization {
    /// The fence the credential is held at.
    pub(super) fn held_fence(&self) -> CredentialFence {
        match &self.material {
            AuthorizationMaterial::Local { lease, .. } => lease.fence(),
            AuthorizationMaterial::Custodied { .. } => self.expected_fence.clone(),
        }
    }

    /// Whether the held record belongs to `fence`'s account.
    pub(super) fn holds_account(&self, fence: &CredentialFence) -> bool {
        match &self.material {
            AuthorizationMaterial::Local { credential, .. } => {
                fence.account_binding().as_str() == credential.account_id
            }
            AuthorizationMaterial::Custodied { .. } => true,
        }
    }

    /// The held login selection, as a route with `account` and `authority`
    /// binds it.
    pub(super) fn held_selection_epoch(
        &self,
        account: Digest,
        authority: Digest,
    ) -> Result<Digest, super::NativeCodexAuthError> {
        match &self.material {
            AuthorizationMaterial::Local { credential, .. } => {
                super::credential_selection_epoch(account, authority, &credential.selection_epoch)
            }
            AuthorizationMaterial::Custodied {
                selection_epoch, ..
            } => Ok(*selection_epoch),
        }
    }

    pub(crate) const fn target(&self) -> &OpenAiTarget {
        self.preflight.target()
    }

    pub(crate) fn credential_generation(&self) -> CredentialGenerationFingerprint {
        // The login selection is stable across refreshes; the generation
        // keeps one fingerprint per stored credential generation.
        let mut binding = self.expected_selection_epoch.bytes().to_vec();
        binding.extend_from_slice(&self.expected_fence.generation().get().to_be_bytes());
        match CredentialGenerationFingerprint::mint_exact(
            plakous_auth::api_key::CredentialProviderTag::OpenAiPlatform,
            &binding,
        ) {
            Ok(fingerprint) => fingerprint,
            Err(_) => unreachable!("a digest is a valid credential-generation binding"),
        }
    }

    pub(crate) fn revalidate(
        &self,
        target: &OpenAiTarget,
        physical: &ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        if physical != self.preflight.physical()
            || self.held_fence() != self.expected_fence
            || !self.holds_account(&self.expected_fence)
        {
            return Err(AuthError::BindingMismatch);
        }
        validate_bound_credential_selection(
            target,
            &self.expected_fence,
            self.expected_selection_epoch,
        )
    }

    pub(crate) fn authorize_headers(
        &mut self,
        target: &OpenAiTarget,
        headers: &mut HeaderMap,
    ) -> Result<(), AuthError> {
        if self.headers_consumed {
            return Err(AuthError::AuthorizationAlreadyResolved);
        }
        let physical_request = self
            .physical_request
            .as_ref()
            .ok_or(AuthError::NoActivePhysicalRequest)?;
        if physical_request != self.preflight.physical() {
            return Err(AuthError::BindingMismatch);
        }
        validate_bound_credential_selection(
            target,
            &self.expected_fence,
            self.expected_selection_epoch,
        )?;
        validate_bound_credential_selection(
            self.preflight.target(),
            &self.expected_fence,
            self.expected_selection_epoch,
        )?;
        let fence = self.held_fence();
        if fence != self.expected_fence || !self.holds_account(&fence) {
            return Err(AuthError::BindingMismatch);
        }
        let (authorization, account) = match &self.material {
            AuthorizationMaterial::Local { credential, .. } => {
                let bearer = CredentialField::bearer(credential.access_token.as_bytes());
                let authorization = sensitive_header_value(bearer.into_header_bytes())
                    .map_err(|_| AuthError::SourceFailure)?;
                let account = CredentialField::exact(credential.account_id.as_bytes().to_vec());
                let account = sensitive_header_value(account.into_header_bytes())
                    .map_err(|_| AuthError::BindingMismatch)?;
                (authorization, account)
            }
            AuthorizationMaterial::Custodied {
                authorization,
                account,
                ..
            } => (authorization.clone(), account.clone()),
        };
        headers.insert(HeaderName::from_static("authorization"), authorization);
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        headers.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static(CODEX_ORIGINATOR),
        );
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(USER_AGENT),
        );
        self.headers_consumed = true;
        Ok(())
    }

    pub(super) fn bind_physical(
        &mut self,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        if self.physical_request.is_some() || physical_request != *self.preflight.physical() {
            return Err(AuthError::BindingMismatch);
        }
        validate_bound_credential_selection(
            self.preflight.target(),
            &self.expected_fence,
            self.expected_selection_epoch,
        )?;
        self.physical_request = Some(physical_request);
        Ok(())
    }
}

fn validate_bound_credential_selection(
    target: &OpenAiTarget,
    expected_fence: &CredentialFence,
    expected_selection_epoch: Digest,
) -> Result<(), AuthError> {
    let Some((target_fence, target_selection_epoch)) = target.native_codex_credential_selection()
    else {
        return Err(AuthError::BindingMismatch);
    };
    if target_fence != expected_fence || target_selection_epoch != expected_selection_epoch {
        return Err(AuthError::BindingMismatch);
    }
    Ok(())
}

impl fmt::Debug for NativeCodexAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeCodexAuthorization(<redacted lease-bound grant>)")
    }
}
