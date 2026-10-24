use super::*;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn deadline_codec_preserves_domain_and_refuses_old_or_malformed_bytes() {
    let domain = LeaseDomain::new([0; 16], [0; 32]);
    let deadline = LeaseInstant::new(domain, u64::MAX, 999_999_999).unwrap();
    let bytes = lease_time_bytes(deadline);
    assert_eq!(lease_time(&bytes, 999_999_999).unwrap(), deadline);
    assert_eq!(lease_time(&u64b(50), 0), Err(SessionStoreError::corrupt()));
    assert_eq!(lease_time(&bytes, -1), Err(SessionStoreError::corrupt()));
    assert_eq!(
        lease_time(&bytes, 1_000_000_000),
        Err(SessionStoreError::corrupt())
    );
    let foreign =
        LeaseInstant::new(LeaseDomain::new([1; 16], [0; 32]), u64::MAX, 999_999_999).unwrap();
    assert_ne!(lease_instant_bytes(deadline), lease_instant_bytes(foreign));
    let foreign =
        LeaseInstant::new(LeaseDomain::new([0; 16], [1; 32]), u64::MAX, 999_999_999).unwrap();
    assert_ne!(lease_instant_bytes(deadline), lease_instant_bytes(foreign));
}

fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "plakous-store-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("runtime.sqlite3");
    (root, database)
}

#[test]
fn current_shape_is_explicitly_non_authoritative() {
    let (root, database) = fixture("current-shape");
    drop(SqliteSessionStore::open(&database).unwrap());
    let connection = Connection::open(&database).unwrap();
    connection
        .execute("UPDATE store_meta SET sessions=1 WHERE id=1", [])
        .unwrap();
    drop(connection);

    assert_eq!(
        SqliteSessionStore::classify_persistence(&database).unwrap(),
        SqlitePersistenceShape::Current
    );
    assert!(matches!(
        SqliteSessionStore::open(&database),
        Err(SessionStoreError::Corrupt(_))
    ));
    fs::remove_dir_all(root).unwrap();
}
