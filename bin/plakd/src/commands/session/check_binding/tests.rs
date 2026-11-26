#![allow(clippy::unwrap_used, reason = "binding inspection fixtures")]

use super::*;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

fn seeded(directory: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/hosting/hosted/tests/fixtures/current-bindings/runtime.sqlite3");
    let runtime = directory.join("runtime.sqlite3");
    std::fs::copy(fixture, &runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o600)).unwrap();
    runtime
}

fn heads(runtime: &Path) -> Vec<(String, String)> {
    let (report, _) =
        inspect_with_dispositions(runtime, HashMap::new(), HashMap::new(), BindingScope::All)
            .unwrap();
    let mut heads: Vec<_> = report
        .sessions
        .iter()
        .map(|row| (location(row), row.checkpoint.clone()))
        .collect();
    heads.sort();
    heads
}

/// The `<session>/<branch>` key product and tree dispositions use.
fn location(row: &Row) -> String {
    format!("{}/{}", row.session, row.branch)
}

fn corrupt(runtime: &Path, checkpoint: &str) {
    let connection = rusqlite::Connection::open(runtime).unwrap();
    let changed = connection
        .execute(
            "UPDATE runtime_object_bytes SET bytes=x'00' WHERE id IN (
                SELECT o.id FROM runtime_objects o JOIN checkpoints c
                ON (o.kind,o.codec,o.digest)=(c.root_kind,c.root_codec,c.root_digest)
                WHERE c.checkpoint_id=?1
            )",
            [checkpoint],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

fn comparable(row: &Row) -> bool {
    !row.archived && !row.superseded && !row.closed
}

fn json(rows: impl IntoIterator<Item = impl Serialize>) -> Vec<serde_json::Value> {
    rows.into_iter()
        .map(|row| serde_json::to_value(row).unwrap())
        .collect()
}

/// Archived, superseded and closed rows regress too, but neither scope
/// lets them gate. The gating scope decodes only the rows that can.
#[test]
fn the_gating_scan_gives_the_full_scan_verdicts() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = seeded(directory.path());
    let heads = heads(&runtime);
    assert!(heads.len() >= 10, "{heads:?}");
    let [
        product_archived,
        product_superseded,
        runtime_archived,
        closed_child,
        archived_tree_child,
        live_child,
        live_root,
        ..,
    ] = heads.as_slice()
    else {
        unreachable!();
    };
    let mut rows = HashMap::new();
    rows.insert(product_archived.0.clone(), (true, false));
    rows.insert(product_superseded.0.clone(), (false, true));
    rows.insert(archived_tree_child.0.clone(), (true, false));
    let mut children = HashMap::new();
    children.insert(closed_child.0.clone(), true);
    children.insert(archived_tree_child.0.clone(), false);
    children.insert(live_child.0.clone(), false);
    let (session, _) = runtime_archived.0.split_once('/').unwrap();
    let connection = rusqlite::Connection::open(&runtime).unwrap();
    connection
        .execute(
            "UPDATE sessions SET archived=1 WHERE session_id=?1",
            [session],
        )
        .unwrap();
    drop(connection);
    let decoding =
        |scope| inspect_with_dispositions(&runtime, rows.clone(), children.clone(), scope).unwrap();
    let report = |scope| decoding(scope).0;
    let (full_source, gating_source) = (report(BindingScope::All), report(BindingScope::Gating));
    for (_, checkpoint) in [
        product_archived,
        product_superseded,
        runtime_archived,
        closed_child,
        archived_tree_child,
        live_child,
        live_root,
    ] {
        corrupt(&runtime, checkpoint);
    }
    let (full, decoded) = decoding(BindingScope::All);
    assert_eq!(decoded.len(), heads.len());
    let (gating, decoded) = decoding(BindingScope::Gating);
    // Non-gating heads are never decoded, whichever flag excludes them.
    assert_eq!(decoded.len(), heads.len() - 5);
    for (key, _) in [
        product_archived,
        product_superseded,
        runtime_archived,
        closed_child,
        archived_tree_child,
    ] {
        assert!(!decoded.contains(key), "{key} was decoded");
    }

    assert_eq!(full.sessions.len(), heads.len());
    assert_eq!(gating.sessions.len(), heads.len() - 5);
    // The non-gating rows really did regress in the full audit.
    for (key, _) in [
        product_archived,
        product_superseded,
        runtime_archived,
        closed_child,
        archived_tree_child,
    ] {
        let row = full
            .sessions
            .iter()
            .find(|row| location(row) == *key)
            .unwrap();
        assert!(!comparable(row) && row.audit_failed(), "{key}");
    }
    assert_eq!(
        json(gating.sessions.iter()),
        json(full.sessions.iter().filter(|row| comparable(row)))
    );
    assert_eq!(
        (full_source.compatible, full.compatible),
        (gating_source.compatible, gating.compatible)
    );
    assert!(full_source.compatible && !full.compatible);
    for (key, _) in [live_child, live_root] {
        let row = gating
            .sessions
            .iter()
            .find(|row| location(row) == *key)
            .unwrap();
        assert!(row.audit_failed(), "{key}");
    }
}
