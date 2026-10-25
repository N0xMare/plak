//! Credential-store record regressions.

use crate::private_store::Fixture;
use crate::private_store::MAX_RECORD_FILE_BYTES;
use crate::private_store::PrivateCredentialStore;
use crate::private_store::PrivateStoreError;
use crate::{
    AccountBinding, CredentialFence, CredentialGeneration, CredentialStatus, SecretBytes,
    SubscriptionIntegrationProfileId,
};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;

#[test]
fn rejects_foreign_malformed_unknown_and_oversized_records()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let documents = [
        json!({
            "format_version": 999,
            "profile_id": "profile",
            "slot_id": "primary",
            "account_binding": "opaque-account",
            "generation": 1,
            "status": "ready",
            "payload": "eA=="
        }),
        json!({
            "format_version": 1,
            "profile_id": "profile",
            "account_binding": "opaque-account",
            "generation": 1,
            "status": "ready",
            "payload": "eA=="
        }),
        json!({
            "format_version": 2,
            "profile_id": "profile",
            "slot_id": "primary",
            "account_binding": "opaque-account",
            "generation": 1,
            "status": "ready",
            "payload": "eA==",
            "future_security_field": true
        }),
        json!({
            "format_version": 2,
            "profile_id": "foreign",
            "slot_id": "primary",
            "account_binding": "opaque-account",
            "generation": 1,
            "status": "ready",
            "payload": "eA=="
        }),
        json!({
            "format_version": 2,
            "profile_id": "profile",
            "slot_id": "other-slot",
            "account_binding": "opaque-account",
            "generation": 1,
            "status": "ready",
            "payload": "eA=="
        }),
    ];
    let expected = [
        PrivateStoreError::WrittenByNewerBuild,
        PrivateStoreError::UnsupportedVersion,
        PrivateStoreError::MalformedRecord,
        PrivateStoreError::ForeignProfile,
        PrivateStoreError::ForeignSlot,
    ];
    for (document, expected) in documents.into_iter().zip(expected) {
        fs::write(&fixture.path, serde_json::to_vec(&document)?)?;
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
        assert!(matches!(store.load(), Err(error) if error == expected));
    }

    let oversized_length = usize::try_from(MAX_RECORD_FILE_BYTES)? + 1;
    fs::write(&fixture.path, vec![b'x'; oversized_length])?;
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
    assert!(matches!(
        store.load(),
        Err(PrivateStoreError::RecordTooLarge)
    ));
    Ok(())
}

#[test]
fn newer_envelopes_and_profiles_are_refused_untouched_and_retire_only_locally()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let current = SubscriptionIntegrationProfileId::new("vendor.product.compat.2026-07-31.r1")?;
    let store =
        PrivateCredentialStore::open_slot(&fixture.path, current.clone(), fixture.slot.clone())?;
    let document = |format_version: u32, profile: &str, status: &str| {
        json!({
            "format_version": format_version,
            "profile_id": profile,
            "slot_id": "primary",
            "account_binding": "opaque-account",
            "generation": 4,
            "status": status,
            "payload": if status == "revoked" { "" } else { "eA==" }
        })
    };
    let cases = [
        (
            document(1, current.as_str(), "ready"),
            Some(PrivateStoreError::UnsupportedVersion),
        ),
        (document(2, current.as_str(), "ready"), None),
        (
            document(3, current.as_str(), "ready"),
            Some(PrivateStoreError::WrittenByNewerBuild),
        ),
        (
            document(2, "vendor.product.compat.2026-07-31.r0", "ready"),
            Some(PrivateStoreError::ForeignProfile),
        ),
        (
            document(2, "vendor.product.compat.2026-07-31.r2", "refresh_required"),
            Some(PrivateStoreError::WrittenByNewerBuild),
        ),
        (
            document(
                2,
                "vendor.product.compat.2026-08-01.r1",
                "refresh_outcome_unknown",
            ),
            Some(PrivateStoreError::WrittenByNewerBuild),
        ),
        (
            document(2, "vendor-renamed-profile", "ready"),
            Some(PrivateStoreError::WrittenByNewerBuild),
        ),
        // A newer build's tombstone holds no login, so it is not guarded.
        (
            document(2, "vendor.product.compat.2026-08-01.r1", "revoked"),
            Some(PrivateStoreError::ForeignProfile),
        ),
    ];
    for (document, expected) in cases {
        let bytes = serde_json::to_vec(&document)?;
        fs::write(&fixture.path, &bytes)?;
        fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
        assert_eq!(store.load().err(), expected, "{document}");
        assert_eq!(store.status().err(), expected);
        assert_eq!(store.inspect_payload(SecretBytes::len).err(), expected);
        if expected == Some(PrivateStoreError::WrittenByNewerBuild) {
            let fence = CredentialFence::new(
                current.clone(),
                AccountBinding::new("opaque-account")?,
                CredentialGeneration::new(4)?,
                CredentialStatus::Ready,
            );
            assert!(store.begin_refresh(&fence).is_err());
            assert!(store.try_acquire_ready(&fence).is_err());
            assert_eq!(fs::read(&fixture.path)?, bytes, "{document}");
        }
    }

    // Explicit local retirement still works: a newer profile becomes a
    // tombstone, and a newer envelope it cannot decode is removed.
    let newer_profile =
        serde_json::to_vec(&document(2, "vendor.product.compat.2026-08-01.r1", "ready"))?;
    fs::write(&fixture.path, &newer_profile)?;
    assert_eq!(
        store.logout_local()?,
        crate::OfflineCredentialLifecycle::Revoked
    );
    assert_eq!(store.status()?, Some(CredentialStatus::Revoked));
    fs::write(
        &fixture.path,
        serde_json::to_vec(&document(3, current.as_str(), "ready"))?,
    )?;
    assert_eq!(
        store.logout_local()?,
        crate::OfflineCredentialLifecycle::SignedOut
    );
    assert!(!fixture.path.exists());
    assert_eq!(store.status()?, None);
    Ok(())
}
