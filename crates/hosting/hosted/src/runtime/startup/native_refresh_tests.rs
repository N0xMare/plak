#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use plakous::model::{
    DigestDomain, LanguageModel, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, PreparedModelCall, RuntimeModelAuthorityOperation, Timestamp,
    ValidatedRequestPlan,
};
use std::sync::Arc;

pub(in crate::runtime) struct AwaitingNativeModel;

impl LanguageModel for AwaitingNativeModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        _: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async { Err(RuntimeModelAuthorityError::AwaitingCredential) })
    }

    fn prepare_call(
        self: Arc<Self>,
        _: ValidatedRequestPlan,
        _: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        Err(ModelPreparationError::rejected(
            ModelPreparationErrorKind::InvalidConfiguration,
            "freshness probe cannot dispatch",
        ))
    }
}

pub(in crate::runtime) fn binding(generation: u64) -> FreshNativeBinding {
    let digest = |domain| Digest::sha256(domain, 5, &[b"same-provider-account"]).unwrap();
    FreshNativeBinding {
        selected: serde_json::from_value(
            serde_json::to_value(digest(DigestDomain::SelectedProviderConfiguration)).unwrap(),
        )
        .unwrap(),
        effective: serde_json::from_value(
            serde_json::to_value(digest(DigestDomain::EffectiveProviderConfiguration)).unwrap(),
        )
        .unwrap(),
        account: digest(DigestDomain::ModelBinding),
        credential_authority: digest(DigestDomain::ModelBinding),
        selection: digest(DigestDomain::ModelBinding),
        generation,
    }
}

#[tokio::test]
async fn awaiting_native_at_startup_resolves_to_named_login_and_quarantine_guidance() {
    for (quarantined, expected) in [
        (
            false,
            "Claude credential is unavailable (signed out, quarantined, or a login made by an earlier build); run `plak login claude`",
        ),
        (
            true,
            "Claude credential was rejected at refresh; run `plak login claude`",
        ),
    ] {
        let error = refresh_native_authority(
            Arc::new(AwaitingNativeModel),
            "Claude",
            "claude",
            async { Err(native_credential_message("Claude", "claude", quarantined)) },
            async { panic!("AwaitingCredential must attempt fresh resolution") },
        )
        .await
        .unwrap_err();
        assert_eq!(error.message(), expected);
    }
}

#[tokio::test]
async fn awaiting_native_preserves_named_login_selection_refusal() {
    let old = binding(1);
    let mut fresh = binding(2);
    fresh.selection =
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"new-login-selection"]).unwrap();
    let error = refresh_native_authority(
        Arc::new(AwaitingNativeModel),
        "Codex",
        "codex",
        async {
            assert!(!old.admits(&fresh));
            Err(old.refusal(&fresh, "Codex", "codex"))
        },
        async { panic!("AwaitingCredential must attempt fresh resolution") },
    )
    .await
    .unwrap_err();
    assert!(error.message().contains("login selection changed"));
    assert!(error.message().contains("/reload or /rebind"));
    assert!(!error.message().contains("cannot be refreshed"));
}
