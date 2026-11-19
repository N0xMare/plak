#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;

const BASE: Schema = Schema {
    noun: "sample",
    stem: "sample",
    archive: "sample.sqlite3",
    generation: 11,
    objects: &[
        (
            "sample_schema",
            "CREATE TABLE sample_schema(singleton INTEGER PRIMARY KEY,instance_id TEXT NOT NULL,fingerprint TEXT NOT NULL)",
        ),
        (
            "notes",
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, note TEXT NOT NULL)",
        ),
    ],
    base_fingerprint: "0e1a32ac85edc70c409e10899f90292254a5f1c3f46069e23e90ca922fae67be",
    revisions: &[],
};

const CURRENT: Schema = Schema {
    revisions: &[
        Revision {
            sql: "ALTER TABLE notes ADD COLUMN flag INTEGER NOT NULL DEFAULT 0",
            objects: &[(
                "notes",
                "CREATE TABLE notes(id INTEGER PRIMARY KEY, note TEXT NOT NULL, flag INTEGER NOT NULL DEFAULT 0)",
            )],
            fingerprint: "382f9e1134e26c0465eec07735dad09516eff864045b9102ccf9ca25c6c3a1d6",
        },
        Revision {
            sql: "CREATE INDEX notes_flag ON notes(flag)",
            objects: &[("notes_flag", "CREATE INDEX notes_flag ON notes(flag)")],
            fingerprint: "3b5012aef51175dbd0e51c59c05c43f28df9c87d4ca0a0e785d45fed5065d018",
        },
    ],
    ..BASE
};

fn seed(path: &Path) {
    BASE.open_writer(path, |_| Ok(()))
        .unwrap()
        .execute("INSERT INTO notes VALUES (1,'retained')", [])
        .unwrap();
}

#[test]
fn fresh_and_upgraded_shapes_match_and_old_rows_get_the_old_behavior() {
    let home = tempfile::tempdir().unwrap();
    let upgraded_path = home.path().join("sample.sqlite3");
    seed(&upgraded_path);
    let fresh = CURRENT
        .open_writer(&home.path().join("fresh.sqlite3"), |_| Ok(()))
        .unwrap();
    CURRENT
        .lock_upgrade(&upgraded_path)
        .unwrap()
        .unwrap()
        .backup()
        .unwrap()
        .commit(|_, _| {})
        .unwrap();
    let upgraded = CURRENT.open_writer(&upgraded_path, |_| Ok(())).unwrap();
    assert_eq!(
        CURRENT.read_shape(&upgraded).unwrap(),
        CURRENT.read_shape(&fresh).unwrap()
    );
    for connection in [&upgraded, &fresh] {
        CURRENT.validate(connection).unwrap();
    }
    assert_eq!(
        upgraded
            .query_row("SELECT note,flag FROM notes WHERE id=1", [], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .unwrap(),
        ("retained".to_owned(), 0)
    );
    let backup = home.path().join("sample.pre-revision-11.sqlite3");
    assert_eq!(BASE.inspect(&backup).unwrap(), Status::Current);
    let connection = BASE.open_reader(&backup).unwrap().unwrap();
    assert_eq!(
        connection
            .query_row("SELECT note FROM notes WHERE id=1", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "retained"
    );
    assert!(CURRENT.lock_upgrade(&upgraded_path).unwrap().is_none());
}

#[test]
fn read_only_and_ordinary_writer_refuse_pending_and_foreign_files_unchanged() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("sample.sqlite3");
    let writer = BASE.open_writer(&path, |_| Ok(())).unwrap();
    writer
        .execute("INSERT INTO notes VALUES (1,'retained')", [])
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let wal = path.with_extension("sqlite3-wal");
    let before_wal = std::fs::read(&wal).unwrap();
    assert_ne!(before_wal, [] as [u8; 0]);
    {
        let reader = CURRENT.open_reader(&path).unwrap().unwrap();
        assert!(matches!(
            CURRENT.require_current(&reader),
            Err(AdmissionError::UpgradePending(_))
        ));
    }
    assert!(matches!(
        CURRENT.open_writer(&path, |_| Ok(())),
        Err(AdmissionError::UpgradePending(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
    drop(writer);

    for foreign in [10_i64, 14] {
        let writer = Connection::open(&path).unwrap();
        // Changing to WAL before checking user_version must not pass this test.
        writer.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
        writer.pragma_update(None, "user_version", foreign).unwrap();
        drop(writer);
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            CURRENT.open_writer(&path, |_| Ok(())),
            Err(AdmissionError::Unsupported(_))
        ));
        assert!(matches!(
            CURRENT.lock_upgrade(&path),
            Err(AdmissionError::Unsupported(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn a_locked_store_or_failed_copy_cannot_commit_a_revision() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("sample.sqlite3");
    seed(&path);
    let writer = BASE.open_writer(&path, |_| Ok(())).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE notes SET note='busy';")
        .unwrap();
    assert!(matches!(
        CURRENT.lock_upgrade(&path),
        Err(AdmissionError::Busy(_))
    ));
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);

    let before = std::fs::read(&path).unwrap();
    let backup = home.path().join("sample.pre-revision-11.sqlite3");
    std::fs::create_dir(backup).unwrap();
    assert!(matches!(
        CURRENT.lock_upgrade(&path).unwrap().unwrap().backup(),
        Err(AdmissionError::Unavailable(_))
    ));
    assert_eq!(BASE.inspect(&path).unwrap(), Status::Current);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn admission_resolves_parent_aliases_but_never_follows_a_linked_database() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("real");
    std::fs::create_dir(&dir).unwrap();
    let alias = home.path().join("alias");
    std::os::unix::fs::symlink(&dir, &alias).unwrap();
    let path = alias.join("sample.sqlite3");
    seed(&path);
    assert_eq!(BASE.inspect(&path).unwrap(), Status::Current);
    let linked = alias.join("link.sqlite3");
    std::os::unix::fs::symlink(&path, &linked).unwrap();
    assert!(matches!(
        BASE.inspect(&linked),
        Err(AdmissionError::Unavailable(_))
    ));
}

#[test]
fn real_tree_registry_and_each_synthetic_revision_converge() {
    for schema in [crate::database::schema(), CURRENT] {
        let directory = tempfile::tempdir().unwrap();
        test_registry::check_revision_paths(&schema, directory.path()).unwrap();
    }
}

#[test]
fn revision_lock_retries_a_transient_wal_reader() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sample.sqlite3");
    seed(&path);
    let reader = BASE.open_reader(&path).unwrap().unwrap();
    reader.execute_batch("BEGIN; SELECT * FROM notes;").unwrap();
    assert!(matches!(
        CURRENT.try_lock_upgrade(&path),
        Err(AdmissionError::Busy(_))
    ));
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(reader);
    });
    let locked = CURRENT.lock_upgrade(&path).unwrap().unwrap();
    release.join().unwrap();
    locked.backup().unwrap().commit(|_, _| {}).unwrap();
}
