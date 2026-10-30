//! The one-use inference authorization that retains a Ready lease.

use super::{
    AuthError, CODEX_ORIGINATOR, CredentialFence, CredentialGenerationFingerprint, Digest,
    HeaderMap, HeaderName, HeaderValue, NativeCodexPreflightRequest, OpenAiTarget,
    ReadyCredentialLease, ResponsesPhysicalRequestBinding, StoredCredential, USER_AGENT, fmt,
    sensitive_header_value,
};

/// Private one-use inference authorization retaining the exact Ready lease.
pub(crate) struct NativeCodexAuthorization {
    pub(super) lease: ReadyCredentialLease,
    pub(super) expected_fence: CredentialFence,
    pub(super) expected_selection_epoch: Digest,
    pub(super) credential: StoredCredential,
    pub(super) preflight: NativeCodexPreflightRequest,
    pub(super) physical_request: Option<ResponsesPhysicalRequestBinding>,
    pub(super) headers_consumed: bool,
}

impl NativeCodexAuthorization {
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
            || self.lease.fence() != self.expected_fence
            || self.expected_fence.account_binding().as_str() != self.credential.account_id
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
        let fence = self.lease.fence();
        if fence != self.expected_fence
            || fence.account_binding().as_str() != self.credential.account_id
        {
            return Err(AuthError::BindingMismatch);
        }
        let mut bearer = Vec::with_capacity(7 + self.credential.access_token.len());
        bearer.extend_from_slice(b"Bearer ");
        bearer.extend_from_slice(self.credential.access_token.as_bytes());
        let authorization = sensitive_header_value(bearer).map_err(|_| AuthError::SourceFailure)?;
        let account = sensitive_header_value(self.credential.account_id.as_bytes().to_vec())
            .map_err(|_| AuthError::BindingMismatch)?;
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
