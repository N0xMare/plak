use super::{SCHEMA_OBJECTS, SCHEMA_VERSION, SubagentsDatabase, expected_schema_fingerprint};

/// Identity golden for the unreleased t10 cohort's durable format.
#[test]
fn the_tree_schema_fingerprint_is_pinned() {
    assert_eq!(SCHEMA_VERSION, 10);
    assert_eq!(
        expected_schema_fingerprint(),
        "aad61f5a559e15c43774eda2aee30ea040e1e09226121ab75c092d396fac480c"
    );
}

/// The seventeen objects, in the order the fingerprint is taken over.
#[test]
fn the_tree_schema_holds_its_own_identity_row_and_the_agent_tables() {
    let names: Vec<&str> = SCHEMA_OBJECTS.iter().map(|(name, _)| *name).collect();
    assert_eq!(names[0], "subagents_schema", "identity row comes first");
    assert!(
        names[1..].iter().all(|name| name.starts_with("agent_")),
        "{names:?}"
    );
    assert_eq!(names.len(), 17);
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

/// The admission steps are shared with the product store; the tree's
/// refusals keep their exact text.
#[test]
fn tree_admission_refusals_are_exact() {
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
            "tree store schema shape does not match its exact revision; current-schema tree state is not authoritative".to_owned()
        )
    );
    assert_eq!(
        refusal("fingerprint"),
        (
            unavailable,
            "tree store schema fingerprint does not match its exact revision; current-schema tree state is not authoritative".to_owned()
        )
    );
    assert_eq!(
        refusal("missing"),
        (
            unavailable,
            "tree store schema fingerprint is missing; current-schema tree state is not authoritative".to_owned()
        )
    );
    assert_eq!(
        refusal("version"),
        (
            super::OpenErrorKind::UnsupportedPersistence,
            "subagents schema 99, expected 10: this subagents.sqlite3 was created by an incompatible plak build; move it aside to start fresh".to_owned()
        )
    );
}

/// A file at any other version is refused as unsupported, never migrated.
#[test]
fn a_foreign_schema_version_is_refused_as_unsupported() {
    // Earlier cohorts have no lineage rows or custody record.
    for foreign in [4_i64, 5, 6, 7, 8, 9, 99] {
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
            error
                .message()
                .contains(&format!("subagents schema {foreign}, expected 10")),
            "{error}"
        );
    }
}
