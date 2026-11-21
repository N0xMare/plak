//! Bounded product facts for the session directory, not stored history.

use plakous_hosted::runtime::{
    ResumeAfterLoss,
    operator_attention::{AttentionReason, root_reason},
};
use plakous_hosted::sessions::projector::SessionViews;
use rusqlite::OptionalExtension as _;
use std::path::PathBuf;

use super::views::{intents::read_intents, unavailable, view_cursor_and_state};
use super::{ProductStore, decode_node, from_i64};

pub(crate) struct DirectoryMetadata {
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
            .prepare(
                "SELECT s.workspace_root,s.workspace_identity,COALESCE(s.title_override,s.title),
                        s.node_tree_id,s.node_agent_id,s.runner_display,s.archived,s.created_at,s.updated_at,
                        CASE WHEN v.session_id IS NULL THEN s.locator ELSE v.head_locator END,
                        s.account,s.model,s.effort,s.target_selection
                 FROM sessions s LEFT JOIN session_views v ON v.session_id=s.id WHERE s.id=?1",
            )
            .and_then(|mut statement| statement.query_row([id], |row| {
                let selection = serde_json::from_str::<crate::product_config::ProductTargetSelection>(
                    &row.get::<_, String>(13)?,
                )
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(DirectoryMetadata {
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
        let intents = read_intents(&tx, &mut self.verified_submissions, id, false)?;
        let mut views = SessionViews::empty();
        if let Some((cursor, state)) = view_cursor_and_state(&tx, id)? {
            views.cursor = cursor;
            views.state = state;
        }
        let standing = plakous_hosted::runtime::root_standing_from_intents(&intents, &views);
        let reason = root_reason(&views, &intents, policy);
        let mut wakes = 0_u32;
        if reason.is_some() {
            let commands =
                plakous_hosted::sessions::QueueCommands::from_join(&intents, Some(&views));
            for item in super::load_queue(&tx, id)? {
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
