//! Complete, byte-stable snapshots of the product's `SQLite` views.

use std::collections::BTreeMap;

use plak::home::paths::PlakPaths;
use rusqlite::{Connection, OpenFlags, types::ValueRef};

const NON_VIEWS: &[&str] = &[
    "agent_acknowledgements",
    "host_intents",
    "identity_bindings",
    "operator_notices",
    "product_schema",
    "prompt_history",
    "queue_items",
    "sessions",
    "submissions",
    "workflow_requests",
    "workflows",
];

pub(super) const VIEW_TABLES: &[&str] = &[
    "projector_cursors",
    "session_call_views",
    "session_open_turn_views",
    "session_process_views",
    "session_views",
    "transcript_rows",
];

pub(super) fn snapshot(paths: &PlakPaths) -> Vec<u8> {
    let connection =
        Connection::open_with_flags(paths.product_database(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ).unwrap();
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .filter(|name| !NON_VIEWS.contains(&name.as_str()))
        .collect::<Vec<_>>();
    // Classify the entire live schema, not just tables matching today's naming convention.
    assert_eq!(
        tables, VIEW_TABLES,
        "unclassified product table: update the view inventory"
    );
    let mut snapshot = BTreeMap::new();
    for table in tables {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|column| {
                        Ok(match row.get_ref(column)? {
                            ValueRef::Null => serde_json::json!(["null"]),
                            ValueRef::Integer(value) => serde_json::json!(["integer", value]),
                            ValueRef::Real(value) => serde_json::json!(["real", value.to_bits()]),
                            ValueRef::Text(value) => serde_json::json!(["text", value]),
                            ValueRef::Blob(value) => serde_json::json!(["blob", value]),
                        })
                    })
                    .collect::<Result<Vec<_>, rusqlite::Error>>()
            })
            .unwrap()
            .map(|row| serde_json::to_vec(&row.unwrap()).unwrap())
            .collect::<Vec<_>>();
        rows.sort();
        snapshot.insert(table, rows);
    }
    serde_json::to_vec(&snapshot).unwrap()
}

pub(super) fn explain_difference(before: &[u8], after: &[u8]) {
    let decode =
        |bytes: &[u8]| serde_json::from_slice::<BTreeMap<String, Vec<Vec<u8>>>>(bytes).unwrap();
    let before = decode(before);
    let after = decode(after);
    for (table, rows) in &before {
        for (side, changed) in [("before", rows), ("after", &after[table])] {
            let other = if side == "before" {
                &after[table]
            } else {
                rows
            };
            for row in changed.iter().filter(|row| !other.contains(row)) {
                let mut cells: serde_json::Value = serde_json::from_slice(row).unwrap();
                for cell in cells.as_array_mut().unwrap() {
                    if cell[0] == "text" {
                        let bytes: Vec<u8> = serde_json::from_value(cell[1].clone()).unwrap();
                        cell[1] = serde_json::Value::String(String::from_utf8(bytes).unwrap());
                    }
                }
                eprintln!("VIEW_DIFF {table} {side}: {cells}");
            }
        }
    }
}
