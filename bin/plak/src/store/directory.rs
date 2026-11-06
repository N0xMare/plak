//! What a host's session directory reads of the product store (spec 02
//! §4.4): every session's identity and labels, newest first, and the newest
//! session of a tree. Lean on purpose: no transcript previews, so a host can
//! read it every second while a client watches.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension as _;

use super::{ProductStore, decode_node, decode_root_profile, from_i64, sessions};

/// The most rows one directory read returns.
pub const MAX_DIRECTORY_ROWS: i64 = 256;

/// One session as the directory reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub id: String,
    pub workspace_root: PathBuf,
    pub updated_at_unix: u64,
    pub title: Option<String>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub node: plakous_hosted::sessions::SessionNode,
    pub root_profile: Option<plakous_hosted::sessions::RootProfile>,
}

impl ProductStore {
    /// The newest sessions, at most [`MAX_DIRECTORY_ROWS`], of `workspace`
    /// when given, else of the whole home.
    ///
    /// # Errors
    ///
    /// The read failed or a row does not decode.
    pub fn directory(&mut self, workspace: Option<&Path>) -> Result<Vec<DirectoryEntry>, String> {
        let identity = workspace.map(|workspace| {
            sessions::workspace_identity(workspace)
                .to_string_lossy()
                .into_owned()
        });
        let mut statement = self
            .connection
            .prepare(
                "SELECT id,workspace_root,updated_at,title,account,model,node_tree_id,node_agent_id,root_profile FROM sessions WHERE ?1 IS NULL OR workspace_identity=?1 ORDER BY updated_at DESC, id ASC LIMIT ?2",
            )
            .map_err(|_| "could not query the session directory".to_owned())?;
        let rows = statement
            .query_map((identity, MAX_DIRECTORY_ROWS), |row| {
                Ok(DirectoryEntry {
                    id: row.get(0)?,
                    workspace_root: PathBuf::from(row.get::<_, String>(1)?),
                    updated_at_unix: from_i64(row.get(2)?)?,
                    title: row.get(3)?,
                    account: row.get(4)?,
                    model: row.get(5)?,
                    node: decode_node(row, 6)?,
                    root_profile: decode_root_profile(row, 8)?,
                })
            })
            .map_err(|_| "could not query the session directory".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "a session directory row is invalid".to_owned())?;
        Ok(rows)
    }

    /// The newest session whose node is in the tree `tree_id`.
    ///
    /// # Errors
    ///
    /// The read failed.
    pub fn newest_of_tree(&mut self, tree_id: &str) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT id FROM sessions WHERE node_tree_id=?1 ORDER BY updated_at DESC, id ASC LIMIT 1",
                [tree_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "could not read the session of a tree".to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    /// The directory lists the newest sessions first, across workspaces or
    /// of one, and a tree's newest session is the one an attach of its root
    /// opens.
    #[test]
    fn the_directory_lists_newest_first_and_finds_a_trees_newest_session() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"));
        let (one, two) = (root.path().join("one"), root.path().join("two"));
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        let mut store = ProductStore::open(&paths).unwrap();
        let mut older = sessions::SessionRecord::new(one.clone());
        older.updated_at_unix = 10;
        older.title = Some("older".into());
        store.commit(&mut older).unwrap();
        let mut newer = sessions::SessionRecord::new(two.clone());
        newer.updated_at_unix = 20;
        store.commit(&mut newer).unwrap();
        // A successor in the older one's tree, newer than both.
        let mut successor = sessions::SessionRecord::new(one.clone());
        successor.node = older.node.clone();
        successor.updated_at_unix = 30;
        store.commit(&mut successor).unwrap();
        let all: Vec<String> = store
            .directory(None)
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(
            all,
            [successor.id.clone(), newer.id.clone(), older.id.clone()]
        );
        let of_one = store.directory(Some(&one)).unwrap();
        assert_eq!(of_one.len(), 2);
        assert_eq!(of_one[1].title.as_deref(), Some("older"));
        assert_eq!(of_one[1].workspace_root, one);
        assert_eq!(
            store.newest_of_tree(&older.node.tree_id).unwrap(),
            Some(successor.id.clone())
        );
        assert_eq!(store.newest_of_tree("nowhere").unwrap(), None);
    }
}
