use std::collections::BTreeSet;

use plakous::sessions::SessionLocator;
use plakous_hosted::sessions::projector::{Submission, SubmissionKind};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::check;
use crate::CliResult;
use crate::home::paths::PlakPaths;
use crate::store::{queue_tombstone_digest, revisions::databases};

use super::facts::{
    Budget, Key, Map, branch_key, columns, finish, get, insert, locator_branch, nonnegative, scan,
    subset, texts,
};
use super::unreadable;

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Root {
    stable: Key,
    archived: bool,
    branch: Option<Key>,
}

#[derive(Deserialize, Serialize)]
struct Draft {
    id: Key,
    content: Key,
    witness: Option<Key>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Product {
    instance: Key,
    roots: Map<Root>,
    queues: Map<Vec<Draft>>,
}

pub(super) struct Admission {
    pub locator: SessionLocator,
    pub anchor: u64,
    pub command: Key,
    pub kind: SubmissionKind,
    pub target: Option<Key>,
    pub draft: Key,
}

impl Product {
    pub(super) fn compare(&self, after: &Self, accepted: &BTreeSet<Key>) -> CliResult<()> {
        check(
            self.instance == after.instance,
            "preservation changed the product store identity",
        )?;
        for predicate in [(|root: &Root| root.archived) as fn(&Root) -> bool, |root| {
            !root.archived && root.branch.is_none()
        }] {
            let select = |(_, root): &(&Key, &Root)| predicate(root);
            check(
                self.roots.iter().filter(select).map(|(id, _)| id).eq(after
                    .roots
                    .iter()
                    .filter(select)
                    .map(|(id, _)| id)),
                "preservation changed the archived or unstarted root head set",
            )?;
        }
        subset(&self.roots, &after.roots, "product roots")?;
        for (root, before) in &self.queues {
            let current = after.queues.get(root).map_or(&[][..], Vec::as_slice);
            let surviving = ids(current);
            let mut suffix = current;
            for item in before {
                if suffix
                    .first()
                    .is_some_and(|next| next.content == item.content && next.id == item.id)
                {
                    suffix = &suffix[1..];
                } else {
                    check(
                        !surviving.contains(item.id.as_str())
                            && item.witness.is_some()
                            && accepted.contains(&item.id),
                        "preservation changed a queued prefix without exact durable admission",
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn ids(queue: &[Draft]) -> BTreeSet<&str> {
    queue.iter().map(|draft| draft.id.as_str()).collect()
}

pub(super) fn capture(
    paths: &PlakPaths,
    before: Option<&Product>,
    budget: &mut Budget,
) -> CliResult<(Product, Vec<Admission>)> {
    let [(schema, path), _] = databases(paths);
    schema
        .inspect_existing(&path, budget.deadline(), |db, _| {
            capture_product(db, before, budget)
        })
        .map_err(|_| unreadable("product store"))?
}

fn capture_product(
    db: &Connection,
    before: Option<&Product>,
    budget: &mut Budget,
) -> CliResult<(Product, Vec<Admission>)> {
    super::facts::configure_reader(db)?;
    let instance: String = db
        .query_row(
            "SELECT instance_id FROM product_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| unreadable("product schema identity"))?;
    let mut roots = Map::new();
    let mut queues = Map::new();
    let mut admissions = Vec::new();
    scan(db, "
        WITH heads AS (
          SELECT id, row_number() OVER (PARTITION BY node_tree_id ORDER BY updated_at DESC,id) AS pos
          FROM sessions s WHERE NOT EXISTS (
            SELECT 1 FROM sessions next WHERE next.source_session_id=s.id
              AND next.node_tree_id=s.node_tree_id AND next.node_agent_id=s.node_agent_id)
        )
        SELECT s.id,archived,locator,node_tree_id,node_agent_id,account,model,effort,composition,
               workspace_root,workspace_identity,root_profile,runner_display,target_selection,
               frozen_tools,created_at,create_operation_id,source_session_id,source_operation_id
        FROM heads JOIN sessions s ON s.id=heads.id WHERE pos=1 ORDER BY s.id
    ", [], "product root", |row| {
        budget.take()?;
        let id: String = get(row, 0)?;
        let key = texts([id.as_str()]);
        let locator = get::<Option<String>>(row, 2)?.as_deref().map(locator_branch).transpose()?;
        let root = Root {
            stable: columns(row, 3..19)?,
            archived: get(row, 1)?,
            branch: locator.map(branch_key),
        };
        let queue = queue(db, &id, budget)?;
        if let Some(locator) = locator
            && let Some(prior) = before.and_then(|snapshot| snapshot.queues.get(&key))
        {
            let surviving = ids(&queue);
            let wanted = prior.iter().filter_map(|item| {
                let witness = item.witness.as_ref()?;
                (!surviving.contains(item.id.as_str())).then_some((item.id.clone(), witness))
            }).collect();
            read_admissions(db, &id, locator, &wanted, &mut admissions, budget)?;
        }
        if !queue.is_empty() {
            insert(&mut queues, key.clone(), queue)?;
        }
        insert(&mut roots, key, root)
    })?;
    Ok((
        Product {
            instance: texts([instance.as_str()]),
            roots,
            queues,
        },
        admissions,
    ))
}

fn queue(db: &Connection, root: &str, budget: &mut Budget) -> CliResult<Vec<Draft>> {
    let mut result = Vec::new();
    scan(
        db,
        "
        SELECT item_id,revision,text,submitted_at FROM queue_items
        WHERE session_id=?1 ORDER BY position,item_id
    ",
        [root],
        "queue",
        |row| {
            budget.take()?;
            let item: String = get(row, 0)?;
            let revision = nonnegative(row, 1)?;
            let draft = columns(row, 0..4)?;
            let mut content = Sha256::new();
            content.update(draft.as_bytes());
            let mut ordinal = 0_u64;
            scan(
                db,
                "
            SELECT ordinal,name,media_type,bytes FROM queue_attachments
            WHERE item_id=?1 ORDER BY ordinal
        ",
                [&item],
                "queue attachment",
                |image| {
                    budget.take()?;
                    check(
                        nonnegative(image, 0)? == ordinal,
                        "invalid queue attachment order",
                    )?;
                    ordinal += 1;
                    content.update(columns(image, 1..4)?.as_bytes());
                    Ok(())
                },
            )?;
            let witness = if ordinal == 0 {
                let text = row.get_ref(2).map_err(|_| unreadable("queue text"))?;
                let text = text.as_str().map_err(|_| unreadable("queue text"))?;
                let rev = revision.to_string();
                Some(texts([
                    rev.as_str(),
                    queue_tombstone_digest(root, &rev, text).as_str(),
                ]))
            } else {
                None
            };
            result.push(Draft {
                id: texts([item.as_str()]),
                content: finish(content),
                witness,
            });
            Ok(())
        },
    )?;
    Ok(result)
}

fn read_admissions(
    db: &Connection,
    root: &str,
    locator: SessionLocator,
    wanted: &Map<&Key>,
    admissions: &mut Vec<Admission>,
    budget: &mut Budget,
) -> CliResult<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    // The tombstone binds the original draft; the immutable submission may
    // contain child mail not in that draft. Neither is an admission receipt.
    scan(
        db,
        "
        SELECT s.queue_item_id,s.queue_revision,q.binding_digest,s.command_id,s.kind,
               s.target_turn_id,s.canonical_command,s.submitted_at_ms,s.anchor,b.binding_digest
        FROM submissions s JOIN identity_bindings q
          ON q.identity=s.queue_item_id AND q.owner_session_id=s.session_id
         AND q.kind='queue' AND q.terminal_disposition='deleted' AND q.terminal_outcome IS NULL
        JOIN identity_bindings b ON b.identity='plak:submission:'||s.command_id
         AND b.owner_session_id=s.session_id AND b.kind='command'
         AND b.terminal_disposition IS NULL AND b.terminal_outcome IS NULL
        WHERE s.session_id=?1
    ",
        [root],
        "queue submission",
        |row| {
            let draft = columns(row, [0])?;
            let Some(&expected) = wanted.get(&draft) else {
                return Ok(());
            };
            let rev = nonnegative(row, 1)?;
            let tombstone: String = get(row, 2)?;
            if texts([rev.to_string().as_str(), tombstone.as_str()]) != *expected {
                return Ok(());
            }
            let kind: String = get(row, 4)?;
            let submission = Submission {
                command_id: get(row, 3)?,
                kind: SubmissionKind::parse(&kind)
                    .ok_or_else(|| unreadable("queue submission kind"))?,
                queue_item_id: get(row, 0)?,
                queue_item_revision: rev,
                target_turn_id: get(row, 5)?,
                canonical_command: get(row, 6)?,
                submitted_at_ms: nonnegative(row, 7)?,
            };
            let bound: String = get(row, 9)?;
            check(
                bound == submission.binding_digest(root),
                "preservation found a queue submission that contradicts its permanent command binding",
            )?;
            budget.take()?;
            admissions.push(Admission {
                locator,
                anchor: nonnegative(row, 8)?,
                command: texts([submission.command_id.as_str()]),
                kind: submission.kind,
                target: submission
                    .target_turn_id
                    .as_deref()
                    .map(|turn| texts([turn])),
                draft,
            });
            Ok(())
        },
    )
}
