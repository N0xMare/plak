use super::{GENERATION, SCHEMA, SCHEMA_OBJECTS, SCHEMA_VERSION, SubagentsDatabase};

/// Identity golden for the semantic-move cohort's durable format.
#[test]
fn the_tree_schema_fingerprint_is_pinned() {
    assert_eq!(SCHEMA_VERSION, 13);
    assert_eq!(
        (GENERATION, SCHEMA.base_fingerprint),
        (
            11,
            "f853d767e6fb5d05a546fd1c32dbb7b160d85b35ae5caeb7fde2efade76ca23a"
        )
    );
}

/// The eighteen objects, in the order the fingerprint is taken over.
#[test]
fn the_tree_schema_holds_its_own_identity_row_and_the_agent_tables() {
    let names: Vec<&str> = SCHEMA_OBJECTS.iter().map(|(name, _)| *name).collect();
    assert_eq!(names[0], "subagents_schema", "identity row comes first");
    assert!(
        names[1..].iter().all(|name| name.starts_with("agent_")),
        "{names:?}"
    );
    assert_eq!(names.len(), 18);
}

/// A fresh file is created, versioned, fingerprinted, and reopens clean.
#[test]
fn a_fresh_tree_database_is_versioned_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("subagents.sqlite3");
    let store = SubagentsDatabase::open(&database).unwrap();
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    drop(store);
    SubagentsDatabase::open(&database).expect("a current file reopens");
}

#[test]
fn revision_thirteen_backs_up_the_child_summary_store() {
    use crate::admission::{AdmissionError, Schema, Status};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("subagents.sqlite3");
    let previous = Schema {
        revisions: &[super::RETIREMENT_REVISION],
        ..SCHEMA
    };
    let old = previous.open_writer(&path, |_| Ok(())).unwrap();
    let instance = |conn: &rusqlite::Connection| {
        conn.query_row("SELECT instance_id FROM subagents_schema", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
    };
    let prior = instance(&old);
    drop(old);
    assert_eq!(SCHEMA.inspect(&path).unwrap(), Status::Pending(12));
    SCHEMA
        .lock_upgrade(&path)
        .unwrap()
        .unwrap()
        .backup()
        .unwrap()
        .commit(|_, _| {})
        .unwrap();
    let live = SubagentsDatabase::open(&path).unwrap();
    assert_eq!(instance(&live.connection), prior);
    live.connection
        .prepare("SELECT * FROM agent_child_archive_receipts")
        .unwrap();
    let backup_path = root.path().join("subagents.pre-revision-12.sqlite3");
    assert_eq!(previous.inspect(&backup_path).unwrap(), Status::Current);
    assert!(matches!(
        previous.inspect(&path),
        Err(AdmissionError::Unsupported(_))
    ));
}

/// The admission steps are shared with the product store; the tree's
/// refusals keep their exact text.
#[test]
fn tree_admission_refusals_are_exact() {
    let base = SCHEMA.fingerprint();
    let refusal = |damage: &str| {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("subagents.sqlite3");
        if damage != "unversioned" {
            drop(SubagentsDatabase::open(&database).unwrap());
        }
        let connection = rusqlite::Connection::open(&database).unwrap();
        let damage_sql = match damage {
            "unversioned" | "shape" => "CREATE TABLE stray(x)",
            "fingerprint" => "UPDATE subagents_schema SET fingerprint='drift'",
            "missing" => "DELETE FROM subagents_schema",
            _ => "PRAGMA user_version=99",
        };
        connection.execute_batch(damage_sql).unwrap();
        drop(connection);
        let Err(error) = SubagentsDatabase::open(&database) else {
            panic!("a damaged file must be refused");
        };
        (error.kind(), error.message().to_owned())
    };
    let unavailable = super::OpenErrorKind::Unavailable;
    assert_eq!(
        refusal("unversioned"),
        (
            unavailable,
            "unversioned tree store contains unrecognized state; archive subagents.sqlite3, then retry".to_owned()
        )
    );
    assert_eq!(
        refusal("shape"),
        (
            unavailable,
            format!(
                "tree store schema shape does not match its exact revision (expected fingerprint {base}); current-schema tree state is not authoritative"
            )
        )
    );
    assert_eq!(
        refusal("fingerprint"),
        (
            unavailable,
            format!(
                "tree store schema fingerprint does not match its exact revision (expected fingerprint {base}); current-schema tree state is not authoritative"
            )
        )
    );
    assert_eq!(
        refusal("missing"),
        (
            unavailable,
            format!(
                "tree store schema fingerprint is missing (expected fingerprint {base}); current-schema tree state is not authoritative"
            )
        )
    );
    assert_eq!(
        refusal("version"),
        (
            super::OpenErrorKind::UnsupportedPersistence,
            format!(
                "subagents schema 99, expected {SCHEMA_VERSION}: this subagents.sqlite3 was created by an incompatible plakd build; move it aside to start fresh"
            )
        )
    );
}

/// A file at any other version is refused as unsupported, never migrated.
#[test]
fn a_foreign_schema_version_is_refused_as_unsupported() {
    // Earlier cohorts have no lineage rows or custody record.
    for foreign in [4_i64, 5, 6, 7, 8, 9, 10, 99] {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("subagents.sqlite3");
        drop(SubagentsDatabase::open(&database).unwrap());
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .pragma_update(None, "user_version", foreign)
            .unwrap();
        drop(connection);
        let Err(error) = SubagentsDatabase::open(&database) else {
            panic!("a foreign cohort must be refused");
        };
        assert_eq!(error.kind(), super::OpenErrorKind::UnsupportedPersistence);
        assert!(
            error.message().contains(&format!(
                "subagents schema {foreign}, expected {SCHEMA_VERSION}"
            )),
            "{error}"
        );
    }
}
