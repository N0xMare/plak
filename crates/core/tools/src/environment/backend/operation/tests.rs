use std::str::FromStr;

use plakous_model::{BranchId, CancellationToken, LineId, SessionId};

use super::{LineOwner, OperationClass, OperationContext};
use crate::JobOwner;

fn ids() -> (LineId, BranchId, BranchId) {
    (
        LineId::from_str("00000000-0000-0000-0000-000000000001").unwrap(),
        BranchId::from_str("00000000-0000-0000-0000-000000000002").unwrap(),
        BranchId::from_str("00000000-0000-0000-0000-000000000003").unwrap(),
    )
}

/// Owners of one line order by `(generation, fence)`: a successor head is
/// newer whatever its fence, a same-branch reopen is newer only under a later
/// fence, and another line's owner is never newer.
#[test]
fn line_owners_order_by_generation_then_fence() {
    let (line, head, successor) = ids();
    let owner = LineOwner::new(line, 2, head, 7);
    assert!(LineOwner::new(line, 3, successor, 1).is_newer_than(&owner));
    assert!(LineOwner::new(line, 2, head, 8).is_newer_than(&owner));
    assert!(!LineOwner::new(line, 2, head, 7).is_newer_than(&owner));
    assert!(!LineOwner::new(line, 1, head, 99).is_newer_than(&owner));
    let other = LineId::from_str("00000000-0000-0000-0000-000000000009").unwrap();
    assert!(!LineOwner::new(other, 9, head, 9).is_newer_than(&owner));
}

#[test]
fn a_job_owner_names_its_line_owner() {
    let (line, head, _) = ids();
    let job_owner = JobOwner {
        line,
        generation: 4,
        session_id: SessionId::from_str("00000000-0000-0000-0000-000000000004").unwrap(),
        branch_id: head,
        fence: 11,
    };
    assert_eq!(
        LineOwner::from(&job_owner),
        LineOwner::new(line, 4, head, 11)
    );
}

#[test]
fn a_bare_context_is_unkeyed_ownerless_and_unbounded() {
    let token = CancellationToken::new();
    let context = OperationContext::new(token.clone());
    assert_eq!(context.owner(), None);
    assert_eq!(context.class(), OperationClass::Unkeyed);
    assert_eq!(context.budget(), None);
    assert!(context.check_cancelled().is_ok());
    token.cancel();
    assert!(context.check_cancelled().is_err());
}
