#![allow(clippy::unwrap_used, reason = "isolated retention authority fixtures")]

use super::*;
use crate::state::Tables;
use crate::{FileOwnerFloors, OwnerFloors};
use std::sync::Arc;

fn first() -> LineOwner {
    LineOwner {
        line: "line".into(),
        generation: 1,
        branch: "branch".into(),
        fence: 1,
    }
}

#[test]
fn review_c47_orphan_does_not_strip_retention_authority() {
    let root = tempfile::tempdir().unwrap();
    let mut tables = Tables::with_floors(Arc::new(
        FileOwnerFloors::open(root.path().join("owners.json")).unwrap(),
    ));
    let from = first();
    tables.transfer("home", "same", "native".into(), None, from.clone());
    let to = LineOwner {
        fence: 2,
        ..from.clone()
    };
    tables.transfer_retained("home", "same", "retain".into(), &from, to.clone());
    tables.orphan("home", "same");
    assert!(tables.retained_by("home", "same", &to));
    assert!(!tables.held_by("home", "same", &to));
    assert_eq!(tables.held_lines("home", "same"), []);
}

#[test]
fn retention_transfer_persists_native_ban_and_lost_ack_reconciles_only_own_tuple() {
    let root = tempfile::tempdir().unwrap();
    let floors = Arc::new(FileOwnerFloors::open(root.path().join("owners.json")).unwrap());
    let mut tables = Tables::with_floors(floors.clone());
    let from = first();
    assert!(matches!(
        tables.transfer("home", "old", "native".into(), None, from.clone()),
        RunnerFrame::TransferAccepted { .. }
    ));
    let to = LineOwner {
        fence: 2,
        ..from.clone()
    };
    assert!(matches!(
        tables.transfer_retained("home", "new", "retain".into(), &from, to.clone()),
        RunnerFrame::TransferAccepted { .. }
    ));
    assert!(floors.load()[0].1.retention_only);
    let reopened = FileOwnerFloors::open(root.path().join("owners.json")).unwrap();
    let mut restarted = Tables::with_floors(Arc::new(reopened));
    assert!(matches!(
        restarted.transfer("home", "new", "downgrade".into(), Some(&to), to.clone()),
        RunnerFrame::TransferRefused { .. }
    ));
    assert!(matches!(
        restarted.transfer_retained("home", "new", "lost-ack".into(), &from, to.clone()),
        RunnerFrame::TransferAccepted { .. }
    ));
    assert!(!restarted.held_by("home", "new", &to));
    assert!(restarted.retained_by("home", "new", &to));
    assert!(matches!(
        restarted.transfer_retained("home", "foreign", "steal".into(), &from, to.clone()),
        RunnerFrame::StaleOwner { .. }
    ));
    assert!(restarted.owner("other-home", "line").is_none());
}

#[test]
fn retention_recovery_never_dispatches_an_unseen_job_control_or_exposes_it_natively() {
    let root = tempfile::tempdir().unwrap();
    let mut tables = Tables::with_floors(Arc::new(
        FileOwnerFloors::open(root.path().join("owners.json")).unwrap(),
    ));
    let from = first();
    tables.transfer("home", "old", "native".into(), None, from.clone());
    let to = LineOwner {
        fence: 2,
        ..from.clone()
    };
    tables.transfer_retained("home", "new", "retain".into(), &from, to.clone());
    let mut request = Request {
        request_id: "request".into(),
        owner: Some(to),
        class: RequestClass::OwnerControl {
            control_id: "input".into(),
        },
        deadline_ms: None,
        digest: "digest".into(),
        replay: true,
        age_ms: Some(1),
        operation: Operation {
            verb: RunnerVerb::ProcessWrite.as_str().into(),
            payload: json!({}),
        },
    };
    assert!(matches!(
        tables.admit_retained("home", "new", &request, Instant::now()),
        Admission::Answer(RunnerFrame::NotSeen { .. })
    ));
    request.replay = false;
    assert!(matches!(
        tables.admit_retained("home", "new", &request, Instant::now()),
        Admission::Answer(RunnerFrame::NotSeen { .. })
    ));
    assert!(matches!(
        tables.admit("home", "new", &request, Instant::now()),
        Admission::Answer(RunnerFrame::Refused { .. })
    ));
}
