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
    pub archived: bool,
    pub account: Option<String>,
    pub model: Option<String>,
    pub node: plakous_hosted::sessions::SessionNode,
    pub root_profile: Option<plakous_hosted::sessions::RootProfile>,
    /// The runtime head the session's lease is on, once a face opened it.
    pub locator: Option<plakous::sessions::SessionLocator>,
}

impl ProductStore {
    /// Distinct committed workspaces of one runner, including archived records.
    /// Group saved path bytes before limiting so busy workspaces cannot hide recents.
    pub fn recent_workspaces(&self, runner_id: &str) -> Result<Vec<(String, u64)>, String> {
        let mut statement = self.connection.prepare(
            "SELECT workspace_root,MAX(updated_at) AS newest FROM sessions WHERE COALESCE(json_extract(runner_display,'$.runner_id'),'local')=?1 GROUP BY workspace_root COLLATE BINARY ORDER BY newest DESC,workspace_root ASC LIMIT 20",
        ).map_err(|_|"could not query recent workspaces".to_owned())?;
        statement
            .query_map([runner_id], |row| Ok((row.get(0)?, from_i64(row.get(1)?)?)))
            .map_err(|_| "could not query recent workspaces".to_owned())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "a recent workspace row is invalid".to_owned())
    }

    /// Every record's identity for startup recovery, without the face's row limit.
    #[allow(
        dead_code,
        reason = "the optional hosting product reads the full directory"
    )]
    pub(crate) fn session_ids(&self) -> Result<Vec<String>, String> {
        self.connection
            .prepare("SELECT id FROM sessions ORDER BY id")
            .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
            .map_err(|error| format!("the session identities could not be read: {error}"))
    }
    /// The newest sessions, at most [`MAX_DIRECTORY_ROWS`], of `workspace`
    /// when given, else of the whole home.
    ///
    /// # Errors
    ///
    /// The read failed or a row does not decode.
    pub fn directory(&mut self, workspace: Option<&Path>) -> Result<Vec<DirectoryEntry>, String> {
        self.directory_with_archived(workspace, false)
    }

    /// Include archived records only when explicitly requested, before limiting rows.
    pub fn directory_with_archived(
        &mut self,
        workspace: Option<&Path>,
        include_archived: bool,
    ) -> Result<Vec<DirectoryEntry>, String> {
        self.directory_filtered(workspace, None, include_archived)
    }

    /// Runner ids, workspace and archived visibility all filter before the limit.
    pub fn directory_filtered(
        &mut self,
        workspace: Option<&Path>,
        runner_id: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<DirectoryEntry>, String> {
        let nonlocal = runner_id.is_some_and(|runner| runner != "local");
        let identity = workspace
            .map(|workspace| {
                if nonlocal {
                    workspace
                        .to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "remote workspace filter is not UTF-8".to_owned())
                } else {
                    Ok(sessions::workspace_identity(workspace)
                        .to_string_lossy()
                        .into_owned())
                }
            })
            .transpose()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sessions.id,sessions.workspace_root,sessions.updated_at,COALESCE(sessions.title_override,sessions.title),sessions.account,sessions.model,sessions.node_tree_id,sessions.node_agent_id,sessions.root_profile,CASE WHEN v.session_id IS NULL THEN sessions.locator ELSE v.head_locator END,sessions.archived FROM sessions LEFT JOIN session_views v ON v.session_id=sessions.id WHERE (?1 IS NULL OR (CASE WHEN ?5 THEN sessions.workspace_root ELSE sessions.workspace_identity END)=?1) AND (?3 OR sessions.archived=0) AND (?4 IS NULL OR COALESCE(json_extract(sessions.runner_display,'$.runner_id'),'local')=?4) ORDER BY sessions.updated_at DESC, sessions.id ASC LIMIT ?2",
            )
            .map_err(|_| "could not query the session directory".to_owned())?;
        let rows = statement
            .query_map(
                (
                    identity,
                    MAX_DIRECTORY_ROWS,
                    include_archived,
                    runner_id,
                    nonlocal,
                ),
                |row| {
                    Ok(DirectoryEntry {
                        id: row.get(0)?,
                        workspace_root: PathBuf::from(row.get::<_, String>(1)?),
                        updated_at_unix: from_i64(row.get(2)?)?,
                        title: row.get(3)?,
                        archived: row.get(10)?,
                        account: row.get(4)?,
                        model: row.get(5)?,
                        node: decode_node(row, 6)?,
                        root_profile: decode_root_profile(row, 8)?,
                        locator: row
                            .get::<_, Option<String>>(9)?
                            .map(|value| value.parse().map_err(|_| rusqlite::Error::InvalidQuery))
                            .transpose()?,
                    })
                },
            )
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

    #[test]
    fn recent_workspaces_group_before_limit_and_keep_archived_runner_scoped_paths() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"));
        let mut store = ProductStore::open(&paths).unwrap();
        let common = root.path().join("common");
        for timestamp in 2000..2260 {
            let mut record = sessions::SessionRecord::new(common.clone());
            record.updated_at_unix = timestamp;
            store.commit(&mut record).unwrap();
        }
        for index in 0..25 {
            let mut record =
                sessions::SessionRecord::new(root.path().join(format!("older-{index}")));
            record.updated_at_unix = 100 + index;
            record.archived = index == 24;
            store.commit(&mut record).unwrap();
        }
        let remote_root = root.path().join("older-24");
        let mut remote = sessions::SessionRecord::new(remote_root.clone());
        remote.updated_at_unix = 9999;
        remote.runner = Some(plakous_hosted::sessions::RunnerDisplay {
            name: "peer".into(),
            runner_id: "runner-a".into(),
            workspace_root: remote_root.clone(),
            workspace_cwd: remote_root.clone(),
        });
        store.commit(&mut remote).unwrap();
        let mut expected = vec![(common.to_string_lossy().into_owned(), 2259)];
        expected.extend((6..25).rev().map(|index| {
            (
                root.path()
                    .join(format!("older-{index}"))
                    .to_string_lossy()
                    .into_owned(),
                100 + index,
            )
        }));
        assert_eq!(store.recent_workspaces("local").unwrap(), expected);
        assert_eq!(
            store.recent_workspaces("runner-a").unwrap(),
            [(remote_root.to_string_lossy().into_owned(), 9999)]
        );
        assert!(store.recent_workspaces("unknown").unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn remote_workspace_filters_match_saved_bytes_without_host_canonicalization() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"));
        let actual = root.path().join("actual");
        let alias = root.path().join("alias");
        std::fs::create_dir_all(&actual).unwrap();
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        let mut store = ProductStore::open(&paths).unwrap();
        let mut remote = sessions::SessionRecord::new(alias.clone());
        remote.runner = Some(plakous_hosted::sessions::RunnerDisplay {
            name: "peer".into(),
            runner_id: "runner-a".into(),
            workspace_root: alias.clone(),
            workspace_cwd: alias.clone(),
        });
        store.commit(&mut remote).unwrap();
        let mut local = sessions::SessionRecord::new(actual.clone());
        store.commit(&mut local).unwrap();
        for present_on_host in [true, false] {
            if !present_on_host {
                std::fs::remove_file(&alias).unwrap();
            }
            let rows = store
                .directory_filtered(Some(&alias), Some("runner-a"), false)
                .unwrap();
            assert_eq!(
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                [remote.id.as_str()]
            );
            assert_eq!(rows[0].workspace_root, alias);
            assert!(
                store
                    .directory_filtered(Some(&actual), Some("runner-a"), false)
                    .unwrap()
                    .is_empty()
            );
        }
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert_eq!(
            store
                .directory_filtered(Some(&alias), Some("local"), false)
                .unwrap()
                .into_iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [local.id]
        );
    }

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
