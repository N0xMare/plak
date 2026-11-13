use super::*;

fn owner(generation: u64, fence: u64) -> WireOwner {
    WireOwner {
        line: "line".into(),
        branch: "branch".into(),
        generation,
        fence,
    }
}

#[test]
fn move_reconciliation_never_leapfrogs_a_foreign_or_lower_witness() {
    let source = owner(2, 9);
    let intended = OwnerTransfer {
        from: Some(source.clone()),
        to: owner(2, 10),
    };
    assert!(allowed(Some(&source), Some(&source), None));
    assert!(allowed(Some(&intended.to), Some(&source), Some(&intended)));
    assert!(!allowed(Some(&owner(2, 8)), Some(&source), Some(&intended)));
    assert!(!allowed(
        Some(&owner(2, 11)),
        Some(&source),
        Some(&intended)
    ));
    assert!(!allowed(Some(&owner(3, 1)), Some(&source), Some(&intended)));
    assert!(!allowed(None, Some(&source), Some(&intended)));
    assert!(allowed(None, None, None));
    let next = OwnerTransfer {
        from: Some(intended.to.clone()),
        to: owner(2, 12),
    };
    assert!(allowed(Some(&intended.to), Some(&source), Some(&next)));
    assert!(allowed(Some(&next.to), Some(&source), Some(&next)));
    assert!(!allowed(Some(&owner(2, 11)), Some(&source), Some(&next)));
}
