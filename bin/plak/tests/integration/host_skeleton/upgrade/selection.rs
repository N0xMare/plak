//! Archived heads do not consume the carried-root capacity or require inspection.

use super::*;

#[test]
fn host_upgrade_archived_heads_do_not_consume_the_carried_root_limit() {
    let Some(fixture) = fixture() else { return };
    let _owner = HomeHost(fixture.home());
    let session = seed(&fixture, 61);
    let mut source = start(&fixture);
    let paths = plak::home::paths::PlakPaths::explicit(fixture.home());
    let mut product = rusqlite::Connection::open(paths.product_database()).unwrap();
    product.busy_timeout(Duration::from_secs(5)).unwrap();
    let columns = product
        .prepare("PRAGMA table_info(sessions)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let expressions = columns
        .iter()
        .map(|column| match column.as_str() {
            "id" | "create_operation_id" | "node_tree_id" | "node_agent_id" => "?1",
            "archived" => "1",
            _ => column.as_str(),
        })
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO sessions ({}) SELECT {expressions} FROM sessions WHERE id=?2",
        columns.join(",")
    );
    let transaction = product.transaction().unwrap();
    for _ in 0..4097 {
        transaction
            .execute(
                &sql,
                rusqlite::params![uuid::Uuid::new_v4().to_string(), session],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(product);
    let upgraded = upgrade(&fixture, &binary(&fixture));
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let operation = receipts(&fixture).pop().unwrap();
    assert_eq!(operation["mappings"].as_array().unwrap().len(), 1);
    assert_eq!(operation["mappings"][0]["source"], session);
    stop(&mut source);
}
