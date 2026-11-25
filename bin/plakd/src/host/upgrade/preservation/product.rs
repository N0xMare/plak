use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt as _;

use plakous::sessions::{AgentSessions, SessionLocator, SessionOperationId};
use plakous_hosted::sessions::projector::{Submission, SubmissionKind};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Evidence, check};
use crate::CliResult;
use crate::home::paths::PlakPaths;
use crate::store::{queue_tombstone_digest, revisions::databases};

use super::facts::{
    Budget, Key, Map, branch_key, columns, finish, get, insert, locator_branch, nonnegative, scan,
    texts,
};
use super::unreadable;

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Root {
    stable: Key,
    identity: Key,
    revision: u64,
    archived: bool,
    branch: Option<Key>,
    runtime: Option<Key>,
    owns_tree: bool,
}

#[derive(Deserialize, Serialize)]
struct Draft {
    id: Key,
    revision: u64,
    content: Key,
    witness: Option<Key>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Product {
    instance: Key,
    roots: Map<Root>,
    queues: Map<Vec<Draft>>,
    removed_drafts: BTreeSet<Key>,
    pub(super) deleted_trees: BTreeSet<Key>,
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
    pub(super) fn compare(
        &self,
        after: &Self,
        accepted: &BTreeSet<Key>,
        evidence: Evidence,
        runtime_deleted: impl Fn(&str) -> bool,
    ) -> CliResult<()> {
        check(
            self.instance == after.instance,
            "preservation changed the product store identity",
        )?;
        for (id, before) in &self.roots {
            let preserved = if let Some(current) = after.roots.get(id) {
                current.identity == before.identity
                    && current.revision >= before.revision
                    && (evidence == Evidence::BaselinePredatesDrain
                        || (current.stable == before.stable
                            && current.archived == before.archived
                            && current.branch == before.branch))
            } else {
                evidence == Evidence::BaselinePredatesDrain
                    && before.runtime.as_deref().is_some_and(&runtime_deleted)
            };
            check(preserved, "preservation lost or changed a product session")?;
        }
        for (root, before) in &self.queues {
            let current = after.queues.get(root).map_or(&[][..], Vec::as_slice);
            if evidence == Evidence::BaselinePredatesDrain {
                if !after.roots.contains_key(root)
                    && self
                        .roots
                        .get(root)
                        .and_then(|row| row.runtime.as_deref())
                        .is_some_and(&runtime_deleted)
                {
                    continue;
                }
                let current: std::collections::BTreeMap<_, _> =
                    current.iter().map(|draft| (&draft.id, draft)).collect();
                for draft in before {
                    check(
                        current.get(&draft.id).map_or_else(
                            || after.removed_drafts.contains(&draft.id),
                            |new| {
                                new.revision > draft.revision
                                    || (new.revision == draft.revision
                                        && new.content == draft.content)
                            },
                        ),
                        "preservation lost a queued draft without a recorded removal or regressed its revision",
                    )?;
                }
                continue;
            }
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

    pub(super) fn owned_tree_had_runtime_deleted(
        &self,
        tree: &str,
        deleted: impl Fn(&str) -> bool,
    ) -> bool {
        self.roots
            .get(tree)
            .is_some_and(|root| root.owns_tree && root.runtime.as_deref().is_some_and(deleted))
    }
}

fn ids(queue: &[Draft]) -> BTreeSet<&str> {
    queue.iter().map(|draft| draft.id.as_str()).collect()
}

pub(super) fn capture(
    paths: &PlakPaths,
    before: Option<&Product>,
    evidence: Evidence,
    budget: &mut Budget,
) -> CliResult<(Product, Vec<Admission>)> {
    let [(schema, path), _] = databases(paths);
    schema
        .inspect_existing(&path, budget.deadline(), |db, _| {
            capture_product(db, before, evidence, budget)
        })
        .map_err(|_| unreadable("product store"))?
        .and_then(|(mut product, admissions)| {
            if evidence == Evidence::BaselinePredatesDrain {
                product.deleted_trees = deleted_trees(paths, budget)?;
            }
            Ok((product, admissions))
        })
}

fn capture_product(
    db: &Connection,
    before: Option<&Product>,
    evidence: Evidence,
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
    let mut removed_drafts = BTreeSet::new();
    let mut admissions = Vec::new();
    scan(
        db,
        "
        SELECT id,archived,locator,node_tree_id,node_agent_id,account,model,effort,composition,
               workspace_root,workspace_identity,root_profile,runner_display,target_selection,
               frozen_tools,created_at,create_operation_id,source_session_id,source_operation_id,
               revision
        FROM sessions ORDER BY id
    ",
        [],
        "product session",
        |row| {
            budget.take()?;
            let id: String = get(row, 0)?;
            let key = texts([id.as_str()]);
            let locator = get::<Option<String>>(row, 2)?
                .as_deref()
                .map(locator_branch)
                .transpose()?;
            let root = Root {
                stable: if evidence == Evidence::Held {
                    columns(row, (5..15).chain([17, 18]))?
                } else {
                    Key::new()
                },
                identity: columns(row, [3, 4, 15, 16])?,
                revision: nonnegative(row, 19)?,
                archived: get(row, 1)?,
                branch: locator.map(branch_key),
                runtime: match locator {
                    Some(id) => Some(id),
                    None if evidence == Evidence::BaselinePredatesDrain => {
                        let operation: String = get(row, 16)?;
                        SessionOperationId::new(operation)
                            .ok()
                            .and_then(|op| AgentSessions::genesis_locator_for(&op).ok())
                    }
                    None => None,
                }
                .map(|id| texts([id.session_id().to_string().as_str()])),
                owns_tree: columns(row, [3])? == key && columns(row, [4])? == key,
            };
            let queue = queue(db, &id, budget)?;
            if let Some(prior) = before.and_then(|snapshot| snapshot.queues.get(&key)) {
                let surviving = ids(&queue);
                if evidence == Evidence::BaselinePredatesDrain {
                    let missing = prior
                        .iter()
                        .filter(|item| !surviving.contains(item.id.as_str()))
                        .map(|item| item.id.as_str())
                        .collect();
                    read_removals(db, &id, &missing, &mut removed_drafts, budget)?;
                } else if let Some(locator) = locator {
                    let wanted = prior
                        .iter()
                        .filter_map(|item| {
                            let witness = item.witness.as_ref()?;
                            (!surviving.contains(item.id.as_str()))
                                .then_some((item.id.clone(), witness))
                        })
                        .collect();
                    read_admissions(db, &id, locator, &wanted, &mut admissions, budget)?;
                }
            }
            if !queue.is_empty() {
                insert(&mut queues, key.clone(), queue)?;
            }
            insert(&mut roots, key, root)
        },
    )?;
    Ok((
        Product {
            instance: texts([instance.as_str()]),
            roots,
            queues,
            removed_drafts,
            deleted_trees: BTreeSet::new(),
        },
        admissions,
    ))
}

fn deleted_trees(paths: &PlakPaths, budget: &mut Budget) -> CliResult<BTreeSet<Key>> {
    let dir = paths.state_dir().join("deleted-artifacts");
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(_) => return Err(unreadable("deleted tree inventory")),
    };
    let mut deleted = BTreeSet::new();
    for entry in entries {
        budget.take()?;
        let entry = entry.map_err(|_| unreadable("deleted tree inventory"))?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
            && entry.metadata().is_ok_and(|meta| {
                meta.is_file()
                    && meta.mode() & 0o777 == 0o600
                    && meta.uid() == rustix::process::geteuid().as_raw()
            })
        {
            deleted.insert(texts([id]));
        }
    }
    Ok(deleted)
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
                revision,
                content: finish(content),
                witness,
            });
            Ok(())
        },
    )?;
    Ok(result)
}

fn read_removals(
    db: &Connection,
    root: &str,
    missing: &BTreeSet<&str>,
    removed: &mut BTreeSet<Key>,
    budget: &mut Budget,
) -> CliResult<()> {
    if missing.is_empty() {
        return Ok(());
    }
    // A live predecessor may edit before removing. The immutable owner and
    // terminal queue identity prove removal, not exact admission of this text.
    scan(
        db,
        "
        SELECT identity FROM identity_bindings
        WHERE owner_session_id=?1 AND kind='queue'
          AND terminal_disposition IN ('deleted','consumed')
    ",
        [root],
        "queue removal",
        |row| {
            budget.take()?;
            let id = columns(row, [0])?;
            if missing.contains(id.as_str()) {
                removed.insert(id);
            }
            Ok(())
        },
    )
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
