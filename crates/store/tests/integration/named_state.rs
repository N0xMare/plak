#![cfg(feature = "sqlite")]

use std::num::NonZeroU64;

use plakous_agent::internal::{
    CreateSession, NamedState, RuntimeCheckpoint, RuntimeDurability, RuntimeManifest,
    RuntimeObjectKind, SessionStore, Timestamp,
};
use plakous_store::SqliteSessionStore;

use super::{
    conformance,
    sqlite::{Database, seed},
};

#[tokio::test]
async fn sqlite_named_state_reopens_exactly_and_corrupt_or_missing_values_fail_closed()
-> conformance::TestResult {
    let db = Database::new();
    let seed = seed(0x5a7e_3000, RuntimeDurability::Required, 1)?;
    let checkpoint = seed.bundle.validated_checkpoint()?;
    let prior = checkpoint.manifest();
    let state = NamedState::default()
        .put(
            "saved".to_owned(),
            &serde_json::json!({"z": [1, true, null], "a": "exact"}),
        )?
        .put(
            "same_bytes".to_owned(),
            &serde_json::json!({"a": "exact", "z": [1, true, null]}),
        )?
        .put("null_value".to_owned(), &serde_json::Value::Null)?;
    let checkpoint = RuntimeCheckpoint::new(RuntimeManifest::new(
        prior.provenance().clone(),
        prior.topology(),
        prior.lifecycle().clone(),
        prior.harness_plan_component().clone(),
        prior.provider_configuration().clone(),
        prior.canonical().clone(),
        prior.binding().clone(),
        prior.tools().clone(),
        prior.ledger().clone(),
        prior.family_artifact().cloned(),
        prior.interpreter_observation().cloned(),
        prior.jobs().clone(),
        state.clone(),
        prior.durability(),
    )?)?;
    let bundle = conformance::bundle(&checkpoint, None)?;
    assert_eq!(
        bundle
            .objects()
            .iter()
            .filter(|object| object.address().kind() == RuntimeObjectKind::NamedStateValue)
            .count(),
        2,
        "equal JSON values share one immutable object, not mutable authority"
    );
    let store = SqliteSessionStore::open(&db.path)?;
    let created = store
        .create(CreateSession::new(
            seed.selector,
            Timestamp::new(50, 0)?,
            conformance::id(0x5a7e_3010)?,
            NonZeroU64::new(3600).ok_or("lease")?,
            bundle,
        ))
        .await?;
    store.release(created.lease().clone()).await?;
    store.flush().await?;
    drop(store);
    let reopened = SqliteSessionStore::open_verified(&db.path)?;
    let loaded = reopened.load_checkpoint(seed.checkpoint_id).await?;
    assert_eq!(loaded.checkpoint().manifest().named_state(), &state);
    assert_eq!(loaded.checkpoint().integrity(), checkpoint.integrity());
    drop(loaded);
    drop(reopened);
    let connection = rusqlite::Connection::open(&db.path)?;
    let (value_id, original): (i64, Vec<u8>) = connection.query_row(
        "SELECT id,bytes FROM runtime_object_bytes WHERE id=(SELECT id FROM runtime_objects WHERE kind=20 LIMIT 1)", [],
        |row| Ok((row.get(0)?, row.get(1)?)))?;
    connection.execute("UPDATE runtime_object_bytes SET bytes=?1 WHERE id=(SELECT id FROM runtime_objects WHERE kind=20 LIMIT 1)",
        [b"corrupt".as_slice()])?;
    drop(connection);
    assert!(
        SqliteSessionStore::open_verified(&db.path).is_err(),
        "state corruption must not become an empty map"
    );
    let connection = rusqlite::Connection::open(&db.path)?;
    connection.execute(
        "UPDATE runtime_object_bytes SET bytes=?1 WHERE id=?2",
        rusqlite::params![original, value_id],
    )?;
    drop(connection);
    drop(SqliteSessionStore::open_verified(&db.path)?);
    let connection = rusqlite::Connection::open(&db.path)?;
    connection.execute("DELETE FROM runtime_object_bytes WHERE id=?1", [value_id])?;
    drop(connection);
    assert!(
        SqliteSessionStore::open_verified(&db.path).is_err(),
        "missing state must not become an empty map"
    );
    Ok(())
}
