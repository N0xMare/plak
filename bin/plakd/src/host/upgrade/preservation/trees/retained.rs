//! Identity evidence for records the live tree cannot silently discard.

use std::collections::BTreeSet;

use plakous_recursive_subagents::tree::RetiredChild;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::super::facts::{Budget, Key, Map, columns, get, insert, scan};
use super::super::{check, unreadable};
use crate::CliResult;

// An unfacted followup can withdraw its opening, attempt and unfacted
// command together. The tree never does this to an admitted followup.
const WITHDRAWABLE: &str = "
    WITH withdrawable AS (
        SELECT a.tree_id, a.attempt_id
        FROM agent_attempts a JOIN agent_operations o ON o.tree_id=a.tree_id
         AND o.operation_id='plak:followup_opened:'||a.attempt_id
         AND o.tool_name='followup_opened'
        WHERE a.status='running' AND a.turn_id IS NULL AND a.candidate_id IS NULL
          AND a.turns=0 AND a.tool_calls=0 AND a.input_tokens=0 AND a.output_tokens=0
    )
";

const ROWS: &[(&str, &str, &str, &str)] = &[
    ("agent_nodes", "agent_id", "agent_id", ""),
    (
        "agent_attempts",
        "attempt_id",
        "agent_id",
        "
        WHERE NOT EXISTS (SELECT 1 FROM withdrawable w
                          WHERE (w.tree_id,w.attempt_id)=(r.tree_id,r.attempt_id))",
    ),
    (
        "agent_commands",
        "command_id",
        "recipient_agent_id",
        "
        WHERE r.fact IS NOT NULL OR r.kind<>'followup'
           OR NOT EXISTS (SELECT 1 FROM withdrawable w
                          WHERE (w.tree_id,w.attempt_id)=(r.tree_id,r.attempt_id))",
    ),
    ("agent_candidates", "candidate_id", "producing_agent_id", ""),
    (
        "agent_operations",
        "operation_id",
        "caller_agent_id",
        "
        WHERE (r.tool_name NOT IN ('vcs_stage','epoch_pending','wait_agent',
                 'returned_admission','retirement_carrier_terminal','candidate_unpin',
                 'workspace_reclamation','child_retirement','behind_retention_command') OR
               r.operation_id='plak:child_collection_watermark')
          AND (r.tool_name<>'followup_opened' OR NOT EXISTS (
                SELECT 1 FROM withdrawable w WHERE w.tree_id=r.tree_id
                  AND r.operation_id='plak:followup_opened:'||w.attempt_id))",
    ),
];

#[derive(Deserialize, Serialize)]
pub(super) struct Retained {
    records: Map<Map<Map<Key>>>,
    retired: Map<BTreeSet<Key>>,
}

impl Retained {
    pub(super) fn capture(
        db: &Connection,
        revision: i64,
        baseline: bool,
        budget: &mut Budget,
    ) -> CliResult<Self> {
        let mut records = Map::new();
        for &(table, id, owner, filter) in ROWS {
            let mut by_tree: Map<Map<Key>> = Map::new();
            let (with, filter) = if baseline {
                (WITHDRAWABLE, filter)
            } else {
                ("", "")
            };
            let query = format!("{with}SELECT r.tree_id,r.{id},r.{owner} FROM {table} r {filter}");
            scan(db, &query, [], table, |row| {
                budget.take()?;
                let tree = by_tree.entry(columns(row, [0])?).or_default();
                insert(tree, columns(row, [1])?, columns(row, [2])?)
            })?;
            records.insert(table.to_owned(), by_tree);
        }
        let mut retired: Map<BTreeSet<Key>> = Map::new();
        if revision >= 12 {
            scan(
                db,
                "SELECT tree_id,agent_id,summary FROM agent_retired_children",
                [],
                "retired children",
                |row| {
                    budget.take()?;
                    let summary: String = get(row, 2)?;
                    let _: RetiredChild = serde_json::from_str(&summary)
                        .map_err(|_| unreadable("retired child summary"))?;
                    retired
                        .entry(columns(row, [0])?)
                        .or_default()
                        .insert(columns(row, [1])?);
                    Ok(())
                },
            )?;
        }
        Ok(Self { records, retired })
    }

    pub(super) fn compare(&self, after: &Self, deleted: &impl Fn(&str) -> bool) -> CliResult<()> {
        for (table, trees) in &self.records {
            let current = after.records.get(table).ok_or_else(|| unreadable(table))?;
            for (tree, records) in trees {
                if deleted(tree) {
                    continue;
                }
                let surviving = current.get(tree);
                let old_retired = self.retired.get(tree);
                let new_retired = after.retired.get(tree);
                check(
                    records.iter().all(|(id, owner)| {
                        surviving.and_then(|rows| rows.get(id)).map_or_else(
                            || {
                                old_retired.is_none_or(|set| !set.contains(owner))
                                    && new_retired.is_some_and(|set| set.contains(owner))
                            },
                            |new| new == owner,
                        )
                    }),
                    &format!(
                        "preservation lost a durable {table} record without a recorded retirement"
                    ),
                )?;
            }
        }
        Ok(())
    }
}
