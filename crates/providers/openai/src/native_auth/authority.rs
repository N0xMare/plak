//! The manager's native Codex credential authority.

use super::{
    AuthError, CodexSubscriptionCredentialManager, CredentialFence, CredentialStatus, Digest,
    NativeCodexAuthorization, NativeCodexCatalogWitness, NativeCodexCredentialAuthority,
    NativeCodexForcedRefresh, NativeCodexPreflight, NativeCodexPreflightRequest,
    NativeCodexRuntimeAuthority, OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, ResolvedModelDescriptor,
    ResponsesPhysicalRequestBinding, SubscriptionLimitError, SubscriptionRequestPermit, Timestamp,
    credential_selection_epoch,
};

impl NativeCodexCredentialAuthority for CodexSubscriptionCredentialManager {
    fn route_account_binding(&self) -> Digest {
        self.route_account_digest()
    }

    fn authority_binding(&self) -> Digest {
        self.authority_binding
    }

    fn validate_catalog_witness(
        &self,
        descriptor: &ResolvedModelDescriptor,
        witness: &NativeCodexCatalogWitness,
    ) -> Result<(), AuthError> {
        self.validate_profile_admission()
            .map_err(|_| AuthError::Unavailable)?;
        let selected = self
            .selected_account()
            .map_err(|_| AuthError::Unavailable)?;
        // The witness binds its own login selection; the stored selection is
        // compared wherever the payload is read (revalidation, preflight).
        let expected = self
            .catalog_witness_binding(
                descriptor,
                witness.credential_fence(),
                witness.credential_selection_epoch(),
                witness.observed_at(),
                witness.expires_at(),
                witness.responses_lite(),
            )
            .map_err(|_| AuthError::BindingMismatch)?;
        if witness.binding() != expected
            || descriptor.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || witness.credential_fence().status() != CredentialStatus::Ready
            || witness.credential_fence().account_binding() != &selected
        {
            return Err(AuthError::BindingMismatch);
        }
        Ok(())
    }

    fn revalidate_runtime_authority<'a>(
        &'a self,
        descriptor: &'a ResolvedModelDescriptor,
        witness: &'a NativeCodexCatalogWitness,
        checked_at: Timestamp,
    ) -> NativeCodexRuntimeAuthority<'a> {
        Box::pin(self.revalidate_model_runtime_authority(descriptor, witness, checked_at))
    }

    fn preflight(&self, request: NativeCodexPreflightRequest) -> NativeCodexPreflight<'_> {
        Box::pin(self.preflight_authorization(request))
    }

    fn refresh_after_rejection<'a>(
        &'a self,
        rejected: &'a CredentialFence,
        selection_epoch: Digest,
    ) -> NativeCodexForcedRefresh<'a> {
        Box::pin(async move {
            self.refresh_rejected_selection(rejected, selection_epoch)
                .await
                .map_err(|_| AuthError::SourceFailure)
        })
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, AuthError> {
        self.request_gate
            .as_ref()
            .ok_or(AuthError::Unavailable)?
            .acquire_inference(input_token_upper_bound)
            .map_err(|error| match error {
                SubscriptionLimitError::InferenceInputLimitExceeded => {
                    AuthError::InferenceInputLimit
                }
                SubscriptionLimitError::ClassLimitExceeded {
                    reserved,
                    allowance,
                    ..
                } => AuthError::InferenceClassLimit {
                    reserved,
                    allowance,
                },
                SubscriptionLimitError::InferenceScopeUnavailable => {
                    AuthError::InferenceClassLimit {
                        reserved: 0,
                        allowance: 0,
                    }
                }
                _ => AuthError::Unavailable,
            })
    }

    fn bind(
        &self,
        authorization: &mut NativeCodexAuthorization,
        physical_request: ResponsesPhysicalRequestBinding,
    ) -> Result<(), AuthError> {
        self.validate_profile_admission()
            .map_err(|_| AuthError::Unavailable)?;
        let target = authorization.target();
        let fence = authorization.lease.fence();
        let expected_epoch = credential_selection_epoch(
            target.account_binding(),
            target.credential_authority_binding(),
            &authorization.credential.selection_epoch,
        )
        .map_err(|_| AuthError::BindingMismatch)?;
        if target.route_id().as_str() != crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID
            || target.profile().as_str() != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || target.account_binding() != self.route_account_digest()
            || target.credential_authority_binding() != self.authority_binding
            || fence != authorization.expected_fence
            || expected_epoch != authorization.expected_selection_epoch
            || fence.account_binding().as_str() != authorization.credential.account_id
            || fence.account_binding()
                != &self
                    .selected_account()
                    .map_err(|_| AuthError::Unavailable)?
        {
            return Err(AuthError::BindingMismatch);
        }
        authorization.bind_physical(physical_request)
    }
}
