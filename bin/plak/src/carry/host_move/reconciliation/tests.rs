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
        previous: Vec::new(),
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
        previous: Vec::new(),
        from: Some(intended.to.clone()),
        to: owner(2, 12),
    };
    assert!(allowed(Some(&intended.to), Some(&source), Some(&next)));
    assert!(allowed(Some(&next.to), Some(&source), Some(&next)));
    assert!(!allowed(Some(&owner(2, 11)), Some(&source), Some(&next)));
    let pending = OwnerTransfer {
        from: Some(source.clone()),
        to: owner(2, 12),
        previous: vec![owner(2, 10)],
    };
    assert!(
        allowed(Some(&owner(2, 10)), Some(&source), Some(&pending)),
        "a delayed own CAS is not a foreign witness"
    );
    assert!(!allowed(Some(&owner(2, 11)), Some(&source), Some(&pending)));
}

#[test]
fn move_reconciliation_checks_every_original_before_taking_a_higher_lease() {
    use crate::carry::host_move::JobCut;
    let source = owner(2, 9);
    let intended = OwnerTransfer {
        previous: Vec::new(),
        from: Some(source.clone()),
        to: owner(2, 10),
    };
    let floor = |native, retained| plakous::runner::protocol::OwnerFloor {
        line: "line".into(),
        owner: native,
        retained,
    };
    assert_eq!(
        witness_from(
            floor(None, vec![source.clone()]),
            Some(&source),
            None,
            JobCut::Retained
        )
        .unwrap(),
        Some(source.clone())
    );
    assert!(
        witness_from(
            floor(None, vec![source.clone()]),
            Some(&source),
            None,
            JobCut::Settled
        )
        .is_err()
    );
    assert_eq!(
        witness_from(
            floor(None, vec![source.clone(), intended.to.clone()]),
            Some(&source),
            Some(&intended),
            JobCut::Retained
        )
        .unwrap(),
        Some(intended.to.clone())
    );
    for foreign in [owner(2, 8), owner(2, 11), owner(3, 1)] {
        assert!(
            witness_from(
                floor(Some(source.clone()), vec![foreign.clone()]),
                Some(&source),
                Some(&intended),
                JobCut::Retained
            )
            .is_err()
        );
        assert!(
            witness_from(
                floor(Some(foreign), vec![source.clone()]),
                Some(&source),
                Some(&intended),
                JobCut::Retained
            )
            .is_err()
        );
    }
    assert!(witness_from(floor(None, vec![source]), None, None, JobCut::Retained).is_err());
    assert_eq!(
        witness_from(floor(None, vec![]), None, None, JobCut::Settled).unwrap(),
        None
    );
}
