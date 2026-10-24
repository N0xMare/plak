//! Row-level saves: the rows this connection last committed for a tree, and
//! the diff that writes only what changed since.
//!
//! An image is the exact input of the last acknowledged save, so a record
//! equal to its image entry encodes to the row the file already holds. It is
//! trusted only while `PRAGMA data_version` is unchanged, that is while no
//! other connection has committed to the file since.

use std::collections::BTreeMap;

use plakous_recursive_subagents::protocol::{AttemptId, CandidateId, TopologyGrant};
use plakous_recursive_subagents::tree::{
    AgentCommand, AttemptRecord, CandidateRecord, MailRecord, NodeRecord, OperationRecord,
    ProductAgentTree, WorkspaceRecord,
};
use rusqlite::Transaction;

use crate::rows::{Row, delete_row, write_row};

/// One tree's persisted maps as this connection last committed them.
pub(crate) struct TreeImage {
    data_version: i64,
    nodes: BTreeMap<String, NodeRecord>,
    grants: BTreeMap<String, TopologyGrant>,
    attempts: BTreeMap<AttemptId, AttemptRecord>,
    candidates: BTreeMap<CandidateId, CandidateRecord>,
    workspaces: BTreeMap<String, WorkspaceRecord>,
    messages: BTreeMap<String, MailRecord>,
    operations: BTreeMap<String, OperationRecord>,
    commands: BTreeMap<String, AgentCommand>,
    feed_cursors: BTreeMap<String, u64>,
}

impl TreeImage {
    /// The image of `tree` as a whole-tree save just wrote it, at the file's
    /// `data_version` inside that save's transaction.
    pub(crate) fn of(tree: &ProductAgentTree, data_version: i64) -> Self {
        Self {
            data_version,
            nodes: tree.nodes().clone(),
            grants: tree.grants().clone(),
            attempts: tree.attempts().clone(),
            candidates: tree.candidates().clone(),
            workspaces: tree.workspaces().clone(),
            messages: tree.messages().clone(),
            operations: tree.operations().clone(),
            commands: tree.commands().clone(),
            feed_cursors: tree.feed_cursors().clone(),
        }
    }

    /// Whether the file still holds exactly this image: no other connection
    /// committed since it was taken.
    pub(crate) const fn is_current(&self, data_version: i64) -> bool {
        self.data_version == data_version
    }

    /// Writes the rows of `tree` that differ from this image, after the
    /// header compare-and-swap, and returns the changes to fold in once the
    /// transaction commits.
    ///
    /// Nodes are upserted first and deleted last, so no foreign key sees a
    /// grant or attempt without its node. Within a table, removed keys go
    /// before changed rows, so a new attempt may reuse a removed attempt's
    /// request key.
    pub(crate) fn write_changes<'t>(
        &self,
        tx: &Transaction<'_>,
        tree: &'t ProductAgentTree,
    ) -> Result<Delta<'t>, String> {
        let tree_id = tree.tree_id.as_str();
        let delta = Delta {
            nodes: Changes::between(&self.nodes, tree.nodes()),
            grants: Changes::between(&self.grants, tree.grants()),
            attempts: Changes::between(&self.attempts, tree.attempts()),
            candidates: Changes::between(&self.candidates, tree.candidates()),
            workspaces: Changes::between(&self.workspaces, tree.workspaces()),
            messages: Changes::between(&self.messages, tree.messages()),
            operations: Changes::between(&self.operations, tree.operations()),
            commands: Changes::between(&self.commands, tree.commands()),
            feed_cursors: Changes::between(&self.feed_cursors, tree.feed_cursors()),
        };
        delta.nodes.upsert(tx, tree_id)?;
        delta.grants.replace(tx, tree_id)?;
        delta.attempts.replace(tx, tree_id)?;
        delta.candidates.replace(tx, tree_id)?;
        delta.workspaces.replace(tx, tree_id)?;
        delta.messages.replace(tx, tree_id)?;
        delta.operations.replace(tx, tree_id)?;
        delta.commands.replace(tx, tree_id)?;
        delta.feed_cursors.replace(tx, tree_id)?;
        delta.nodes.delete(tx, tree_id)?;
        Ok(delta)
    }
}

/// What one row-level save wrote, to fold into the image on commit.
pub(crate) struct Delta<'t> {
    nodes: Changes<'t, NodeRecord>,
    grants: Changes<'t, TopologyGrant>,
    attempts: Changes<'t, AttemptRecord>,
    candidates: Changes<'t, CandidateRecord>,
    workspaces: Changes<'t, WorkspaceRecord>,
    messages: Changes<'t, MailRecord>,
    operations: Changes<'t, OperationRecord>,
    commands: Changes<'t, AgentCommand>,
    feed_cursors: Changes<'t, u64>,
}

impl Delta<'_> {
    /// Advances `image` to the tree this delta was taken from. Only for a
    /// committed save: an image ahead of the file would skip writes.
    pub(crate) fn fold_into(self, image: &mut TreeImage) {
        self.nodes.fold_into(&mut image.nodes);
        self.grants.fold_into(&mut image.grants);
        self.attempts.fold_into(&mut image.attempts);
        self.candidates.fold_into(&mut image.candidates);
        self.workspaces.fold_into(&mut image.workspaces);
        self.messages.fold_into(&mut image.messages);
        self.operations.fold_into(&mut image.operations);
        self.commands.fold_into(&mut image.commands);
        self.feed_cursors.fold_into(&mut image.feed_cursors);
    }
}

/// One map's difference from its image: rows new or changed, keys removed.
struct Changes<'t, R: Row> {
    changed: Vec<(&'t R::Key, &'t R)>,
    removed: Vec<R::Key>,
}

impl<'t, R: Row> Changes<'t, R> {
    fn between(image: &BTreeMap<R::Key, R>, current: &'t BTreeMap<R::Key, R>) -> Self {
        Self {
            changed: current
                .iter()
                .filter(|(key, row)| image.get(key).is_none_or(|old| !old.same_row(row)))
                .collect(),
            removed: image
                .keys()
                .filter(|key| !current.contains_key(key))
                .cloned()
                .collect(),
        }
    }

    fn upsert(&self, tx: &Transaction<'_>, tree_id: &str) -> Result<(), String> {
        if self.changed.is_empty() {
            return Ok(());
        }
        let sql = R::TABLE.upsert_sql();
        for (key, row) in &self.changed {
            write_row(tx, &sql, tree_id, *key, *row)?;
        }
        Ok(())
    }

    fn delete(&self, tx: &Transaction<'_>, tree_id: &str) -> Result<(), String> {
        if self.removed.is_empty() {
            return Ok(());
        }
        let sql = R::TABLE.delete_sql();
        for key in &self.removed {
            delete_row::<R>(tx, &sql, tree_id, key)?;
        }
        Ok(())
    }

    fn replace(&self, tx: &Transaction<'_>, tree_id: &str) -> Result<(), String> {
        self.delete(tx, tree_id)?;
        self.upsert(tx, tree_id)
    }

    fn fold_into(self, image: &mut BTreeMap<R::Key, R>) {
        for key in &self.removed {
            image.remove(key);
        }
        for (key, row) in self.changed {
            image.insert(key.clone(), row.clone());
        }
    }
}
