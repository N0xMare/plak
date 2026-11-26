use std::path::PathBuf;

use plakous::sessions::SessionLocator;
use plakous_hosted::sessions::projector::{RootInterrupt, RootInterruptCause};
use rusqlite::{OptionalExtension as _, params};

use super::{ProductStore, ROOT_INTERRUPT, from_i64, invalid, unavailable};

pub(crate) struct RunnerStopRow {
    pub(crate) session_id: String,
    pub(crate) tree_id: String,
    pub(crate) root_stop: Option<(SessionLocator, u64, u32)>,
}

pub(crate) struct RunnerPlacementRow {
    pub(crate) tree_id: String,
    pub(crate) runner_id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) updated_at: u64,
    pub(crate) recorded_stop: bool,
}

impl ProductStore {
    /// Occupancy and pin recovery need neither queues nor presentation rows.
    pub(crate) fn runner_placement_rows(&self) -> Result<Vec<RunnerPlacementRow>, String> {
        #[cfg(test)]
        runner_read_threads::record(self.paths.product_database());
        self.connection
            .prepare(
                "SELECT node_tree_id,runner_display,workspace_root,updated_at,
                        EXISTS(SELECT 1 FROM host_intents i WHERE i.session_id=s.id
                               AND i.kind=?1 AND json_extract(i.value,'$.cause')='stopped')
                 FROM sessions s ORDER BY id",
            )
            .map_err(unavailable)?
            .query_map([ROOT_INTERRUPT], |row| {
                Ok(RunnerPlacementRow {
                    tree_id: row.get(0)?,
                    runner_id: super::super::decode_runner_display(row, 1)?
                        .map_or_else(|| "local".to_owned(), |runner| runner.runner_id),
                    workspace: PathBuf::from(row.get::<_, String>(2)?),
                    updated_at: from_i64(row.get(3)?)?,
                    recorded_stop: row.get(4)?,
                })
            })
            .map_err(unavailable)?
            .collect::<Result<_, _>>()
            .map_err(invalid)
    }

    /// Read stop evidence and its projected binding in the same snapshot.
    pub(crate) fn runner_stop_rows(&self) -> Result<Vec<RunnerStopRow>, String> {
        #[cfg(test)]
        runner_read_threads::record(self.paths.product_database());
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        let mut sessions = tx
            .prepare("SELECT id,node_tree_id FROM sessions ORDER BY id")
            .map_err(unavailable)?;
        let mut latest = tx
            .prepare(
                "SELECT anchor,ordinal,value FROM host_intents
                 WHERE session_id=?1 AND kind=?2 ORDER BY anchor DESC,ordinal DESC LIMIT 1",
            )
            .map_err(unavailable)?;
        let mut binding = tx
            .prepare(
                "SELECT s.locator,v.head_locator FROM sessions s
                 LEFT JOIN session_views v ON v.session_id=s.id WHERE s.id=?1",
            )
            .map_err(unavailable)?;
        let mut superseded = tx
            .prepare(
                "SELECT EXISTS(
                     SELECT 1 FROM session_views v
                     CROSS JOIN json_each(v.state,'$.deliveries') d
                     CROSS JOIN submissions s ON s.command_id=d.key
                     WHERE v.session_id=?1 AND s.session_id=v.session_id
                     AND s.anchor>=?2 AND d.value='accepted'
                 )",
            )
            .map_err(unavailable)?;
        let rows = sessions
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(unavailable)?;
        rows.map(|row| {
            let (session_id, tree_id) = row.map_err(invalid)?;
            let stop = latest
                .query_row(params![session_id, ROOT_INTERRUPT], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .optional()
                .map_err(invalid)?;
            let root_stop = if let Some((anchor, ordinal, value)) = stop
                && RootInterrupt::decode(&value)?.cause == RootInterruptCause::Stopped
                && !superseded
                    .query_row(params![session_id, anchor], |row| row.get::<_, bool>(0))
                    .map_err(invalid)?
            {
                let (recorded, projected) = binding
                    .query_row([&session_id], |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    })
                    .map_err(invalid)?;
                let recorded = parse_locator(recorded.as_deref())?;
                let projected = parse_locator(projected.as_deref())?.filter(|head| {
                    recorded.is_none_or(|recorded| {
                        head.session_id() == recorded.session_id()
                            && head.branch_id() == recorded.branch_id()
                    })
                });
                let anchor = from_i64(anchor).map_err(invalid)?;
                projected
                    .or(recorded)
                    .map(|locator| (locator, anchor, ordinal))
            } else {
                None
            };
            Ok(RunnerStopRow {
                session_id,
                tree_id,
                root_stop,
            })
        })
        .collect()
    }
}

fn parse_locator(locator: Option<&str>) -> Result<Option<SessionLocator>, String> {
    locator
        .map(str::parse)
        .transpose()
        .map_err(|_| "product session locator is invalid".to_owned())
}

#[cfg(test)]
pub(crate) mod runner_read_threads {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, PoisonError};
    use std::thread::{self, ThreadId};

    static READS: Mutex<Vec<(PathBuf, ThreadId)>> = Mutex::new(Vec::new());

    pub(super) fn record(path: PathBuf) {
        READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((path, thread::current().id()));
    }

    pub(crate) fn take(path: &Path) -> Vec<ThreadId> {
        let mut observed = Vec::new();
        READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(database, worker)| {
                if database == path {
                    observed.push(*worker);
                    false
                } else {
                    true
                }
            });
        observed
    }
}
