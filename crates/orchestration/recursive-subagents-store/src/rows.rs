//! The ten `agent_*` row codecs.
//!
//! A whole tree in, a whole tree out, or single keyed rows for a row-level
//! save, over one `rusqlite` transaction the caller hands in. It owns no
//! connection and no file: which database these rows live in is the
//! caller's, which is what lets the tables move from `product.sqlite3` to
//! their own `subagents.sqlite3` without the codec changing a byte.

mod codec;

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
    TreeAdmissionState, TreeStoreError, UsageRow, WorkspaceRecord,
};

pub(crate) use codec::Row;

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

/// Clears every row of one tree, child tables first, so the foreign keys
/// never see a parent disappear under a child.
pub fn delete_agent_tree_rows(tx: &Transaction<'_>, tree_id: &str) -> Result<(), TreeStoreError> {
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
        tx.execute(statement, [tree_id]).map_err(|error| {
            crate::store::sqlite_write_error(error, "could not replace agent tree rows".to_owned())
        })?;
    }
    Ok(())
}

/// Writes every row of one tree: the header, then nodes, grants, attempts,
/// candidates, workspaces, mail and the operation ledger.
///
/// Each insert asserts it changed exactly one row, so a constraint that
/// silently dropped a write fails the save instead of persisting a tree the
/// reopen would read back short.
pub fn insert_agent_tree_rows(
    tx: &Transaction<'_>,
    tree: &ProductAgentTree,
) -> Result<(), TreeStoreError> {
    let header = Header::of(tree)?;
    let changed = tx
        .execute(
            "INSERT INTO agent_trees(tree_id, root_agent_id, revision, host_generation, state, writer_mode, selected_tuple, launch_checkout, snapshot_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                tree.tree_id,
                tree.root_agent_id,
                header.revision,
                header.host_generation,
                header.state,
                header.writer_mode,
                tree.selected_tuple,
                header.launch_checkout,
                header.snapshot_json,
            ],
        )
        .map_err(|error| crate::store::sqlite_write_error(error, "could not insert agent tree row".to_owned()))?;
    if changed != 1 {
        return Err("agent tree insert was not exact".to_owned().into());
    }
    let tree_id = tree.tree_id.as_str();
    insert_rows(tx, tree_id, tree.nodes())?;
    insert_rows(tx, tree_id, tree.grants())?;
    insert_rows(tx, tree_id, tree.attempts())?;
    insert_rows(tx, tree_id, tree.candidates())?;
    insert_rows(tx, tree_id, tree.workspaces())?;
    insert_rows(tx, tree_id, tree.messages())?;
    insert_rows(tx, tree_id, tree.operations())?;
    insert_rows(tx, tree_id, tree.commands())?;
    insert_rows(tx, tree_id, tree.feed_cursors())
}

/// The compare-and-swap of a row-level save: rewrites the header only while
/// the store still holds `tree.stored_revision`, and returns how many rows
/// that changed, 0 or 1.
pub(crate) fn swap_agent_tree_header(
    tx: &Transaction<'_>,
    tree: &ProductAgentTree,
) -> Result<usize, TreeStoreError> {
    let header = Header::of(tree)?;
    tx.execute(
        "UPDATE agent_trees SET root_agent_id=?2, revision=?3, host_generation=?4, state=?5, writer_mode=?6, selected_tuple=?7, launch_checkout=?8, snapshot_json=?9 WHERE tree_id=?1 AND revision=?10",
        params![
            tree.tree_id,
            tree.root_agent_id,
            header.revision,
            header.host_generation,
            header.state,
            header.writer_mode,
            tree.selected_tuple,
            header.launch_checkout,
            header.snapshot_json,
            to_i64(tree.stored_revision)?,
        ],
    )
    .map_err(|error| crate::store::sqlite_write_error(error, "could not update agent tree row".to_owned()))
}

/// The `agent_trees` columns that are not plain text, encoded once for
/// either the insert or the compare-and-swap.
struct Header {
    revision: i64,
    host_generation: i64,
    state: String,
    writer_mode: String,
    launch_checkout: String,
    snapshot_json: String,
}

impl Header {
    fn of(tree: &ProductAgentTree) -> Result<Self, String> {
        Ok(Self {
            revision: to_i64(tree.revision)?,
            host_generation: to_i64(tree.host_generation)?,
            state: agent_tree_enum_text(tree.state)?,
            writer_mode: agent_tree_enum_text(tree.writer_mode)?,
            launch_checkout: tree.launch_checkout.to_string_lossy().into_owned(),
            snapshot_json: serde_json::to_string(&tree.snapshot)
                .map_err(|_| "orchestration snapshot is not serializable".to_owned())?,
        })
    }
}

fn insert_rows<R: Row>(
    tx: &Transaction<'_>,
    tree_id: &str,
    rows: &BTreeMap<R::Key, R>,
) -> Result<(), TreeStoreError> {
    let sql = R::TABLE.insert_sql();
    for (key, row) in rows {
        write_row(tx, &sql, "insert", tree_id, key, row)?;
    }
    Ok(())
}

/// Writes one row with `sql`, an insert or an upsert of `R::TABLE`, and
/// requires it to change exactly one row. `verb` names the write in the
/// error: `insert` for the whole-tree path, `upsert` for the row path.
pub(crate) fn write_row<R: Row>(
    tx: &Transaction<'_>,
    sql: &str,
    verb: &str,
    tree_id: &str,
    key: &R::Key,
    row: &R,
) -> Result<(), TreeStoreError> {
    let noun = R::TABLE.noun;
    let changed = row.write(tx, sql, tree_id, key)?.map_err(|error| {
        crate::store::sqlite_write_error(error, format!("could not {verb} {noun} row"))
    })?;
    if changed != 1 {
        return Err(format!("{noun} {verb} was not exact").into());
    }
    Ok(())
}

/// Deletes one keyed row of `R::TABLE`, requiring it to have existed.
pub(crate) fn delete_row<R: Row>(
    tx: &Transaction<'_>,
    sql: &str,
    tree_id: &str,
    key: &R::Key,
) -> Result<(), TreeStoreError> {
    let noun = R::TABLE.noun;
    let changed = tx
        .execute(sql, params![tree_id, R::key_text(key)])
        .map_err(|error| {
            crate::store::sqlite_write_error(error, format!("could not delete {noun} row"))
        })?;
    if changed != 1 {
        return Err(format!("{noun} delete was not exact").into());
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
