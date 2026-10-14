//! The ten `agent_*` row codecs.
//!
//! A whole tree in, a whole tree out, over one `rusqlite` transaction the
//! caller hands in. It owns no connection and no file: which database these
//! rows live in is the caller's, which is what lets the tables move from
//! `product.sqlite3` to their own `subagents.sqlite3` without the codec
//! changing a byte.

use rusqlite::{OptionalExtension, Transaction, params};

use std::collections::BTreeMap;
use std::path::PathBuf;

use plakous_recursive_subagents::epoch::OrchestrationSnapshot;
use plakous_recursive_subagents::protocol::{
    AttemptId, CandidateId, TemplateName, TopologyGrant, WriterMode,
};
use plakous_recursive_subagents::tree::{
    AgentCommand, AttemptFailure, AttemptRecord, BlockedReason, CandidateRecord, FoldAnchor,
    MailRecord, NodeRecord, OperationRecord, PersistedTree, ProductAgentTree, SubmitNudgeBudget,
    TreeAdmissionState, UsageRow, WorkspaceRecord,
};

use crate::{from_i64, to_i64};

/// The exact wire spelling of a tree enum, for the `CHECK` constraint that
/// admits it.
///
/// The column stores the serde spelling and nothing else, so a variant renamed
/// on the wire is a schema change rather than a silent write of an unadmitted
/// string.
pub fn agent_tree_enum_text<T: serde::Serialize>(value: T) -> Result<String, String> {
    match serde_json::to_value(value)
        .map_err(|_| "agent tree enum is not serializable".to_owned())?
    {
        serde_json::Value::String(text) => Ok(text),
        _ => Err("agent tree enum encoding is invalid".to_owned()),
    }
}

fn parse_agent_tree_enum<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(&format!("\"{value}\""))
        .map_err(|_| "agent tree enum value is invalid".to_owned())
}

fn parse_sql_enum<T: serde::de::DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    parse_agent_tree_enum(value).map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, "enum".to_owned(), rusqlite::types::Type::Text)
    })
}

fn encode_id_list(values: &[String]) -> Result<String, String> {
    serde_json::to_string(values).map_err(|_| "agent grant id list is not serializable".to_owned())
}

fn decode_id_list(value: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(value).map_err(|_| "agent grant id list is invalid".to_owned())
}

/// Clears every row of one tree, child tables first.
///
/// A save is a whole-tree replace, not a diff: this half runs inside the same
/// transaction as [`insert_agent_tree_rows`], so a tree is never half-written
/// and the foreign keys never see a parent disappear under a child.
pub fn delete_agent_tree_rows(tx: &Transaction<'_>, tree_id: &str) -> Result<(), String> {
    for statement in [
        "DELETE FROM agent_feed_cursors WHERE tree_id=?1",
        "DELETE FROM agent_commands WHERE tree_id=?1",
        "DELETE FROM agent_messages WHERE tree_id=?1",
        "DELETE FROM agent_operations WHERE tree_id=?1",
        "DELETE FROM agent_candidates WHERE tree_id=?1",
        "DELETE FROM agent_attempts WHERE tree_id=?1",
        "DELETE FROM agent_grants WHERE tree_id=?1",
        "DELETE FROM agent_workspaces WHERE tree_id=?1",
        "DELETE FROM agent_nodes WHERE tree_id=?1",
        "DELETE FROM agent_trees WHERE tree_id=?1",
    ] {
        tx.execute(statement, [tree_id])
            .map_err(|_| "could not replace agent tree rows".to_owned())?;
    }
    Ok(())
}

/// Writes every row of one tree: the header, then nodes, grants, attempts,
/// candidates, workspaces, mail and the operation ledger.
///
/// Each insert asserts it changed exactly one row, so a constraint that
/// silently dropped a write fails the save instead of persisting a tree the
/// reopen would read back short.
pub fn insert_agent_tree_rows(tx: &Transaction<'_>, tree: &ProductAgentTree) -> Result<(), String> {
    let changed = tx
        .execute(
            "INSERT INTO agent_trees(tree_id, root_agent_id, revision, host_generation, state, writer_mode, selected_tuple, launch_checkout, snapshot_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                tree.tree_id,
                tree.root_agent_id,
                to_i64(tree.revision)?,
                to_i64(tree.host_generation)?,
                agent_tree_enum_text(tree.state)?,
                agent_tree_enum_text(tree.writer_mode)?,
                tree.selected_tuple,
                tree.launch_checkout.to_string_lossy().as_ref(),
                serde_json::to_string(&tree.snapshot)
                    .map_err(|_| "orchestration snapshot is not serializable".to_owned())?,
            ],
        )
        .map_err(|_| "could not insert agent tree row".to_owned())?;
    if changed != 1 {
        return Err("agent tree insert was not exact".to_owned());
    }
    for node in tree.nodes().values() {
        let changed = tx
            .execute(
                "INSERT INTO agent_nodes(tree_id, agent_id, parent_agent_id, depth, description, prompt, output_schema_json, schema_digest, template, tuple_id, account, effort, status, workspace_id, tier, attempt_id, session_locator, path, branch, active_turn, fold_order) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
                params![
                    tree.tree_id,
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
            )
            .map_err(|_| "could not insert agent node row".to_owned())?;
        if changed != 1 {
            return Err("agent node insert was not exact".to_owned());
        }
    }
    for (agent_id, grant) in tree.grants() {
        let changed = tx
            .execute(
                "INSERT INTO agent_grants(tree_id, agent_id, may_spawn, may_adopt, max_depth, max_nodes, max_children, visible_ids, message_peers, managed_ids, interrupt_ids, close_ids) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    tree.tree_id,
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
            )
            .map_err(|_| "could not insert agent grant row".to_owned())?;
        if changed != 1 {
            return Err("agent grant insert was not exact".to_owned());
        }
    }
    for attempt in tree.attempts().values() {
        let changed = tx
            .execute(
                "INSERT INTO agent_attempts(tree_id, attempt_id, agent_id, status, turn_token, turn_id, output_json, submit_harness_nudge, candidate_id, retry_of, request_key, started_at, ended_at, blocked_kind, blocked_account, blocked_resets_at, failure_kind, failure_message, elapsed_ms, turns, tool_calls, input_tokens, output_tokens, spawn_identity, output_schema_json, schema_digest, applied_terminal_read_seqs) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)",
                params![
                    tree.tree_id,
                    attempt.attempt_id.as_str(),
                    attempt.agent_id,
                    agent_tree_enum_text(attempt.status)?,
                    to_i64(attempt.turn_token)?,
                    attempt.turn_id,
                    attempt.output_json,
                    i64::from(attempt.submit_harness_nudge.rounds()),
                    attempt.candidate_id.as_ref().map(CandidateId::as_str),
                    attempt.retry_of.as_ref().map(AttemptId::as_str),
                    attempt.request_key,
                    to_i64(attempt.started_at)?,
                    attempt.ended_at.map(to_i64).transpose()?,
                    attempt.blocked.as_ref().map(|blocked| blocked.quota.as_str()),
                    attempt.blocked.as_ref().map(|blocked| blocked.account.as_str()),
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
                    attempt.spawn_identity.as_ref().map(serde_json::to_string).transpose()
                        .map_err(|_| "spawn identity is not serializable".to_owned())?,
                    attempt.output_schema_json,
                    attempt.schema_digest,
                    serde_json::to_string(&attempt.applied_terminal_read_seqs)
                        .map_err(|_| "applied terminal reads are not serializable".to_owned())?,
                ],
            )
            .map_err(|_| "could not insert agent attempt row".to_owned())?;
        if changed != 1 {
            return Err("agent attempt insert was not exact".to_owned());
        }
    }
    for candidate in tree.candidates().values() {
        let changed = tx
            .execute(
                "INSERT INTO agent_candidates(tree_id, candidate_id, workspace_id, producing_agent_id, producing_attempt_id, base_commit, base_content, base_change_id, commit_id, content, change_id, sealed_at, adopted_by, adopted_evidence, merged_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    tree.tree_id,
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
                ],
            )
            .map_err(|_| "could not insert agent candidate row".to_owned())?;
        if changed != 1 {
            return Err("agent candidate insert was not exact".to_owned());
        }
    }
    for workspace in tree.workspaces().values() {
        let changed = tx
            .execute(
                "INSERT INTO agent_workspaces(tree_id, workspace_id, owner_agent_id, path, mode, status, base_commit, base_content, base_change_id, generation, adopter_agent_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    tree.tree_id,
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
                ],
            )
            .map_err(|_| "could not insert agent workspace row".to_owned())?;
        if changed != 1 {
            return Err("agent workspace insert was not exact".to_owned());
        }
    }
    for mail in tree.messages().values() {
        let changed = tx
            .execute(
                "INSERT INTO agent_messages(tree_id, message_id, from_agent, to_agent, body, purpose, priority, in_reply_to, disposition, durable) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    tree.tree_id,
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
            )
            .map_err(|_| "could not insert agent message row".to_owned())?;
        if changed != 1 {
            return Err("agent message insert was not exact".to_owned());
        }
    }
    for operation in tree.operations().values() {
        let terminal_code = operation
            .terminal_code
            .map(agent_tree_enum_text)
            .transpose()?;
        let changed = tx
            .execute(
                "INSERT INTO agent_operations(tree_id, operation_id, caller_agent_id, tool_name, canonical_request, request_digest, terminal_code, terminal_response, fold_anchor, fold_order) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    tree.tree_id,
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
            )
            .map_err(|_| "could not insert agent operation row".to_owned())?;
        if changed != 1 {
            return Err("agent operation insert was not exact".to_owned());
        }
    }
    for command in tree.commands().values() {
        let changed = tx
            .execute(
                "INSERT INTO agent_commands(tree_id, command_id, recipient_agent_id, attempt_id, kind, canonical_text, message_ids, withdrawn, fact, turn_id, accepted_seq) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    tree.tree_id,
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
            )
            .map_err(|_| "could not insert agent command row".to_owned())?;
        if changed != 1 {
            return Err("agent command insert was not exact".to_owned());
        }
    }
    for (session, applied) in tree.feed_cursors() {
        let changed = tx
            .execute(
                "INSERT INTO agent_feed_cursors(tree_id, runtime_session_id, applied_seq) VALUES (?1,?2,?3)",
                params![tree.tree_id, session, to_i64(*applied)?],
            )
            .map_err(|_| "could not insert agent feed cursor row".to_owned())?;
        if changed != 1 {
            return Err("agent feed cursor insert was not exact".to_owned());
        }
    }
    Ok(())
}

/// Reads one tree back as the rows the store holds, or `None` when the file
/// carries no such tree.
///
/// The result is deliberately [`PersistedTree`] and not a live tree: the
/// reopen law — paused admission, no restored turn — is the tree's to apply,
/// not the store's.
pub fn load_agent_tree(
    tx: &Transaction<'_>,
    tree_id: &str,
) -> Result<Option<PersistedTree>, String> {
    let header = tx
        .query_row(
            "SELECT root_agent_id, revision, host_generation, state, writer_mode, selected_tuple, launch_checkout, snapshot_json FROM agent_trees WHERE tree_id=?1",
            [tree_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    from_i64(row.get(1)?)?,
                    from_i64(row.get(2)?)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    PathBuf::from(row.get::<_, String>(6)?),
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|_| "agent tree row is invalid".to_owned())?;
    let Some((
        root_agent_id,
        revision,
        host_generation,
        state,
        writer_mode,
        selected_tuple,
        launch_checkout,
        snapshot_json,
    )) = header
    else {
        return Ok(None);
    };
    let state = parse_agent_tree_enum::<TreeAdmissionState>(&state)?;
    let writer_mode = parse_agent_tree_enum::<WriterMode>(&writer_mode)?;
    let snapshot: OrchestrationSnapshot = serde_json::from_str(&snapshot_json)
        .map_err(|_| "orchestration snapshot is invalid".to_owned())?;
    let mut nodes = BTreeMap::new();
    let mut node_statement = tx
        .prepare(
            "SELECT agent_id, parent_agent_id, depth, description, prompt, output_schema_json, schema_digest, template, tuple_id, account, effort, status, workspace_id, tier, attempt_id, session_locator, path, branch, active_turn, fold_order FROM agent_nodes WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent nodes".to_owned())?;
    let node_rows = node_statement
        .query_map([tree_id], |row| {
            let template = row
                .get::<_, Option<String>>(7)?
                .map(|text| {
                    TemplateName::parse(&text).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            7,
                            "template".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })
                })
                .transpose()?;
            Ok(NodeRecord {
                // Not persisted: it describes one reopen, and the face that
                // reads it is the one running when that reopen happened. A
                // restart has nothing to say about a turn it did not abandon.
                abandoned_turn: None,
                agent_id: row.get(0)?,
                parent_agent_id: row.get(1)?,
                depth: u8::try_from(row.get::<_, i64>(2)?).map_err(|_| {
                    rusqlite::Error::InvalidColumnType(
                        2,
                        "depth".to_owned(),
                        rusqlite::types::Type::Integer,
                    )
                })?,
                description: row.get(3)?,
                prompt: row.get(4)?,
                output_schema_json: row.get(5)?,
                schema_digest: row.get(6)?,
                template,
                tuple_id: row.get(8)?,
                account: row.get(9)?,
                effort: row.get(10)?,
                status: parse_sql_enum(&row.get::<_, String>(11)?)?,
                workspace_id: row.get(12)?,
                tier: parse_sql_enum(&row.get::<_, String>(13)?)?,
                attempt_id: row
                    .get::<_, Option<String>>(14)?
                    .map(AttemptId::from_persisted),
                session_locator: row.get(15)?,
                path: PathBuf::from(row.get::<_, String>(16)?),
                branch: row.get(17)?,
                active_turn: row.get::<_, i64>(18)? != 0,
                fold_order: row.get::<_, Option<i64>>(19)?.map(from_i64).transpose()?,
            })
        })
        .map_err(|_| "could not query agent nodes".to_owned())?;
    for node in node_rows {
        let node = node.map_err(|_| "agent node row is invalid".to_owned())?;
        nodes.insert(node.agent_id.clone(), node);
    }
    drop(node_statement);
    let mut grants = BTreeMap::new();
    let mut grant_statement = tx
        .prepare(
            "SELECT agent_id, may_spawn, max_depth, max_nodes, max_children, visible_ids, message_peers, managed_ids, interrupt_ids, close_ids, may_adopt FROM agent_grants WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent grants".to_owned())?;
    let grant_rows = grant_statement
        .query_map([tree_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                TopologyGrant {
                    visible_ids: decode_id_list(&row.get::<_, String>(5)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            5,
                            "visible_ids".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                    message_peers: decode_id_list(&row.get::<_, String>(6)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            6,
                            "message_peers".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                    managed_ids: decode_id_list(&row.get::<_, String>(7)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            7,
                            "managed_ids".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                    interrupt_ids: decode_id_list(&row.get::<_, String>(8)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            8,
                            "interrupt_ids".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                    close_ids: decode_id_list(&row.get::<_, String>(9)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            9,
                            "close_ids".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                    may_spawn: row.get::<_, i64>(1)? != 0,
                    may_adopt: row.get::<_, i64>(10)? != 0,
                    max_depth: u8::try_from(row.get::<_, i64>(2)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            2,
                            "max_depth".to_owned(),
                            rusqlite::types::Type::Integer,
                        )
                    })?,
                    max_nodes: u8::try_from(row.get::<_, i64>(3)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            3,
                            "max_nodes".to_owned(),
                            rusqlite::types::Type::Integer,
                        )
                    })?,
                    max_children: u8::try_from(row.get::<_, i64>(4)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            4,
                            "max_children".to_owned(),
                            rusqlite::types::Type::Integer,
                        )
                    })?,
                },
            ))
        })
        .map_err(|_| "could not query agent grants".to_owned())?;
    for grant in grant_rows {
        let (agent_id, grant) = grant.map_err(|_| "agent grant row is invalid".to_owned())?;
        grants.insert(agent_id, grant);
    }
    drop(grant_statement);
    let mut attempts = BTreeMap::new();
    let mut attempt_statement = tx
        .prepare(
            "SELECT attempt_id, agent_id, status, turn_token, turn_id, output_json, submit_harness_nudge, candidate_id, retry_of, request_key, started_at, ended_at, blocked_kind, blocked_account, blocked_resets_at, failure_kind, failure_message, elapsed_ms, turns, tool_calls, input_tokens, output_tokens, spawn_identity, output_schema_json, schema_digest, applied_terminal_read_seqs FROM agent_attempts WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent attempts".to_owned())?;
    let attempt_rows = attempt_statement
        .query_map([tree_id], |row| {
            let blocked = match (
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
            ) {
                (Some(quota), Some(account)) => Some(BlockedReason {
                    quota,
                    account,
                    resets_at: row.get::<_, Option<i64>>(14)?.map(from_i64).transpose()?,
                }),
                _ => None,
            };
            Ok(AttemptRecord {
                output_schema_json: row.get(23)?,
                schema_digest: row.get(24)?,
                applied_terminal_read_seqs: serde_json::from_str(&row.get::<_, String>(25)?)
                    .map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            25,
                            "applied_terminal_read_seqs".to_owned(),
                            rusqlite::types::Type::Text,
                        )
                    })?,
                attempt_id: AttemptId::from_persisted(row.get::<_, String>(0)?),
                agent_id: row.get(1)?,
                status: parse_sql_enum(&row.get::<_, String>(2)?)?,
                turn_token: from_i64(row.get(3)?)?,
                turn_id: row.get(4)?,
                output_json: row.get(5)?,
                submit_harness_nudge: {
                    let value: i64 = row.get(6)?;
                    // The column's own CHECK admits 0..=2; a row past the
                    // ceiling is a corrupt file, not a budget to clamp.
                    u8::try_from(value)
                        .ok()
                        .and_then(SubmitNudgeBudget::spent)
                        .ok_or_else(|| {
                            rusqlite::Error::InvalidColumnType(
                                6,
                                "submit_harness_nudge".to_owned(),
                                rusqlite::types::Type::Integer,
                            )
                        })?
                },
                candidate_id: row
                    .get::<_, Option<String>>(7)?
                    .map(CandidateId::from_persisted),
                retry_of: row
                    .get::<_, Option<String>>(8)?
                    .map(AttemptId::from_persisted),
                request_key: row.get(9)?,
                spawn_identity: row
                    .get::<_, Option<String>>(22)?
                    .map(|value| {
                        serde_json::from_str(&value).map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                22,
                                "spawn_identity".to_owned(),
                                rusqlite::types::Type::Text,
                            )
                        })
                    })
                    .transpose()?,
                started_at: from_i64(row.get(10)?)?,
                ended_at: row.get::<_, Option<i64>>(11)?.map(from_i64).transpose()?,
                blocked,
                failure: match (
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                ) {
                    (Some(kind), Some(message)) => Some(AttemptFailure { kind, message }),
                    _ => None,
                },
                usage: UsageRow {
                    elapsed_ms: from_i64(row.get(17)?)?,
                    turns: u32::try_from(row.get::<_, i64>(18)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            18,
                            "turns".to_owned(),
                            rusqlite::types::Type::Integer,
                        )
                    })?,
                    tool_calls: u32::try_from(row.get::<_, i64>(19)?).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            19,
                            "tool_calls".to_owned(),
                            rusqlite::types::Type::Integer,
                        )
                    })?,
                    input_tokens: from_i64(row.get(20)?)?,
                    output_tokens: from_i64(row.get(21)?)?,
                },
            })
        })
        .map_err(|_| "could not query agent attempts".to_owned())?;
    for attempt in attempt_rows {
        let attempt = attempt.map_err(|_| "agent attempt row is invalid".to_owned())?;
        attempts.insert(attempt.attempt_id.clone(), attempt);
    }
    drop(attempt_statement);
    let mut candidates = BTreeMap::new();
    let mut candidate_statement = tx
        .prepare(
            "SELECT candidate_id, workspace_id, producing_agent_id, producing_attempt_id, base_commit, base_content, base_change_id, commit_id, content, change_id, sealed_at, adopted_by, adopted_evidence, merged_at FROM agent_candidates WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent candidates".to_owned())?;
    let candidate_rows = candidate_statement
        .query_map([tree_id], |row| {
            Ok(CandidateRecord {
                candidate_id: CandidateId::from_persisted(row.get::<_, String>(0)?),
                workspace_id: row.get(1)?,
                producing_agent_id: row.get(2)?,
                producing_attempt_id: AttemptId::from_persisted(row.get::<_, String>(3)?),
                base_commit: row.get(4)?,
                base_content: row.get(5)?,
                base_change_id: row.get(6)?,
                commit: row.get(7)?,
                content: row.get(8)?,
                change_id: row.get(9)?,
                sealed_at: from_i64(row.get(10)?)?,
                adopted_by: row.get(11)?,
                merged_at: row.get(13)?,
                adopted_evidence: serde_json::from_str(&row.get::<_, String>(12)?)
                    .unwrap_or_default(),
            })
        })
        .map_err(|_| "could not query agent candidates".to_owned())?;
    for candidate in candidate_rows {
        let candidate = candidate.map_err(|_| "agent candidate row is invalid".to_owned())?;
        candidates.insert(candidate.candidate_id.clone(), candidate);
    }
    drop(candidate_statement);
    let mut workspaces = BTreeMap::new();
    let mut workspace_statement = tx
        .prepare(
            "SELECT workspace_id, owner_agent_id, path, mode, status, base_commit, base_content, base_change_id, generation, adopter_agent_id FROM agent_workspaces WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent workspaces".to_owned())?;
    let workspace_rows = workspace_statement
        .query_map([tree_id], |row| {
            Ok(WorkspaceRecord {
                workspace_id: row.get(0)?,
                owner_agent_id: row.get(1)?,
                path: PathBuf::from(row.get::<_, String>(2)?),
                mode: parse_sql_enum(&row.get::<_, String>(3)?)?,
                status: parse_sql_enum(&row.get::<_, String>(4)?)?,
                base_commit: row.get(5)?,
                base_content: row.get(6)?,
                base_change_id: row.get(7)?,
                generation: from_i64(row.get(8)?)?,
                adopter_agent_id: row.get(9)?,
            })
        })
        .map_err(|_| "could not query agent workspaces".to_owned())?;
    for workspace in workspace_rows {
        let workspace = workspace.map_err(|_| "agent workspace row is invalid".to_owned())?;
        workspaces.insert(workspace.workspace_id.clone(), workspace);
    }
    drop(workspace_statement);
    let mut messages = BTreeMap::new();
    let mut message_statement = tx
        .prepare(
            "SELECT message_id, from_agent, to_agent, body, purpose, priority, in_reply_to, disposition, durable FROM agent_messages WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent messages".to_owned())?;
    let message_rows = message_statement
        .query_map([tree_id], |row| {
            Ok(MailRecord {
                message_id: row.get(0)?,
                from: row.get(1)?,
                to: row.get(2)?,
                body: row.get(3)?,
                purpose: parse_sql_enum(&row.get::<_, String>(4)?)?,
                priority: parse_sql_enum(&row.get::<_, String>(5)?)?,
                in_reply_to: row.get(6)?,
                disposition: parse_sql_enum(&row.get::<_, String>(7)?)?,
                durable: parse_sql_enum(&row.get::<_, String>(8)?)?,
            })
        })
        .map_err(|_| "could not query agent messages".to_owned())?;
    for message in message_rows {
        let message = message.map_err(|_| "agent message row is invalid".to_owned())?;
        let sequence = message
            .message_id
            .strip_prefix('m')
            .and_then(|sequence| sequence.parse::<usize>().ok());
        if !sequence
            .is_some_and(|sequence| sequence > 0 && message.message_id == format!("m{sequence}"))
        {
            return Err("agent message sequence identity is invalid".to_owned());
        }
        messages.insert(message.message_id.clone(), message);
    }
    drop(message_statement);
    let mut operations = BTreeMap::new();
    let mut operation_statement = tx
        .prepare(
            "SELECT operation_id, caller_agent_id, tool_name, canonical_request, request_digest, terminal_code, terminal_response, fold_anchor, fold_order FROM agent_operations WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent operations".to_owned())?;
    let operation_rows = operation_statement
        .query_map([tree_id], |row| {
            let terminal_code = match row.get::<_, Option<String>>(5)? {
                Some(value) => Some(parse_sql_enum(&value)?),
                None => None,
            };
            Ok(OperationRecord {
                operation_id: row.get(0)?,
                caller_agent_id: row.get(1)?,
                tool_name: row.get(2)?,
                canonical_request: row.get(3)?,
                request_digest: row.get(4)?,
                terminal_code,
                terminal_response: row.get(6)?,
                fold: match (row.get::<_, Option<i64>>(7)?, row.get::<_, Option<i64>>(8)?) {
                    (None, None) => None,
                    (Some(seq), Some(order)) => Some(FoldAnchor {
                        seq: from_i64(seq)?,
                        order: from_i64(order)?,
                    }),
                    _ => {
                        return Err(rusqlite::Error::InvalidColumnType(
                            7,
                            "fold anchor pair".to_owned(),
                            rusqlite::types::Type::Null,
                        ));
                    }
                },
            })
        })
        .map_err(|_| "could not query agent operations".to_owned())?;
    for operation in operation_rows {
        let operation = operation.map_err(|_| "agent operation row is invalid".to_owned())?;
        operations.insert(operation.operation_id.clone(), operation);
    }
    drop(operation_statement);
    let commands = load_commands(tx, tree_id)?;
    let feed_cursors = load_feed_cursors(tx, tree_id)?;
    Ok(Some(
        PersistedTree::from_rows(
            tree_id.to_owned(),
            root_agent_id,
            revision,
            host_generation,
            state,
            writer_mode,
            selected_tuple,
            launch_checkout,
            snapshot,
            nodes,
            grants,
            attempts,
            candidates,
            workspaces,
            messages,
            operations,
        )
        .with_feed_rows(commands, feed_cursors),
    ))
}

fn load_commands(
    tx: &Transaction<'_>,
    tree_id: &str,
) -> Result<BTreeMap<String, AgentCommand>, String> {
    let mut statement = tx
        .prepare(
            "SELECT command_id, recipient_agent_id, attempt_id, kind, canonical_text, message_ids, withdrawn, fact, turn_id, accepted_seq FROM agent_commands WHERE tree_id=?1",
        )
        .map_err(|_| "could not query agent commands".to_owned())?;
    let rows = statement
        .query_map([tree_id], |row| {
            let fact = match row.get::<_, Option<String>>(7)? {
                Some(value) => Some(parse_sql_enum(&value)?),
                None => None,
            };
            Ok((
                AgentCommand {
                    command_id: row.get(0)?,
                    recipient: row.get(1)?,
                    attempt_id: row
                        .get::<_, Option<String>>(2)?
                        .map(AttemptId::from_persisted),
                    kind: parse_sql_enum(&row.get::<_, String>(3)?)?,
                    text: row.get(4)?,
                    message_ids: Vec::new(),
                    withdrawn: row.get::<_, i64>(6)? == 1,
                    fact,
                    accepted_seq: row.get::<_, Option<i64>>(9)?.map(from_i64).transpose()?,
                    turn_id: row.get(8)?,
                },
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|_| "could not query agent commands".to_owned())?;
    let mut commands = BTreeMap::new();
    for row in rows {
        let (mut command, message_ids) =
            row.map_err(|_| "agent command row is invalid".to_owned())?;
        command.message_ids = decode_id_list(&message_ids)?;
        commands.insert(command.command_id.clone(), command);
    }
    Ok(commands)
}

fn load_feed_cursors(tx: &Transaction<'_>, tree_id: &str) -> Result<BTreeMap<String, u64>, String> {
    let mut statement = tx
        .prepare("SELECT runtime_session_id, applied_seq FROM agent_feed_cursors WHERE tree_id=?1")
        .map_err(|_| "could not query agent feed cursors".to_owned())?;
    let rows = statement
        .query_map([tree_id], |row| {
            Ok((row.get::<_, String>(0)?, from_i64(row.get(1)?)?))
        })
        .map_err(|_| "could not query agent feed cursors".to_owned())?;
    let mut cursors = BTreeMap::new();
    for row in rows {
        let (session, applied) = row.map_err(|_| "agent feed cursor row is invalid".to_owned())?;
        cursors.insert(session, applied);
    }
    Ok(cursors)
}
