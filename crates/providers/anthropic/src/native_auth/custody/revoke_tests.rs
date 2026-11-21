use super::super::tests::{manager, private_store};
use super::super::{
    ClaudeNativeAuthManager, CredentialSlotId, CredentialStatus, PrivateCredentialStore,
    auth_profile_id,
};
use super::{CustodyError, advance_ready_unchanged, store_error};

#[test]
fn local_revoke_rereads_once_and_maps_a_second_generation_move_to_custody_busy() {
    for peer_moves in 1..=2 {
        let temp = tempfile::tempdir().unwrap();
        let path = private_store(&temp);
        let manager = manager(path.clone());
        manager
            .persist_test_grant("offline-access", "offline-refresh", 3600, "user:inference")
            .unwrap();
        let peer = PrivateCredentialStore::open_slot(
            path,
            auth_profile_id().unwrap(),
            CredentialSlotId::new("primary").unwrap(),
        )
        .unwrap();
        let store = manager.core.store().unwrap();
        let before = peer.load().unwrap().unwrap();
        let mut moves = peer_moves;
        let outcome = ClaudeNativeAuthManager::revoke_from_store(store, || {
            let fence = store.load().unwrap().unwrap();
            if moves > 0 {
                advance_ready_unchanged(&peer);
                moves -= 1;
            }
            Ok(fence)
        });
        let after = peer.load().unwrap().unwrap();
        assert_eq!(moves, 0);
        assert_eq!(after.generation().get(), before.generation().get() + 2);
        if peer_moves == 1 {
            assert_eq!(outcome.unwrap(), after.generation());
            assert_eq!(after.status(), CredentialStatus::Revoked);
        } else {
            assert!(matches!(
                store_error(outcome.unwrap_err()),
                CustodyError::Busy { .. }
            ));
            assert_eq!(after.status(), CredentialStatus::Ready);
        }
    }
}
