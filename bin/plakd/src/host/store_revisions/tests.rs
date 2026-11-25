#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use crate::command::HostCommand;
use crate::shell::services::Services;
use crate::store::{ProductStore, revisions::refuse_incompatible};
use plakous_recursive_subagents_store::admission::{Schema, Status};

#[test]
fn service_rehearses_the_previous_product_revision_without_migrating_the_live_store() {
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

    let scratch = tempfile::tempdir_in(home.path()).unwrap();
    let _request = request_rehearsal(scratch.path()).unwrap();
    crate::host::preflight_upgrade_service(
        &Services::for_home(paths.clone()),
        &HostCommand::default(),
        &scratch.path().join("service.json"),
    )
    .unwrap();
    let probe = paths.with_probe_stores(scratch.path().join("stores"));

    assert!(probe.product_database().is_file());
    assert_eq!(
        ProductStore::inspect_upgrade_roots(&probe).unwrap().len(),
        0
    );
    assert!(matches!(previous.inspect(&live).unwrap(), Status::Current));
}
