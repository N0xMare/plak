//! Bounded product facts for the session directory, not stored history.

use plakous_hosted::runtime::{
    ResumeAfterLoss,
    operator_attention::{AttentionReason, root_reason},
};
use plakous_hosted::sessions::projector::{ProductIntents, RootInterruptCause, SessionViews};
use rusqlite::{Connection, OptionalExtension as _};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::views::{
    intents::{read_host_intents, read_intents},
    unavailable, view_cursor_and_state,
};
use super::{ProductStore, decode_node, from_i64};

type Stamp = (
    i64,
    i64,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

const STAMP_FIELDS: &str = "s.revision,s.updated_at,s.archived,s.title_override,s.runner_display,
    v.head_locator,p.applied_seq,(SELECT max(i.rowid) FROM host_intents i WHERE i.session_id=s.id)";

fn stamp(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Stamp> {
    Ok((
        row.get(at)?,
        row.get(at + 1)?,
        row.get(at + 2)?,
        row.get(at + 3)?,
        row.get(at + 4)?,
        row.get(at + 5)?,
        row.get(at + 6)?,
        row.get(at + 7)?,
    ))
}

pub(crate) struct ProductDirectoryProbe {
    connection: Connection,
    version: i64,
    rows: BTreeMap<String, Stamp>,
}

impl ProductDirectoryProbe {
    pub(crate) fn open(paths: &crate::home::paths::PlakPaths) -> Result<Self, String> {
        let connection =
            ProductStore::inspection_connection(paths, std::time::Duration::from_millis(250))
                .map_err(|error| error.to_string())?;
        ProductStore::admit_read_only(&connection).map_err(|error| error.to_string())?;
        let version = data_version(&connection)?;
        let rows = fingerprints(&connection)?;
        Ok(Self {
            connection,
            version,
            rows,
        })
    }

    pub(crate) fn changes(&mut self) -> Result<BTreeSet<String>, String> {
        let version = data_version(&self.connection)?;
        if version == self.version {
            return Ok(BTreeSet::new());
        }
        let rows = fingerprints(&self.connection)?;
        let changed = self
            .rows
            .keys()
            .filter(|id| !rows.contains_key(*id))
            .chain(
                rows.iter()
                    .filter(|(id, stamp)| self.rows.get(*id) != Some(*stamp))
                    .map(|(id, _)| id),
            )
            .cloned()
            .collect();
        self.rows = rows;
        self.version = version;
        Ok(changed)
    }

    pub(crate) fn note(&mut self, id: &str, metadata: Option<&DirectoryMetadata>) {
        if let Some(metadata) = metadata {
            self.rows.insert(id.into(), metadata.stamp.clone());
        } else {
            self.rows.remove(id);
        }
    }
}

fn data_version(connection: &Connection) -> Result<i64, String> {
    connection
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .map_err(unavailable)
}

fn fingerprints(connection: &Connection) -> Result<BTreeMap<String, Stamp>, String> {
    connection
        .prepare(&format!(
            "SELECT s.id,{STAMP_FIELDS}
            FROM sessions s LEFT JOIN projector_cursors p ON p.session_id=s.id
            LEFT JOIN session_views v ON v.session_id=s.id"
        ))
        .map_err(unavailable)?
        .query_map([], |row| Ok((row.get(0)?, stamp(row, 1)?)))
        .map_err(unavailable)?
        .collect::<Result<_, _>>()
        .map_err(unavailable)
}

pub(crate) struct DirectoryMetadata {
    stamp: Stamp,
    pub(crate) workspace: PathBuf,
    pub(crate) identity: String,
    pub(crate) title: Option<String>,
    pub(crate) node: plakous_hosted::sessions::SessionNode,
    pub(crate) runner: Option<plakous_hosted::sessions::RunnerDisplay>,
    pub(crate) archived: bool,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) locator: Option<plakous::sessions::SessionLocator>,
    pub(crate) account: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) fast: bool,
}
impl ProductStore {
    pub(crate) fn directory_metadata(&self, id: &str) -> Result<Option<DirectoryMetadata>, String> {
        self.connection
            .prepare(&format!(
                "SELECT s.workspace_root,s.workspace_identity,COALESCE(s.title_override,s.title),
                        s.node_tree_id,s.node_agent_id,s.runner_display,s.archived,s.created_at,s.updated_at,
                        CASE WHEN v.session_id IS NULL THEN s.locator ELSE v.head_locator END,
                        s.account,s.model,s.effort,s.target_selection,{STAMP_FIELDS}
                 FROM sessions s LEFT JOIN session_views v ON v.session_id=s.id
                 LEFT JOIN projector_cursors p ON p.session_id=s.id WHERE s.id=?1",
            ))
            .and_then(|mut statement| statement.query_row([id], |row| {
                let selection = serde_json::from_str::<crate::product_config::ProductTargetSelection>(
                    &row.get::<_, String>(13)?,
                )
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(DirectoryMetadata {
                    stamp: stamp(row, 14)?,
                    workspace: PathBuf::from(row.get::<_, String>(0)?),
                    identity: row.get(1)?,
                    title: row.get(2)?,
                    node: decode_node(row, 3)?,
                    runner: super::decode_runner_display(row, 5)?,
                    archived: row.get(6)?,
                    created_at: from_i64(row.get(7)?)?,
                    updated_at: from_i64(row.get(8)?)?,
                    locator: row.get::<_, Option<String>>(9)?
                        .map(|loc| loc.parse().map_err(|_| rusqlite::Error::InvalidQuery))
                        .transpose()?,
                    account: row.get(10)?,
                    model: row.get(11)?,
                    effort: row.get(12)?,
                    fast: selection == crate::product_config::ProductTargetSelection::NativePriority,
                })
            }))
            .optional()
            .map_err(|_| "directory metadata is unavailable".into())
    }

    pub(crate) fn archive_flag(&self, id: &str) -> Result<bool, String> {
        self.connection
            .query_row("SELECT archived FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|_| "product session archive state is invalid".to_owned())?
            .ok_or_else(|| "session not found".into())
    }

    pub(crate) fn directory_activity(
        &mut self,
        id: &str,
        policy: ResumeAfterLoss,
    ) -> Result<
        (
            plakous_hosted::runtime::RootStanding,
            Option<AttentionReason>,
            u32,
        ),
        String,
    > {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(unavailable)?;
        let mut intents = ProductIntents::default();
        read_host_intents(
            &tx,
            "SELECT kind,anchor,ordinal,value FROM host_intents WHERE session_id=?1 AND kind IN ('pause_cause','interrupt_cause','continuation_decision') ORDER BY anchor,ordinal",
            id,
            &mut intents,
        )?;
        // Only a recorded loss needs submission evidence to decide standing.
        // Reading all submitted prompt bodies on an ordinary idle update is
        // proportional to the session's history, not its directory row.
        let loss = intents
            .root_interrupts
            .iter()
            .max_by_key(|interrupt| (interrupt.anchor, interrupt.ordinal))
            .is_some_and(|interrupt| {
                matches!(
                    interrupt.value.cause,
                    RootInterruptCause::OwnerLost | RootInterruptCause::Handoff,
                )
            });
        if loss {
            intents = read_intents(&tx, &mut self.verified_submissions, id, false)?;
        }
        let mut views = SessionViews::empty();
        if let Some((cursor, state)) = view_cursor_and_state(&tx, id)? {
            views.cursor = cursor;
            views.state = state;
        }
        let standing = plakous_hosted::runtime::root_standing_from_intents(&intents, &views);
        let reason = root_reason(&views, &intents, policy);
        let mut wakes = 0_u32;
        if reason.is_some() {
            let items = super::load_queue(&tx, id)?;
            if items.is_empty() {
                return Ok((standing, reason, 0));
            }
            if !loss {
                intents = read_intents(&tx, &mut self.verified_submissions, id, false)?;
            }
            let commands =
                plakous_hosted::sessions::QueueCommands::from_join(&intents, Some(&views));
            for item in items {
                if commands.pending_for(&item).is_some_and(|submission| {
                    submission.kind == plakous_hosted::sessions::projector::SubmissionKind::HostWake
                        && !plakous_hosted::runtime::is_root_loss_command(&submission.command_id)
                }) {
                    wakes = wakes.saturating_add(1);
                }
            }
        }
        Ok((standing, reason, wakes))
    }
}

#[cfg(test)]
#[test]
#[allow(clippy::unwrap_used, reason = "isolated store fixture")]
fn projector_replay_notifies_only_when_the_cursor_or_view_changes() {
    use std::time::Instant;

    use plakous_hosted::sessions::projector::{ProjectionCommit, ViewWrites};

    let (_home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    let mut store = ProductStore::open(&paths).unwrap();
    let mut record = super::sessions::SessionRecord::new(paths.home().to_owned());
    store.commit(&mut record).unwrap();
    let (_inbox, mailbox) = super::directory_notice::local::Inbox::register(&paths);
    let mut view = SessionViews::empty();
    let writes = ViewWrites::between(&view, &view);
    let notified = || mailbox.take_until(Instant::now()).unwrap().records;
    let mut publish = |view: &SessionViews| {
        assert_eq!(
            store
                .commit_projection_views(&record.id, &writes, &view.state)
                .unwrap(),
            ProjectionCommit::Applied
        );
    };
    publish(&view);
    assert_eq!(notified(), BTreeSet::from([record.id.clone()]));
    publish(&view);
    assert_eq!(notified(), BTreeSet::new());
    view.state.counters.root_turns = 1;
    publish(&view);
    assert_eq!(notified(), BTreeSet::from([record.id.clone()]));
}
