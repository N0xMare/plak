//! Operator metadata is independent of resident runtime-record projections.

use rusqlite::OptionalExtension as _;

use crate::commands::session::lifecycle::{LifecycleError, LifecycleRefusal};
use plakous_hosted::error::CliFailure;

use super::ProductStore;

#[cfg(test)]
pub(crate) fn crash_cut(point: &str) {
    if std::env::var("PLAK_LIFECYCLE_CRASH_CUT").as_deref() == Ok(point) {
        std::process::exit(91);
    }
}

pub(super) fn admit_work(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<(), super::RecordStoreError> {
    let archived: bool = connection
        .query_row("SELECT archived FROM sessions WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .map_err(|error| super::write_error(error, "could not read session admission"))?;
    if archived {
        return Err("session_archived: unarchive this session before admitting work".into());
    }
    Ok(())
}

impl ProductStore {
    pub(crate) fn lifecycle_metadata(
        &self,
        id: &str,
    ) -> Result<(Option<String>, bool), LifecycleError> {
        self.connection
            .query_row(
                "SELECT title_override,archived FROM sessions WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| CliFailure::new(error.to_string()))?
            .ok_or(LifecycleError::Refused(LifecycleRefusal::NotFound))
    }

    pub(crate) fn set_title_override(
        &mut self,
        id: &str,
        title: Option<&str>,
    ) -> Result<(), LifecycleError> {
        self.write_lifecycle(id, title, None, &|| Ok(()))
    }

    pub(crate) fn set_archived(&mut self, id: &str, archived: bool) -> Result<(), LifecycleError> {
        self.write_lifecycle(id, None, Some(archived), &|| Ok(()))
    }

    pub(crate) fn set_archived_checked(
        &mut self,
        id: &str,
        archived: bool,
        verify: &dyn Fn() -> Result<(), LifecycleError>,
    ) -> Result<(), LifecycleError> {
        self.write_lifecycle(id, None, Some(archived), verify)
    }

    fn write_lifecycle(
        &mut self,
        id: &str,
        title: Option<&str>,
        archived: Option<bool>,
        verify: &dyn Fn() -> Result<(), LifecycleError>,
    ) -> Result<(), LifecycleError> {
        self.durability(true).map_err(CliFailure::new)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let revision: Option<i64> = transaction
            .query_row("SELECT revision FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let revision = revision.ok_or(LifecycleError::Refused(LifecycleRefusal::NotFound))?;
        // Runtime CAS revisions fence runtime projections, not these disjoint
        // operator columns. Keeping that token avoids aborting a busy turn's
        // next save; every runtime UPDATE deliberately excludes both columns.
        let changed = match archived {
            Some(archived) => transaction.execute(
                "UPDATE sessions SET archived=?2 WHERE id=?1 AND revision=?3",
                (id, archived, revision),
            ),
            None => transaction.execute(
                "UPDATE sessions SET title_override=?2 WHERE id=?1 AND revision=?3",
                (id, title, revision),
            ),
        }
        .map_err(|error| CliFailure::new(error.to_string()))?;
        if changed != 1 {
            return Err(CliFailure::new("session changed concurrently").into());
        }
        #[cfg(test)]
        crash_cut("metadata_before_commit");
        #[cfg(test)]
        if let Ok(milliseconds) = std::env::var("PLAK_LIFECYCLE_COMMIT_PAUSE_MS") {
            let milliseconds = milliseconds
                .parse::<u64>()
                .map_err(|_| CliFailure::new("invalid commit pause fixture"))?;
            std::thread::sleep(std::time::Duration::from_millis(milliseconds));
        }
        verify()?;
        transaction
            .commit()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        #[cfg(test)]
        crash_cut("metadata_after_commit");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "lifecycle store contract tests")]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;
    use crate::store::sessions::SessionRecord;

    #[test]
    fn archived_read_only_inspection_never_reads_runtime_or_drains_work() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let mut store = ProductStore::open(&paths).unwrap();
        let mut record = SessionRecord::new(paths.home().to_owned());
        record.archived = true;
        record.title = Some("generated".into());
        record.title_override = Some("operator".into());
        record
            .pending_queue
            .push(crate::store::sessions::QueueItem::new("do not run".into()));
        store.commit(&mut record).unwrap();
        let revision = record.store_revision;
        std::fs::write(paths.runtime_database(), b"deliberately unreadable runtime").unwrap();
        let inspected = ProductStore::inspect_session_read_only(&paths, &record.id).unwrap();
        assert!(inspected.archived);
        assert_eq!(inspected.title_override.as_deref(), Some("operator"));
        assert_eq!(inspected.pending_queue, record.pending_queue);
        assert_eq!(inspected.store_revision, revision);
        assert_eq!(
            std::fs::read(paths.runtime_database()).unwrap(),
            b"deliberately unreadable runtime"
        );
    }

    #[test]
    fn concurrent_renames_and_resident_save_share_the_revision_fence() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let mut store = ProductStore::open(&paths).unwrap();
        let mut resident = SessionRecord::new(paths.home().to_owned());
        store.commit(&mut resident).unwrap();
        let before_revision = resident.store_revision;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let writers: Vec<_> = ["first label", "second label"]
            .into_iter()
            .map(|title| {
                let paths = paths.clone();
                let id = resident.id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut store = ProductStore::open(&paths).unwrap();
                    barrier.wait();
                    store.set_title_override(&id, Some(title)).unwrap();
                })
            })
            .collect();
        barrier.wait();
        resident.title = Some("resident generated title".into());
        store.commit(&mut resident).unwrap();
        for writer in writers {
            writer.join().unwrap();
        }
        let loaded = store.load(&resident.id).unwrap();
        assert_eq!(loaded.store_revision, before_revision + 1);
        assert_eq!(loaded.title.as_deref(), Some("resident generated title"));
        assert!(matches!(
            loaded.title_override.as_deref(),
            Some("first label" | "second label")
        ));
    }

    #[test]
    fn metadata_survives_stale_resident_projection_and_restart() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let mut store = ProductStore::open(&paths).unwrap();
        let mut record = SessionRecord::new(paths.home().to_owned());
        store.commit(&mut record).unwrap();
        let mut metadata = ProductStore::open(&paths).unwrap();
        let busy_revision = record.store_revision;
        metadata
            .set_title_override(&record.id, Some("chosen"))
            .unwrap();
        assert_eq!(
            store.load(&record.id).unwrap().store_revision,
            busy_revision
        );
        metadata.set_archived(&record.id, true).unwrap();
        record.title = Some("generated during turn".into());
        store.commit(&mut record).unwrap();
        assert_eq!(record.title_override.as_deref(), Some("chosen"));
        assert!(record.archived);
        drop(store);
        drop(metadata);
        let mut store = ProductStore::open(&paths).unwrap();
        let loaded = store.load(&record.id).unwrap();
        assert_eq!(loaded.title_override.as_deref(), Some("chosen"));
        assert!(loaded.archived);
        assert!(admit_work(&store.connection, &record.id).is_err());
        store.set_archived(&record.id, false).unwrap();
        admit_work(&store.connection, &record.id).unwrap();
    }

    #[test]
    fn directory_archive_filter_precedes_row_limit() {
        let (_home, paths) = PlakPaths::isolated_test_home();
        let mut store = ProductStore::open(&paths).unwrap();
        let mut visible = SessionRecord::new(paths.home().to_owned());
        visible.updated_at_unix = 1;
        store.commit(&mut visible).unwrap();
        for _ in 0..super::super::directory::MAX_DIRECTORY_ROWS {
            let mut hidden = SessionRecord::new(paths.home().to_owned());
            hidden.archived = true;
            store.commit(&mut hidden).unwrap();
        }
        assert_eq!(
            store
                .directory(None)
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [visible.id]
        );
    }

    #[test]
    fn lifecycle_crash_worker() {
        let Ok(home) = std::env::var("PLAK_LIFECYCLE_CRASH_HOME") else {
            return;
        };
        let paths = PlakPaths::explicit(home.into());
        let id = std::env::var("PLAK_LIFECYCLE_CRASH_SESSION").unwrap();
        let operation = std::env::var("PLAK_LIFECYCLE_CRASH_OPERATION").unwrap();
        let mut store = ProductStore::open(&paths).unwrap();
        match operation.as_str() {
            "rename" => store
                .set_title_override(&id, Some("committed title"))
                .unwrap(),
            "archive" => store.set_archived(&id, true).unwrap(),
            "delete" => {
                let revision = store.load(&id).unwrap().store_revision;
                store.delete_session(&id, revision).unwrap();
            }
            _ => panic!("unknown crash operation"),
        }
        panic!("crash hook did not run");
    }

    #[test]
    fn lifecycle_commit_crash_cuts_reopen_exactly_before_or_after() {
        for operation in ["rename", "archive", "delete"] {
            for after in [false, true] {
                let (_home, paths) = PlakPaths::isolated_test_home();
                let mut store = ProductStore::open(&paths).unwrap();
                let mut record = SessionRecord::new(paths.home().to_owned());
                store.commit(&mut record).unwrap();
                drop(store);
                let point = match (operation, after) {
                    ("delete", false) => "delete_before_commit",
                    ("delete", true) => "delete_after_commit",
                    (_, false) => "metadata_before_commit",
                    (_, true) => "metadata_after_commit",
                };
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "store::lifecycle::tests::lifecycle_crash_worker",
                        "--nocapture",
                    ])
                    .env("PLAK_LIFECYCLE_CRASH_HOME", paths.home())
                    .env("PLAK_LIFECYCLE_CRASH_SESSION", &record.id)
                    .env("PLAK_LIFECYCLE_CRASH_OPERATION", operation)
                    .env("PLAK_LIFECYCLE_CRASH_CUT", point)
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(91),
                    "{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                let mut store = ProductStore::open(&paths).unwrap();
                let loaded = store.load(&record.id);
                if operation == "delete" && after {
                    assert!(loaded.is_err());
                    continue;
                }
                let loaded = loaded.unwrap();
                assert_eq!(
                    loaded.title_override.as_deref(),
                    (operation == "rename" && after).then_some("committed title")
                );
                assert_eq!(loaded.archived, operation == "archive" && after);
            }
        }
    }
}
