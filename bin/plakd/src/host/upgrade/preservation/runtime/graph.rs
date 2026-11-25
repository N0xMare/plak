use std::collections::BTreeSet;

use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};

use super::check;
use crate::CliResult;

use super::super::facts::{Budget, Key, Map, columns, get, insert, nonnegative, scan};
use super::super::unreadable;

// The base address of a retained checkpoint may name an already pruned
// parent. Only retained checkpoint roots anchor the reachable object graph.
const REACHABLE: &str = "
    WITH RECURSIVE reached(kind,codec,digest) AS (
        SELECT root_kind,root_codec,root_digest FROM checkpoints
        UNION
        SELECT e.child_kind,e.child_codec,e.child_digest FROM reached r
        JOIN runtime_object_edges e ON (e.parent_kind,e.parent_codec,e.parent_digest)=(r.kind,r.codec,r.digest)
    )";

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Object {
    kind: u8,
    bytes: Key,
    children: Vec<Key>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Graph(Map<Object>);

pub(super) fn object_key(row: &Row<'_>, start: usize) -> CliResult<Key> {
    columns(row, start..start + 3)
}

impl Graph {
    pub(super) fn capture(db: &Connection, budget: &mut Budget) -> CliResult<Self> {
        let mut objects = Map::new();
        scan(
            db,
            &format!(
                "{REACHABLE}
            SELECT r.kind,r.codec,r.digest,o.byte_len,b.bytes FROM reached r
            LEFT JOIN runtime_objects o ON (o.kind,o.codec,o.digest)=(r.kind,r.codec,r.digest)
            LEFT JOIN runtime_object_bytes b ON b.id=o.id"
            ),
            [],
            "runtime object",
            |row| {
                budget.take()?;
                let length = nonnegative(row, 3)?;
                let corrupt = || unreadable("runtime object payload");
                let bytes = row
                    .get_ref(4)
                    .map_err(|_| corrupt())?
                    .as_blob()
                    .map_err(|_| corrupt())?;
                check(
                    length == bytes.len() as u64,
                    "preservation found a malformed retained object",
                )?;
                let kind: u8 = get(row, 0)?;
                check((1..=20).contains(&kind), "unknown runtime object kind")?;
                let _: [u8; 32] = get(row, 2)?;
                insert(
                    &mut objects,
                    object_key(row, 0)?,
                    Object {
                        kind,
                        bytes: columns(row, [4])?,
                        children: Vec::new(),
                    },
                )
            },
        )?;
        scan(
            db,
            &format!(
                "{REACHABLE}
            SELECT r.kind,r.codec,r.digest,e.position,e.child_kind,e.child_codec,e.child_digest
            FROM reached r JOIN runtime_object_edges e
              ON (e.parent_kind,e.parent_codec,e.parent_digest)=(r.kind,r.codec,r.digest)
            ORDER BY r.kind,r.codec,r.digest,e.position"
            ),
            [],
            "runtime edge",
            |row| {
                budget.take()?;
                let parent = objects
                    .get_mut(&object_key(row, 0)?)
                    .ok_or_else(|| unreadable("runtime edge parent"))?;
                check(
                    nonnegative(row, 3)? == parent.children.len() as u64,
                    "invalid runtime edge order",
                )?;
                parent.children.push(object_key(row, 4)?);
                Ok(())
            },
        )?;
        Ok(Self(objects))
    }

    fn child(&self, parent: &str, kind: u8, optional: bool) -> CliResult<Option<&str>> {
        let corrupt = || unreadable("canonical history topology");
        let mut found = None;
        for child in &self.0.get(parent).ok_or_else(corrupt)?.children {
            if self.0.get(child).ok_or_else(corrupt)?.kind == kind {
                check(
                    found.replace(child.as_str()).is_none(),
                    "ambiguous canonical history topology",
                )?;
            }
        }
        if !optional && found.is_none() {
            return Err(corrupt());
        }
        Ok(found)
    }

    fn history(&self, root: &str) -> CliResult<Vec<&str>> {
        let corrupt = || unreadable("canonical history topology");
        let manifest = self.child(root, 2, false)?.ok_or_else(corrupt)?;
        let state = self.child(manifest, 9, false)?.ok_or_else(corrupt)?;
        let mut cursor = self.child(state, 6, true)?;
        let mut reverse = Vec::new();
        while let Some(node) = cursor {
            // The graph bounds the longest possible acyclic history.
            check(reverse.len() < self.0.len(), "cycle in canonical history")?;
            reverse.push(self.child(node, 5, false)?.ok_or_else(corrupt)?);
            cursor = self.child(node, 6, true)?;
        }
        reverse.reverse();
        Ok(reverse)
    }

    pub(super) fn validate_history(&self, root: &str) -> CliResult<()> {
        self.history(root).map(drop)
    }

    pub(super) fn compare_history(&self, root: &str, before: &Self, old: &str) -> CliResult<()> {
        check(
            self.history(root)?.starts_with(&before.history(old)?),
            "preservation found a pruned head without its ordered canonical history prefix",
        )
    }

    pub(super) fn compare<'a>(
        &'a self,
        after: &Self,
        retained: Option<impl Iterator<Item = &'a Key>>,
    ) -> CliResult<()> {
        let pruning = retained.is_some();
        let mut reachable = BTreeSet::new();
        if let Some(roots) = retained {
            let mut walk: Vec<&Key> = roots.collect();
            while let Some(key) = walk.pop() {
                if reachable.insert(key) {
                    walk.extend(
                        &self
                            .0
                            .get(key)
                            .ok_or_else(|| unreadable("runtime graph"))?
                            .children,
                    );
                }
            }
        }
        for (key, object) in &self.0 {
            // History nodes/segments, command records and named-state values
            // remain obligatory even if they were only used by pruned roots.
            let durable = matches!(object.kind, 5 | 6 | 12 | 20);
            check(
                after
                    .0
                    .get(key)
                    .map_or(pruning && !durable && !reachable.contains(key), |current| {
                        current == object
                    }),
                "preservation lost or changed a retained runtime object or its ordered edges",
            )?;
        }
        Ok(())
    }
}
