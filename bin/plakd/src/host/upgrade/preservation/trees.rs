use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

mod retained;

use crate::carry::upgrade::timing::REVISION_BACKUP as REVISION_BACKUP_TIME;
use plakous_recursive_subagents_store::admission::Status;
use plakous_recursive_subagents_store::database::schema;
use rusqlite::{Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use super::{Evidence, check};
use crate::CliResult;
use crate::home::paths::PlakPaths;
use crate::shipping_artifact as shipping;

use super::facts::{
    Budget, Fact, Key, Map, Table, compare_tables, get, insert, nonnegative, scan, tables, texts,
};
use super::unreadable;

const TABLES: &[Table] = &[
    Table {
        live: &["host_generation", "state", "snapshot_json"],
        monotone: &["current_epoch", "revision"],
        ..Table::stable("agent_trees", &["tree_id"])
    },
    Table {
        live: &[
            "status",
            "attempt_id",
            "session_locator",
            "active_turn",
            "fold_order",
        ],
        once: &["line_id"],
        ..Table::stable("agent_nodes", &["tree_id", "agent_id"])
    },
    Table {
        live: &[
            "visible_ids",
            "message_peers",
            "managed_ids",
            "interrupt_ids",
            "close_ids",
        ],
        ..Table::stable("agent_grants", &["tree_id", "agent_id"])
    },
    Table {
        once: &[
            "terminal_code",
            "terminal_response",
            "fold_anchor",
            "fold_order",
        ],
        ..Table::stable("agent_operations", &["tree_id", "operation_id"])
    },
    Table {
        live: &["durable"],
        ..Table::stable("agent_messages", &["tree_id", "message_id"])
    },
    Table {
        live: &[
            "status",
            "turn_id",
            "submit_harness_nudge",
            "blocked_kind",
            "blocked_account",
            "blocked_resets_at",
            "failure_kind",
            "failure_message",
            "elapsed_ms",
        ],
        monotone: &["turns", "tool_calls", "input_tokens", "output_tokens"],
        once: &["candidate_id", "ended_at"],
        prefix: &["applied_terminal_read_seqs"],
        ..Table::stable("agent_attempts", &["tree_id", "attempt_id"])
    },
    Table {
        live: &["adopted_evidence"],
        once: &[
            "adopted_by",
            "merged_at",
            "target_before",
            "adopted_via",
            "reverted_at",
            "reverted_by",
        ],
        ..Table::stable("agent_candidates", &["tree_id", "candidate_id"])
    },
    Table {
        live: &["status", "adopter_agent_id"],
        monotone: &["generation"],
        ..Table::stable("agent_workspaces", &["tree_id", "workspace_id"])
    },
    Table {
        live: &["fact"],
        monotone: &["withdrawn"],
        once: &["turn_id", "accepted_seq"],
        ..Table::stable("agent_commands", &["tree_id", "command_id"])
    },
    Table::stable("agent_move_seeds", &["tree_id"]),
    Table {
        monotone: &["applied_seq"],
        ..Table::stable("agent_feed_cursors", &["tree_id", "runtime_session_id"])
    },
    Table::stable("agent_tree_epochs", &["tree_id", "epoch_no"]),
    Table {
        prefix: &["commits"],
        ..Table::stable("agent_tree_runner_ancestors", &["tree_id", "runner_id"])
    },
];

const RETIRED: Table = Table::stable("agent_retired_children", &["tree_id", "agent_id"]);
const ARCHIVES: Table = Table::stable(
    "agent_child_archive_receipts",
    &[
        "tree_id",
        "agent_id",
        "attempt_id",
        "head",
        "runtime_revision",
    ],
);
const MAX_REVISION_BACKUP_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct SchemaIdentity {
    version: i64,
    instance: Key,
    pin: Key,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Stamp {
    file: Key,
    version: Key,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Trees {
    schema: SchemaIdentity,
    backups: BTreeMap<i64, Stamp>,
    fences: Map<u64>,
    tables: Map<Map<Fact>>,
    retained: Option<retained::Retained>,
}

pub(super) fn capture(
    paths: &PlakPaths,
    evidence: Evidence,
    baseline: bool,
    budget: &mut Budget,
) -> CliResult<Trees> {
    let path = paths.subagents_database();
    let registry = schema();
    registry
        .inspect_existing(&path, budget.deadline(), |db, status| {
            super::facts::configure_reader(db)?;
            let schema = identity(db, status)?;
            let held = evidence == Evidence::Held;
            let mut facts = tables(db, if held { TABLES } else { &TABLES[..1] }, budget)?;
            if held && schema.version >= 12 {
                facts.append(&mut tables(db, &[RETIRED], budget)?);
            }
            if held && schema.version >= 13 {
                facts.append(&mut tables(db, &[ARCHIVES], budget)?);
            }
            let mut fences = Map::new();
            scan(
                db,
                "SELECT tree_id,fence FROM agent_tree_custody",
                [],
                "tree fence",
                |row| {
                    budget.take()?;
                    let tree: String = get(row, 0)?;
                    let fence = nonnegative(row, 1)?;
                    check(fence > 0, "invalid stored tree fence")?;
                    insert(&mut fences, texts([tree.as_str()]), fence)
                },
            )?;
            let mut backups = BTreeMap::new();
            for version in registry.generation..=schema.version {
                if let Some(stamp) = stamp(&backup_path(paths, version))? {
                    backups.insert(version, stamp);
                }
            }
            let retained = if held {
                None
            } else {
                Some(retained::Retained::capture(
                    db,
                    schema.version,
                    baseline,
                    budget,
                )?)
            };
            Ok(Trees {
                schema,
                backups,
                fences,
                tables: facts,
                retained,
            })
        })
        .map_err(|_| unreadable("subagent store"))?
}

fn identity(db: &Connection, status: Status) -> CliResult<SchemaIdentity> {
    let (instance, pin) = db
        .query_row(
            "SELECT instance_id,fingerprint FROM subagents_schema WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| unreadable("tree schema identity"))?;
    let version = match status {
        Status::Pending(version) => version,
        Status::Current => schema().version(),
        Status::Fresh => return Err(unreadable("tree schema identity")),
    };
    Ok(SchemaIdentity {
        version,
        instance: super::identity::instance_key(&instance),
        pin: texts([pin.as_str()]),
    })
}

fn backup_path(paths: &PlakPaths, source_revision: i64) -> PathBuf {
    paths
        .subagents_database()
        .with_file_name(format!("subagents.pre-revision-{source_revision}.sqlite3"))
}

fn stamp(path: &Path) -> CliResult<Option<Stamp>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(stamp_of(&meta))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(unreadable("stopped-tree backup")),
    }
}

fn stamp_of(meta: &fs::Metadata) -> Stamp {
    Stamp {
        file: texts([
            meta.dev().to_string().as_str(),
            meta.ino().to_string().as_str(),
        ]),
        version: texts([
            meta.mtime().to_string().as_str(),
            meta.mtime_nsec().to_string().as_str(),
            meta.len().to_string().as_str(),
            meta.ctime().to_string().as_str(),
            meta.ctime_nsec().to_string().as_str(),
        ]),
    }
}

impl Trees {
    pub(super) fn compare(
        &self,
        after: &Self,
        paths: &PlakPaths,
        evidence: Evidence,
        deleted: impl Fn(&str) -> bool,
    ) -> CliResult<Option<String>> {
        let backup = if self.schema == after.schema {
            None
        } else {
            Some(self.verify_revision(after, paths)?)
        };
        let recorded_delete = |id: &str| evidence == Evidence::BaselinePredatesDrain && deleted(id);
        if evidence == Evidence::Held {
            compare_tables(&self.tables, &after.tables, |_, _| false)?;
        } else {
            let prior = &self.tables["agent_trees"];
            let current = &after.tables["agent_trees"];
            check(
                prior.iter().all(|(id, before)| {
                    current.get(id).map_or_else(
                        || recorded_delete(id),
                        |after| before.revisions_preserved_by(after),
                    )
                }),
                "preservation lost a tree without a recorded delete or regressed its revision",
            )?;
            let prior = self
                .retained
                .as_ref()
                .ok_or_else(|| unreadable("tree identities"))?;
            let current = after
                .retained
                .as_ref()
                .ok_or_else(|| unreadable("tree identities"))?;
            prior.compare(current, &|id| {
                !after.tables["agent_trees"].contains_key(id) && recorded_delete(id)
            })?;
        }
        check(
            self.fences.iter().all(|(tree, old)| {
                after
                    .fences
                    .get(tree)
                    .map_or_else(|| recorded_delete(tree), |new| new >= old)
            }),
            "preservation lost or regressed a tree fence",
        )?;
        Ok(backup)
    }

    fn verify_revision(&self, after: &Self, paths: &PlakPaths) -> CliResult<String> {
        let deadline = Instant::now() + REVISION_BACKUP_TIME;
        // Both fingerprints were already checked against the native registry
        // on their capture transactions.
        check(
            self.schema.version < after.schema.version
                && self.schema.instance == after.schema.instance,
            "preservation changed the tree identity or used an unrecognized schema revision",
        )?;
        let path = backup_path(paths, self.schema.version);
        let file = shipping::open(&path).map_err(|_| unreadable("stopped-tree backup"))?;
        let metadata = file.metadata().map_err(shipping::io)?;
        let observed = stamp_of(&metadata);
        check(
            Some(&observed) == after.backups.get(&self.schema.version)
                && self
                    .backups
                    .get(&self.schema.version)
                    .is_none_or(|old| old.file != observed.file),
            "tree revision did not publish a new stopped-store backup",
        )?;
        check(
            metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o777 == 0o600
                && metadata.len() <= MAX_REVISION_BACKUP_BYTES,
            "tree revision backup must be an owned private regular file",
        )?;
        for suffix in ["sqlite3-wal", "sqlite3-journal"] {
            check(
                path.with_extension(suffix)
                    .symlink_metadata()
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
                "tree revision backup is not a stopped-store copy",
            )?;
        }
        let intact = schema()
            .inspect_existing(&path, deadline, |db, status| {
                let backup = identity(db, status)?;
                let mut query = db
                    .prepare("PRAGMA quick_check(1)")
                    .map_err(|_| unreadable("stopped-tree backup"))?;
                let result: Option<String> = query
                    .query_row([], |row| row.get(0))
                    .optional()
                    .map_err(|_| unreadable("stopped-tree backup"))?;
                Ok::<_, crate::CliFailure>(backup == self.schema && result.as_deref() == Some("ok"))
            })
            .map_err(|_| unreadable("stopped-tree backup"))??;
        check(
            intact,
            "tree revision backup does not contain the intact source store identity",
        )?;
        let (length, hash) = shipping::hash_reader(
            BoundedRead { file, deadline },
            metadata.len(),
            &mut io::sink(),
        )?;
        check(
            length == metadata.len() && stamp(&path)? == Some(observed),
            "tree revision backup changed during verification",
        )?;
        Ok(hash)
    }
}

struct BoundedRead {
    file: fs::File,
    deadline: Instant,
}

impl io::Read for BoundedRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "stopped-tree backup inspection exceeded its bound",
            ));
        }
        self.file.read(buffer)
    }
}
