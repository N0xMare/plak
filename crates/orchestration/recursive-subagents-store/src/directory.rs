use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

use crate::{OpenError, database::schema};

type Images = BTreeMap<String, u64>;
type Custody = BTreeMap<String, (i64, String, String, Option<String>, String, bool)>;

/// Changed tree revisions and custody placement facts since the previous inspection.
#[derive(Default)]
pub struct TreeDirectoryChanges {
    /// Keys whose image was saved or deleted.
    pub images: Vec<String>,
    /// Keys whose custody was acquired, transferred or released.
    pub custody: Vec<String>,
}

/// Read-only observation of directory facts; never opens an agent tree image.
pub struct TreeDirectoryProbe {
    connection: Connection,
    version: i64,
    images: Images,
    custody: Custody,
}

impl TreeDirectoryProbe {
    /// Opens an existing admitted tree store and captures its narrow fingerprints.
    pub fn open(path: &Path) -> Result<Self, OpenError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(unavailable)?;
        connection
            .busy_timeout(std::time::Duration::from_millis(250))
            .map_err(unavailable)?;
        schema().require_current(&connection)?;
        let version = version(&connection)?;
        let (images, custody) = fingerprints(&connection)?;
        Ok(Self {
            connection,
            version,
            images,
            custody,
        })
    }

    /// Refreshes only on a data-version change on this connection.
    pub fn changes(&mut self) -> Result<TreeDirectoryChanges, OpenError> {
        let version = version(&self.connection)?;
        if self.version == version {
            return Ok(TreeDirectoryChanges::default());
        }
        let (images, custody) = fingerprints(&self.connection)?;
        let out = TreeDirectoryChanges {
            images: changed(&self.images, &images),
            custody: changed(&self.custody, &custody),
        };
        self.images = images;
        self.custody = custody;
        self.version = version;
        Ok(out)
    }

    /// Checks one advisory image before publishing it, without loading the tree.
    pub fn revision(&self, tree_id: &str) -> Result<Option<u64>, OpenError> {
        self.connection
            .query_row(
                "SELECT revision FROM agent_trees WHERE tree_id=?1",
                [tree_id],
                |row| crate::from_i64(row.get(0)?),
            )
            .optional()
            .map_err(unavailable)
    }

    /// Records an image revision already observed by a presentation caller.
    pub fn note_image(&mut self, id: &str, revision: Option<u64>) {
        if let Some(revision) = revision {
            self.images.insert(id.into(), revision);
        } else {
            self.images.remove(id);
        }
    }
}

fn changed<T: Eq>(old: &BTreeMap<String, T>, new: &BTreeMap<String, T>) -> Vec<String> {
    old.keys()
        .filter(|id| !new.contains_key(*id))
        .chain(
            new.iter()
                .filter(|(id, stamp)| old.get(*id) != Some(*stamp))
                .map(|(id, _)| id),
        )
        .cloned()
        .collect()
}

fn version(connection: &Connection) -> Result<i64, OpenError> {
    connection
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .map_err(unavailable)
}

fn fingerprints(connection: &Connection) -> Result<(Images, Custody), OpenError> {
    let tx = connection.unchecked_transaction().map_err(unavailable)?;
    let images = tx
        .prepare("SELECT tree_id,revision FROM agent_trees")
        .map_err(unavailable)?
        .query_map([], |row| Ok((row.get(0)?, crate::from_i64(row.get(1)?)?)))
        .map_err(unavailable)?
        .collect::<Result<_, _>>()
        .map_err(unavailable)?;
    let custody = tx.prepare(
        "SELECT tree_id,fence,host_id,host_instance,host_binary,host_name,released FROM agent_tree_custody",
    ).map_err(unavailable)?
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))))
        .map_err(unavailable)?.collect::<Result<_, _>>().map_err(unavailable)?;
    Ok((images, custody))
}

fn unavailable(_: rusqlite::Error) -> OpenError {
    OpenError::unavailable("could not inspect agent tree directory fingerprints")
}
