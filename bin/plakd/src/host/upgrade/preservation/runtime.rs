mod graph;

use std::collections::{BTreeMap, BTreeSet};

use plakous::model::TurnOrigin;
use plakous::sessions::feed::{FeedCommandKind, FeedFact};
use plakous::sessions::rehearsal::PreservationReader;
use plakous_hosted::sessions::projector::SubmissionKind;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{Evidence, check, fail};
use crate::CliResult;
use crate::home::paths::PlakPaths;

use self::graph::{Graph, object_key};
use super::facts::{
    Budget, Fact, Key, Map, Table, columns, compare_tables, get, insert, nonnegative, revision,
    scan, subset, tables, texts,
};
use super::{product::Admission, unreadable};

const BOOKKEEPING: &[Table] = &[
    Table::stable("create_receipts", &["session_id"]),
    Table::stable("commit_receipts", &["checkpoint_id"]),
    Table::stable("fork_receipts", &["workflow_id"]),
    Table::stable("archive_receipts", &["session_id"]),
    Table::stable("delete_tombstones", &["session_id"]),
    Table::stable("pruned_checkpoints", &["checkpoint_id"]),
    Table::stable("line_job_terminals", &["line_id", "generation", "job"]),
    Table::stable("runtime_moves", &["operation"]),
    Table::stable("runtime_move_lines", &["line_id"]),
    Table::stable("runtime_move_members", &["operation", "line_id"]),
    Table::stable("runtime_move_sessions", &["operation", "session_id"]),
    Table::stable(
        "runtime_move_heads",
        &["operation", "session_id", "branch_id"],
    ),
    Table {
        monotone: &["execution_open"],
        ..Table::stable("imported_sessions", &["session_id"])
    },
    Table::stable("imported_checkpoints", &["checkpoint_id"]),
    Table::stable("imported_branches", &["session_id", "branch_id"]),
];

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Checkpoint {
    session: Key,
    branch: Key,
    parent: Option<Key>,
    revision: u64,
    root: Key,
    content: Key,
}

#[derive(Deserialize, Serialize)]
struct Branch {
    session: Key,
    head: Key,
    revision: u64,
    identity: Key,
}

#[derive(Deserialize, Serialize)]
struct Session {
    identity: Key,
    revision: u64,
    archived: bool,
    retained_from: u64,
    tail: u64,
    feed: BTreeMap<u64, Key>,
}

#[derive(Deserialize, Serialize)]
struct Line {
    session: Key,
    head: Key,
    generation: u64,
    origin: Key,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Deleted {
    identity: Key,
    revision: u64,
    seal: Key,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Horizon {
    revision: u64,
    checkpoint: Key,
    count: u64,
    seal: Key,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Runtime {
    instance: Key,
    sessions: Map<Session>,
    lines: Map<Line>,
    deleted: Map<Deleted>,
    checkpoints: Map<Checkpoint>,
    branches: Map<Branch>,
    graph: Graph,
    bookkeeping: Map<Map<Fact>>,
    horizons: Map<Horizon>,
    pruned: Map<Key>,
}

pub(super) fn capture(
    paths: &PlakPaths,
    candidates: Vec<Admission>,
    evidence: Evidence,
    budget: &mut Budget,
) -> CliResult<(Option<Runtime>, BTreeSet<Key>)> {
    match PreservationReader::inspect(&paths.runtime_database(), budget.deadline(), |reader| {
        super::facts::configure_reader(reader.connection())?;
        let accepted = admissions(reader, candidates)?;
        let runtime = capture_runtime(reader, evidence, budget)?;
        Ok((Some(runtime), accepted))
    }) {
        Ok(result) => result,
        Err(_)
            if paths
                .runtime_database()
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok((None, BTreeSet::new()))
        }
        Err(_) => Err(unreadable("runtime store")),
    }
}

fn capture_runtime(
    reader: &PreservationReader<'_>,
    evidence: Evidence,
    budget: &mut Budget,
) -> CliResult<Runtime> {
    let db = reader.connection();
    let instance = db
        .query_row(
            "SELECT lease_domain FROM store_meta WHERE id=1",
            [],
            |row| row.get::<_, [u8; 48]>(0),
        )
        .map_err(|_| unreadable("runtime store identity"))?;
    let mut sessions = Map::new();
    scan(
        db,
        "SELECT session_id,initial_branch,created_s,created_ns,durability,archived,
                revision,feed_retained_from,feed_tail FROM sessions",
        [],
        "runtime session",
        |row| {
            budget.take()?;
            let session = Session {
                identity: columns(row, 1..5)?,
                revision: revision(row, 6)?,
                archived: get(row, 5)?,
                retained_from: nonnegative(row, 7)?,
                tail: nonnegative(row, 8)?,
                feed: BTreeMap::new(),
            };
            check(
                (1..=session.tail.saturating_add(1)).contains(&session.retained_from),
                "preservation found an invalid runtime feed window",
            )?;
            insert(&mut sessions, columns(row, [0])?, session)
        },
    )?;
    scan(
        db,
        "SELECT * FROM session_feed ORDER BY session_id,seq",
        [],
        "runtime feed",
        |row| {
            budget.take()?;
            let session = sessions
                .get_mut(&columns(row, [0])?)
                .ok_or_else(|| unreadable("runtime feed owner"))?;
            let seq = nonnegative(row, 1)?;
            // The retained range must be gap-free; the hashes compare the overlap.
            check(
                seq == session.retained_from + session.feed.len() as u64 && seq <= session.tail,
                "preservation found a gap in the retained runtime feed",
            )?;
            session.feed.insert(seq, columns(row, 2..7)?);
            Ok(())
        },
    )?;
    check(
        sessions
            .values()
            .all(|session| session.retained_from + session.feed.len() as u64 == session.tail + 1),
        "preservation found a truncated runtime feed",
    )?;
    let mut deleted = Map::new();
    scan(
        db,
        "SELECT * FROM delete_tombstones",
        [],
        "runtime deletion receipts",
        |row| {
            budget.take()?;
            let rev = revision(row, 5)?;
            reader
                .validate_delete(
                    get::<String>(row, 0)?
                        .parse()
                        .map_err(|_| unreadable("runtime deletion identity"))?,
                    rev,
                )
                .map_err(|_| unreadable("sealed runtime deletion receipt"))?;
            insert(
                &mut deleted,
                columns(row, [0])?,
                Deleted {
                    identity: columns(row, 1..5)?,
                    revision: rev,
                    seal: columns(row, [7])?,
                },
            )
        },
    )?;
    let mut lines = Map::new();
    scan(
        db,
        "SELECT line_id,head_session,head_branch,generation,forked_from FROM lines",
        [],
        "runtime lines",
        |row| {
            budget.take()?;
            insert(
                &mut lines,
                columns(row, [0])?,
                Line {
                    session: columns(row, [1])?,
                    head: columns(row, [1, 2])?,
                    generation: revision(row, 3)?,
                    origin: columns(row, [4])?,
                },
            )
        },
    )?;
    let mut checkpoints = Map::new();
    scan(
        db,
        "
        SELECT checkpoint_id,session_id,branch_id,parent_id,revision,root_kind,root_codec,
               root_digest,integrity_version,integrity,commit_version,commit_digest,
               base_kind,base_codec,base_digest,fork_session,fork_branch,fork_checkpoint,
               fork_revision,fence,kind,durability,semantic_key,validation_seal
        FROM checkpoints
    ",
        [],
        "checkpoint",
        |row| {
            budget.take()?;
            let parent = get::<Option<String>>(row, 3)?.map(|id| texts([id.as_str()]));
            insert(
                &mut checkpoints,
                columns(row, [0])?,
                Checkpoint {
                    session: columns(row, [1])?,
                    branch: columns(row, [1, 2])?,
                    parent,
                    revision: revision(row, 4)?,
                    root: object_key(row, 5)?,
                    content: columns(row, 5..24)?,
                },
            )
        },
    )?;
    let branches = read_branches(db, &checkpoints, budget)?;
    let graph = Graph::capture(db, budget)?;
    for branch in branches.values() {
        graph.validate_history(&checkpoints[&branch.head].root)?;
    }
    let (horizons, pruned) = read_retention(reader, &checkpoints, budget)?;
    let bookkeeping = if evidence == Evidence::Held {
        tables(db, BOOKKEEPING, budget)?
    } else {
        Map::new()
    };
    Ok(Runtime {
        instance: crate::shipping_artifact::digest(&instance),
        sessions,
        lines,
        deleted,
        checkpoints,
        branches,
        graph,
        bookkeeping,
        horizons,
        pruned,
    })
}

fn read_branches(
    db: &Connection,
    checkpoints: &Map<Checkpoint>,
    budget: &mut Budget,
) -> CliResult<Map<Branch>> {
    let mut branches = Map::new();
    scan(
        db,
        "
        SELECT session_id,branch_id,checkpoint_id,revision,fork_session,fork_branch,
               fork_checkpoint,fork_revision,line_id,line_generation FROM branches
    ",
        [],
        "runtime branch",
        |row| {
            budget.take()?;
            let key = columns(row, [0, 1])?;
            let head = columns(row, [2])?;
            let revision = revision(row, 3)?;
            check(
                checkpoints
                    .get(&head)
                    .is_some_and(|row| row.branch == key && row.revision == revision),
                "preservation found a branch head that disagrees with its checkpoint",
            )?;
            insert(
                &mut branches,
                key,
                Branch {
                    session: columns(row, [0])?,
                    head,
                    revision,
                    identity: columns(row, 4..10)?,
                },
            )
        },
    )?;
    Ok(branches)
}

fn read_retention(
    reader: &PreservationReader<'_>,
    checkpoints: &Map<Checkpoint>,
    budget: &mut Budget,
) -> CliResult<(Map<Horizon>, Map<Key>)> {
    let db = reader.connection();
    let mut horizons = Map::new();
    scan(
        db,
        "
        SELECT session_id,branch_id,horizon_revision,horizon_checkpoint,pruned_checkpoints,integrity
        FROM branch_horizons
    ",
        [],
        "branch horizon",
        |row| {
            budget.take()?;
            let key = columns(row, [0, 1])?;
            let corrupt = || unreadable("sealed branch horizon");
            reader
                .validate_horizon(
                    get::<String>(row, 0)?.parse().map_err(|_| corrupt())?,
                    get::<String>(row, 1)?.parse().map_err(|_| corrupt())?,
                )
                .map_err(|_| corrupt())?;
            let horizon = Horizon {
                revision: revision(row, 2)?,
                checkpoint: columns(row, [3])?,
                count: nonnegative(row, 4)?,
                seal: columns(row, [5])?,
            };
            check(
                horizon.count > 0
                    && checkpoints
                        .get(&horizon.checkpoint)
                        .is_some_and(|row| row.branch == key && row.revision == horizon.revision),
                "preservation found a horizon that does not name its retained checkpoint",
            )?;
            insert(&mut horizons, key, horizon)
        },
    )?;
    let mut pruned = Map::new();
    scan(
        db,
        "SELECT checkpoint_id,session_id,branch_id FROM pruned_checkpoints",
        [],
        "pruned checkpoint",
        |row| {
            budget.take()?;
            let key = columns(row, [0])?;
            check(
                !checkpoints.contains_key(&key),
                "preservation found a checkpoint both pruned and retained",
            )?;
            insert(&mut pruned, key, columns(row, [1, 2])?)
        },
    )?;
    Ok((horizons, pruned))
}

fn admissions(
    reader: &PreservationReader<'_>,
    candidates: Vec<Admission>,
) -> CliResult<BTreeSet<Key>> {
    let mut groups = BTreeMap::new();
    for candidate in candidates {
        let selector = (
            candidate.locator.session_id(),
            candidate.locator.branch_id(),
        );
        let key = candidate.command.clone();
        insert(
            groups.entry(selector).or_insert_with(Map::new),
            key,
            candidate,
        )?;
    }
    let mut accepted = BTreeSet::new();
    for ((session, branch), commands) in groups {
        let Some(anchor) = commands.values().map(|candidate| candidate.anchor).min() else {
            continue;
        };
        let wanted = commands
            .values()
            .map(|candidate| &candidate.draft)
            .collect::<BTreeSet<_>>();
        let mut matched = BTreeSet::new();
        let mut disagrees = false;
        reader
            .visit_advances(session, branch, anchor, |record| {
                for fact in record.facts() {
                    let FeedFact::CommandAccepted {
                        command_id,
                        kind,
                        turn_id,
                        ..
                    } = fact
                    else {
                        continue;
                    };
                    let Some(candidate) = commands.get(&texts([command_id.as_str()])) else {
                        continue;
                    };
                    if record.header().seq.get() <= candidate.anchor {
                        continue;
                    }
                    let valid = turn_id.is_some_and(|turn| match (candidate.kind, kind) {
                        (
                            SubmissionKind::RootStart,
                            FeedCommandKind::Root {
                                origin: TurnOrigin::UserPrompt,
                            },
                        )
                        | (
                            SubmissionKind::HostWake,
                            FeedCommandKind::Root {
                                origin: TurnOrigin::HostWake,
                            },
                        ) => candidate.target.is_none(),
                        (SubmissionKind::SendNowSteer, FeedCommandKind::Steer) => {
                            candidate.target.as_ref() == Some(&texts([turn.to_string().as_str()]))
                        }
                        _ => false,
                    });
                    if !valid {
                        disagrees = true;
                        return false;
                    }
                    matched.insert(&candidate.draft);
                }
                matched != wanted
            })
            .map_err(|_| unreadable("sealed runtime feed"))?;
        check(
            !disagrees,
            "preservation found an admission that disagrees with its queue submission",
        )?;
        accepted.extend(matched.into_iter().cloned());
    }
    Ok(accepted)
}

impl Runtime {
    pub(super) fn deleted(&self) -> &Map<Deleted> {
        &self.deleted
    }

    pub(super) fn compare(&self, after: &Self, evidence: Evidence) -> CliResult<()> {
        check(
            self.instance == after.instance,
            "preservation changed the runtime store identity",
        )?;
        subset(&self.deleted, &after.deleted, "runtime tombstones")?;
        let deleted = |id: &str| {
            evidence == Evidence::BaselinePredatesDrain
                && self.sessions.get(id).is_some_and(|before| {
                    after.deleted.get(id).is_some_and(|record| {
                        record.identity == before.identity && record.revision >= before.revision
                    })
                })
        };
        for (id, before) in &self.sessions {
            let Some(current) = after.sessions.get(id) else {
                check(
                    deleted(id),
                    "preservation lost a runtime session without a sealed delete",
                )?;
                continue;
            };
            check(
                before.identity == current.identity
                    && before.revision <= current.revision
                    && if evidence == Evidence::Held {
                        before.archived == current.archived
                            && before.retained_from == current.retained_from
                    } else {
                        (!before.archived || current.archived)
                            && before.retained_from <= current.retained_from
                    },
                "preservation changed a runtime session or regressed retention",
            )?;
            check(
                before.tail <= current.tail
                    && before
                        .feed
                        .range(current.retained_from..)
                        .all(|(seq, hash)| current.feed.get(seq) == Some(hash)),
                "preservation lost or changed an admitted runtime feed prefix",
            )?;
        }
        for (id, before) in &self.lines {
            let Some(current) = after.lines.get(id) else {
                check(deleted(&before.session), "preservation lost a runtime line")?;
                continue;
            };
            check(
                current.origin == before.origin
                    && (current.generation == before.generation && current.head == before.head
                        || evidence == Evidence::BaselinePredatesDrain
                            && current.generation > before.generation),
                "preservation changed or regressed a runtime line",
            )?;
        }
        for (branch, before) in &self.horizons {
            if self
                .branches
                .get(branch)
                .is_some_and(|branch| deleted(&branch.session))
            {
                continue;
            }
            check(
                after.horizons.get(branch).is_some_and(|new| {
                    new == before || (new.revision > before.revision && new.count > before.count)
                }),
                "preservation found a removed or regressed branch prune horizon",
            )?;
        }
        let advanced = |branch: &str| {
            after
                .horizons
                .get(branch)
                .filter(|new| self.horizons.get(branch) != Some(*new))
                .map(|new| new.revision)
        };
        let mut pruned = BTreeSet::new();
        for (key, before) in &self.checkpoints {
            if deleted(&before.session) {
                pruned.insert(key);
                continue;
            }
            if let Some(current) = after.checkpoints.get(key) {
                check(
                    current == before,
                    "preservation changed an immutable checkpoint",
                )?;
            } else {
                check(
                    after.pruned.get(key) == Some(&before.branch)
                        && advanced(&before.branch)
                            .is_some_and(|horizon| before.revision < horizon),
                    "preservation lost a checkpoint without a matching advanced prune horizon",
                )?;
                pruned.insert(key);
            }
        }
        let retained = self
            .checkpoints
            .iter()
            .filter(|(key, _)| !pruned.contains(key))
            .map(|(_, row)| &row.root);
        self.graph.compare(
            &after.graph,
            if pruned.is_empty() {
                None
            } else {
                Some(retained)
            },
        )?;
        compare_tables(&self.bookkeeping, &after.bookkeeping, |table, key| {
            table == "commit_receipts"
                && self.checkpoints.get(key).is_some_and(|row| {
                    advanced(&row.branch).is_some_and(|horizon| row.revision <= horizon)
                })
        })?;
        for (key, before) in &self.branches {
            if deleted(&before.session) {
                continue;
            }
            let current = after
                .branches
                .get(key)
                .ok_or_else(|| fail("preservation lost a pre-existing branch"))?;
            check(
                current.identity == before.identity && current.revision >= before.revision,
                "preservation changed branch identity or regressed its revision",
            )?;
            self.compare_branch(after, key, before, current, pruned.contains(&before.head))?;
        }
        Ok(())
    }

    fn compare_branch(
        &self,
        after: &Self,
        key: &str,
        before: &Branch,
        current: &Branch,
        head_pruned: bool,
    ) -> CliResult<()> {
        if head_pruned {
            let old_root = &self.checkpoints[&before.head].root;
            let new_root = &after.checkpoints[&current.head].root;
            after
                .graph
                .compare_history(new_root, &self.graph, old_root)?;
        }
        let mut head = &current.head;
        let mut revision = current.revision;
        while head != &before.head {
            let Some(row) = after.checkpoints.get(head) else {
                // Pruning replaced the physical ancestry proof with the
                // ordered canonical history prefix above.
                return check(
                    head_pruned
                        && revision >= before.revision
                        && after
                            .horizons
                            .get(key)
                            .is_some_and(|horizon| revision < horizon.revision)
                        && after.pruned.get(head).is_some_and(|branch| branch == key),
                    "preservation found a divergent branch history",
                );
            };
            check(
                row.branch == key && row.revision == revision && revision > before.revision,
                "preservation found a divergent branch history",
            )?;
            head = row
                .parent
                .as_ref()
                .ok_or_else(|| fail("preservation found a divergent branch history"))?;
            revision -= 1;
        }
        check(
            revision == before.revision,
            "preservation found noncontiguous branch history",
        )
    }
}
