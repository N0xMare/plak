#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use crate::store::revisions::refuse_incompatible;
use plakous_recursive_subagents_store::admission::{Schema, Status};

#[test]
fn rehearsal_revises_private_copies_without_migrating_the_live_store() {
    let home = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(home.path().to_path_buf());
    paths.ensure_layout().unwrap();
    let [(current, live), _] = databases(&paths);
    let Some((_, revisions)) = current.revisions.split_last() else {
        // A new store generation has no same-key predecessor revision.
        return;
    };
    let previous = Schema {
        revisions,
        ..current
    };
    let source = previous.open_writer(&live, |_| Ok(())).unwrap();
    source.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert!(matches!(
        current.inspect(&live).unwrap(),
        Status::Pending(revision) if revision == current.version() - 1
    ));
    refuse_incompatible(&paths).unwrap();

    let scratch = paths.home().join(".rehearsal");
    assert!(rehearse_into(&paths, &home.path().join("nested/outside")).is_err());
    let report = rehearse_into(&paths, &scratch).unwrap();
    assert!(report.revisions_pending);
    assert!(
        rehearse_into(&paths, &scratch).is_err(),
        "an existing directory is refused"
    );
    let probe = paths.with_probe_stores(report.stores);

    assert!(probe.product_database().is_file());
    assert!(matches!(
        current.inspect(&probe.product_database()).unwrap(),
        Status::Current
    ));
    assert!(matches!(previous.inspect(&live).unwrap(), Status::Current));
}

#[test]
fn a_failed_rehearsal_does_not_leave_a_probe_directory() {
    let (_temporary, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    std::fs::write(paths.product_database(), b"not a database").unwrap();
    let scratch = paths.home().join(".rehearsal");
    assert!(rehearse_into(&paths, &scratch).is_err());
    assert!(!scratch.exists());
}

#[test]
fn rehearsal_refuses_when_its_time_budget_is_exhausted() {
    bound(Duration::ZERO, REHEARSAL_BYTES, "time bound");
}

#[test]
fn rehearsal_applies_one_byte_budget_across_both_stores() {
    let (_home, paths) = budget_fixture();
    let product_bytes = std::fs::metadata(paths.product_database()).unwrap().len();
    assert!(product_bytes > 0);
    assert_bound(&paths, REHEARSAL_TIME, product_bytes, "byte bound");
}

fn bound(time: Duration, bytes: u64, reason: &str) {
    let (_home, paths) = budget_fixture();
    assert_bound(&paths, time, bytes, reason);
}

fn budget_fixture() -> (tempfile::TempDir, PlakPaths) {
    let (home, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    for (schema, source) in databases(&paths) {
        schema
            .open_writer(&source, |_| Ok(()))
            .unwrap()
            .close()
            .unwrap();
    }
    (home, paths)
}

fn assert_bound(paths: &PlakPaths, time: Duration, bytes: u64, reason: &str) {
    let scratch = paths.home().join(".rehearsal");
    let error = rehearse_with_budget(paths, &scratch, time, bytes)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("store rehearse") && error.contains(reason),
        "{error}"
    );
    assert!(!error.contains("backup"), "{error}");
    assert!(!scratch.exists());
}
