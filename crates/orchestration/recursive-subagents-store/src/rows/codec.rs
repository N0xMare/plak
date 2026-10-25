//! The per-table row codecs a save writes: each persisted map's table, key
//! and column values.

use rusqlite::{Transaction, params};

use plakous_recursive_subagents::protocol::{AttemptId, CandidateId, TemplateName, TopologyGrant};
use plakous_recursive_subagents::tree::{
    AgentCommand, AttemptRecord, CandidateRecord, MailRecord, NodeRecord, OperationRecord,
    WorkspaceRecord,
};

use super::{agent_tree_enum_text, encode_id_list};
use crate::to_i64;

/// One child table: its name and its columns in parameter order. The first
/// two columns are its primary key, `tree_id` and the map key.
pub(crate) struct Table {
    name: &'static str,
    pub(super) noun: &'static str,
    columns: &'static [&'static str],
}

impl Table {
    pub(super) fn insert_sql(&self) -> String {
        let values = (1..=self.columns.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "INSERT INTO {}({}) VALUES ({values})",
            self.name,
            self.columns.join(", ")
        )
    }

    /// An insert that rewrites the row in place on a key conflict. Unlike
    /// `INSERT OR REPLACE` it never deletes: a node keeps its grant and
    /// attempts under the foreign keys, and a clash on another unique index
    /// fails the save instead of silently dropping the other row.
    pub(crate) fn upsert_sql(&self) -> String {
        let set = self.columns[2..]
            .iter()
            .map(|column| format!("{column}=excluded.{column}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{} ON CONFLICT(tree_id,{}) DO UPDATE SET {set}",
            self.insert_sql(),
            self.columns[1]
        )
    }

    pub(crate) fn delete_sql(&self) -> String {
        format!(
            "DELETE FROM {} WHERE tree_id=?1 AND {}=?2",
            self.name, self.columns[1]
        )
    }
}

/// The row codec of one persisted map: its key, its table, and its values in
/// the table's column order.
pub(crate) trait Row: Clone + PartialEq {
    type Key: Ord + Clone;
    const TABLE: Table;

    fn key_text(key: &Self::Key) -> &str;

    /// Executes `sql` with this row's parameters. The outer error is a value
    /// that does not encode; the inner one is `SQLite`'s.
    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        key: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String>;

    /// Whether `self` and `other` encode to the same row. The default is
    /// `PartialEq`, which is right only while it compares every persisted
    /// field, as a derived impl does; a record with a field that is not a
    /// column overrides it.
    fn same_row(&self, other: &Self) -> bool {
        self == other
    }
}

impl Row for NodeRecord {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_nodes",
        noun: "agent node",
        columns: &[
            "tree_id",
            "agent_id",
            "parent_agent_id",
            "depth",
            "description",
            "prompt",
            "output_schema_json",
            "schema_digest",
            "template",
            "tuple_id",
            "account",
            "effort",
            "status",
            "workspace_id",
            "tier",
            "attempt_id",
            "session_locator",
            "path",
            "branch",
            "active_turn",
            "fold_order",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let node = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                node.agent_id,
                node.parent_agent_id,
                i64::from(node.depth),
                node.description,
                node.prompt,
                node.output_schema_json,
                node.schema_digest,
                node.template.as_ref().map(TemplateName::as_str),
                node.tuple_id,
                node.account,
                node.effort,
                agent_tree_enum_text(node.status)?,
                node.workspace_id,
                agent_tree_enum_text(node.tier)?,
                node.attempt_id.as_ref().map(AttemptId::as_str),
                node.session_locator,
                node.path.to_string_lossy().as_ref(),
                node.branch,
                i64::from(node.active_turn),
                node.fold_order.map(to_i64).transpose()?,
            ],
        ))
    }

    /// Every persisted column; `abandoned_turn` describes one reopen and is
    /// never a row. Destructured without `..`, so a new field cannot be left
    /// out of the comparison silently.
    fn same_row(&self, other: &Self) -> bool {
        let Self {
            agent_id,
            parent_agent_id,
            depth,
            description,
            prompt,
            output_schema_json,
            schema_digest,
            template,
            tuple_id,
            account,
            effort,
            status,
            workspace_id,
            tier,
            attempt_id,
            session_locator,
            path,
            branch,
            active_turn,
            fold_order,
            abandoned_turn: _,
        } = self;
        *agent_id == other.agent_id
            && *parent_agent_id == other.parent_agent_id
            && *depth == other.depth
            && *description == other.description
            && *prompt == other.prompt
            && *output_schema_json == other.output_schema_json
            && *schema_digest == other.schema_digest
            && *template == other.template
            && *tuple_id == other.tuple_id
            && *account == other.account
            && *effort == other.effort
            && *status == other.status
            && *workspace_id == other.workspace_id
            && *tier == other.tier
            && *attempt_id == other.attempt_id
            && *session_locator == other.session_locator
            && *path == other.path
            && *branch == other.branch
            && *active_turn == other.active_turn
            && *fold_order == other.fold_order
    }
}

impl Row for TopologyGrant {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_grants",
        noun: "agent grant",
        columns: &[
            "tree_id",
            "agent_id",
            "may_spawn",
            "may_adopt",
            "max_depth",
            "max_nodes",
            "max_children",
            "visible_ids",
            "message_peers",
            "managed_ids",
            "interrupt_ids",
            "close_ids",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        agent_id: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let grant = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                agent_id,
                i64::from(grant.may_spawn),
                i64::from(grant.may_adopt),
                i64::from(grant.max_depth),
                i64::from(grant.max_nodes),
                i64::from(grant.max_children),
                encode_id_list(&grant.visible_ids)?,
                encode_id_list(&grant.message_peers)?,
                encode_id_list(&grant.managed_ids)?,
                encode_id_list(&grant.interrupt_ids)?,
                encode_id_list(&grant.close_ids)?,
            ],
        ))
    }
}

impl Row for AttemptRecord {
    type Key = AttemptId;
    const TABLE: Table = Table {
        name: "agent_attempts",
        noun: "agent attempt",
        columns: &[
            "tree_id",
            "attempt_id",
            "agent_id",
            "status",
            "turn_token",
            "turn_id",
            "submit_harness_nudge",
            "candidate_id",
            "retry_of",
            "request_key",
            "started_at",
            "ended_at",
            "blocked_kind",
            "blocked_account",
            "blocked_resets_at",
            "failure_kind",
            "failure_message",
            "elapsed_ms",
            "turns",
            "tool_calls",
            "input_tokens",
            "output_tokens",
            "spawn_identity",
            "output_schema_json",
            "schema_digest",
            "applied_terminal_read_seqs",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key.as_str()
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let attempt = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                attempt.attempt_id.as_str(),
                attempt.agent_id,
                agent_tree_enum_text(attempt.status)?,
                to_i64(attempt.turn_token)?,
                attempt.turn_id,
                i64::from(attempt.submit_harness_nudge.rounds()),
                attempt.candidate_id.as_ref().map(CandidateId::as_str),
                attempt.retry_of.as_ref().map(AttemptId::as_str),
                attempt.request_key,
                to_i64(attempt.started_at)?,
                attempt.ended_at.map(to_i64).transpose()?,
                attempt
                    .blocked
                    .as_ref()
                    .map(|blocked| blocked.quota.as_str()),
                attempt
                    .blocked
                    .as_ref()
                    .map(|blocked| blocked.account.as_str()),
                attempt
                    .blocked
                    .as_ref()
                    .and_then(|blocked| blocked.resets_at)
                    .map(to_i64)
                    .transpose()?,
                attempt
                    .failure
                    .as_ref()
                    .map(|failure| failure.kind.as_str()),
                attempt
                    .failure
                    .as_ref()
                    .map(|failure| failure.message.as_str()),
                to_i64(attempt.usage.elapsed_ms)?,
                i64::from(attempt.usage.turns),
                i64::from(attempt.usage.tool_calls),
                to_i64(attempt.usage.input_tokens)?,
                to_i64(attempt.usage.output_tokens)?,
                attempt
                    .spawn_identity
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|_| "spawn identity is not serializable".to_owned())?,
                attempt.output_schema_json,
                attempt.schema_digest,
                serde_json::to_string(&attempt.applied_terminal_read_seqs)
                    .map_err(|_| "applied terminal reads are not serializable".to_owned())?,
            ],
        ))
    }

    fn same_row(&self, other: &Self) -> bool {
        let Self {
            output_json: _,
            output_schema_json,
            schema_digest,
            attempt_id,
            agent_id,
            retry_of,
            request_key,
            spawn_identity,
            started_at,
            ended_at,
            blocked,
            failure,
            usage,
            status,
            turn_token,
            turn_id,
            applied_terminal_read_seqs,
            submit_harness_nudge,
            candidate_id,
        } = self;
        *output_schema_json == other.output_schema_json
            && *schema_digest == other.schema_digest
            && *attempt_id == other.attempt_id
            && *agent_id == other.agent_id
            && *retry_of == other.retry_of
            && *request_key == other.request_key
            && *spawn_identity == other.spawn_identity
            && *started_at == other.started_at
            && *ended_at == other.ended_at
            && *blocked == other.blocked
            && *failure == other.failure
            && *usage == other.usage
            && *status == other.status
            && *turn_token == other.turn_token
            && *turn_id == other.turn_id
            && *applied_terminal_read_seqs == other.applied_terminal_read_seqs
            && *submit_harness_nudge == other.submit_harness_nudge
            && *candidate_id == other.candidate_id
    }
}

impl Row for CandidateRecord {
    type Key = CandidateId;
    const TABLE: Table = Table {
        name: "agent_candidates",
        noun: "agent candidate",
        columns: &[
            "tree_id",
            "candidate_id",
            "workspace_id",
            "producing_agent_id",
            "producing_attempt_id",
            "base_commit",
            "base_content",
            "base_change_id",
            "commit_id",
            "content",
            "change_id",
            "sealed_at",
            "adopted_by",
            "adopted_evidence",
            "merged_at",
            "target_before",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key.as_str()
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let candidate = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                candidate.candidate_id.as_str(),
                candidate.workspace_id,
                candidate.producing_agent_id,
                candidate.producing_attempt_id.as_str(),
                candidate.base_commit,
                candidate.base_content,
                candidate.base_change_id,
                candidate.commit,
                candidate.content,
                candidate.change_id,
                to_i64(candidate.sealed_at)?,
                candidate.adopted_by,
                serde_json::to_string(&candidate.adopted_evidence)
                    .map_err(|_| "candidate evidence could not be encoded".to_owned())?,
                candidate.merged_at,
                candidate.target_before,
            ],
        ))
    }
}

impl Row for WorkspaceRecord {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_workspaces",
        noun: "agent workspace",
        columns: &[
            "tree_id",
            "workspace_id",
            "owner_agent_id",
            "path",
            "mode",
            "status",
            "base_commit",
            "base_content",
            "base_change_id",
            "generation",
            "adopter_agent_id",
            "input_candidate_id",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let workspace = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                workspace.workspace_id,
                workspace.owner_agent_id,
                workspace.path.to_string_lossy().as_ref(),
                agent_tree_enum_text(workspace.mode)?,
                agent_tree_enum_text(workspace.status)?,
                workspace.base_commit,
                workspace.base_content,
                workspace.base_change_id,
                to_i64(workspace.generation)?,
                workspace.adopter_agent_id,
                workspace
                    .input_candidate_id
                    .as_ref()
                    .map(CandidateId::as_str),
            ],
        ))
    }
}

impl Row for MailRecord {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_messages",
        noun: "agent message",
        columns: &[
            "tree_id",
            "message_id",
            "from_agent",
            "to_agent",
            "body",
            "purpose",
            "priority",
            "in_reply_to",
            "disposition",
            "durable",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let mail = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                mail.message_id,
                mail.from,
                mail.to,
                mail.body,
                agent_tree_enum_text(mail.purpose)?,
                agent_tree_enum_text(mail.priority)?,
                mail.in_reply_to,
                agent_tree_enum_text(mail.disposition)?,
                agent_tree_enum_text(mail.durable)?,
            ],
        ))
    }
}

impl Row for OperationRecord {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_operations",
        noun: "agent operation",
        columns: &[
            "tree_id",
            "operation_id",
            "caller_agent_id",
            "tool_name",
            "canonical_request",
            "request_digest",
            "terminal_code",
            "terminal_response",
            "fold_anchor",
            "fold_order",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let operation = self;
        let terminal_code = operation
            .terminal_code
            .map(agent_tree_enum_text)
            .transpose()?;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                operation.operation_id,
                operation.caller_agent_id,
                operation.tool_name,
                operation.canonical_request,
                operation.request_digest,
                terminal_code,
                operation.terminal_response,
                operation.fold.map(|fold| to_i64(fold.seq)).transpose()?,
                operation.fold.map(|fold| to_i64(fold.order)).transpose()?,
            ],
        ))
    }
}

impl Row for AgentCommand {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_commands",
        noun: "agent command",
        columns: &[
            "tree_id",
            "command_id",
            "recipient_agent_id",
            "attempt_id",
            "kind",
            "canonical_text",
            "message_ids",
            "withdrawn",
            "fact",
            "turn_id",
            "accepted_seq",
        ],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        _: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        let command = self;
        Ok(tx.execute(
            sql,
            params![
                tree_id,
                command.command_id,
                command.recipient,
                command.attempt_id.as_ref().map(AttemptId::as_str),
                agent_tree_enum_text(command.kind)?,
                command.text,
                encode_id_list(&command.message_ids)?,
                i64::from(command.withdrawn),
                command.fact.map(agent_tree_enum_text).transpose()?,
                command.turn_id,
                command.accepted_seq.map(to_i64).transpose()?,
            ],
        ))
    }
}

/// A feed cursor: the last sequence applied, keyed by runtime session.
impl Row for u64 {
    type Key = String;
    const TABLE: Table = Table {
        name: "agent_feed_cursors",
        noun: "agent feed cursor",
        columns: &["tree_id", "runtime_session_id", "applied_seq"],
    };

    fn key_text(key: &Self::Key) -> &str {
        key
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        sql: &str,
        tree_id: &str,
        session: &Self::Key,
    ) -> Result<rusqlite::Result<usize>, String> {
        Ok(tx.execute(sql, params![tree_id, session, to_i64(*self)?]))
    }
}
