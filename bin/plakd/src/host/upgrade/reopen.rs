//! Verify while live, then recheck changed or unproven heads at the step hold.

use std::{collections::BTreeMap, path::Path, thread};

use super::{PROBE_BOUND, Request, process};
use crate::carry::{ChildReopenRefusal, REOPEN_HEAD_ADVANCED, ReopenProbe};
use crate::home::paths::PlakPaths;
use crate::store::ProductStore;
use crate::{CliFailure, CliResult};
use plakous::sessions::{AgentSessions, SessionLocator};
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeQualificationStamp};

// Bound the number of cold checkpoint loads as well as processes. Paired
// runners are probed serially so this doesn't fan out SSH connections.
const MAX_LOCAL_PROBES: usize = 8;

#[derive(Clone, Copy, Eq, PartialEq)]
struct Head {
    locator: SessionLocator,
    revision: u64,
    product_revision: u64,
    tree: Option<TreeQualificationStamp>,
}

pub(super) struct Root {
    id: String,
    local: bool,
    head: Option<Head>,
}

pub(super) fn snapshot(paths: &PlakPaths) -> CliResult<Vec<Root>> {
    let records = ProductStore::inspect_upgrade_roots(paths)
        .map_err(CliFailure::new)?
        .into_iter()
        .filter_map(|record| record.plakous_locator.map(|locator| (record, locator)))
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let locators = records
        .iter()
        .map(|(_, locator)| *locator)
        .collect::<Vec<_>>();
    let revisions =
        AgentSessions::inspect_branch_revisions_sqlite(paths.runtime_database(), &locators)
            .map_err(CliFailure::from)?;
    let tree_ids = records
        .iter()
        .map(|(record, _)| record.node.tree_id.as_str())
        .collect::<Vec<_>>();
    let trees =
        SqliteTreeStore::inspect_qualification_stamps(&paths.subagents_database(), &tree_ids)
            .map_err(|error| CliFailure::new(error.message()))?;
    Ok(records
        .into_iter()
        .zip(revisions)
        .zip(trees)
        .map(|(((record, locator), revision), tree)| Root {
            id: record.id,
            local: record
                .runner
                .as_ref()
                .is_none_or(|runner| runner.runner_id == "local"),
            head: revision.map(|revision| Head {
                locator,
                revision,
                product_revision: record.store_revision,
                tree,
            }),
        })
        .collect())
}

pub(super) struct Preflight {
    passed: BTreeMap<String, Head>,
    pub(super) rehearsed: bool,
}

impl Preflight {
    pub(super) fn changed(&self, paths: &PlakPaths) -> CliResult<Vec<Root>> {
        Ok(snapshot(paths)?
            .into_iter()
            .filter(|root| root.head.is_none() || root.head.as_ref() != self.passed.get(&root.id))
            .collect())
    }
}

pub(super) fn preflight(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    before_rehearsal: Vec<Root>,
) -> CliResult<Preflight> {
    // Capture before starting probes: even if a probe follows the latest head,
    // an advance during its execution can't make the held check optional.
    let roots = if scratch.is_some() {
        before_rehearsal
    } else {
        snapshot(paths)?
    };
    let verdicts = probe_roots(paths, request, scratch, &roots, true)?;
    let passed = roots
        .into_iter()
        .zip(verdicts)
        .filter_map(|(root, proved)| {
            proved
                .then_some(root.head)
                .flatten()
                .map(|head| (root.id, head))
        })
        .collect();
    Ok(Preflight {
        passed,
        rehearsed: scratch.is_some(),
    })
}

pub(super) fn verify_held(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    roots: &[Root],
) -> CliResult<()> {
    probe_roots(paths, request, scratch, roots, false).map(drop)
}

fn probe_roots(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    roots: &[Root],
    allow_advance: bool,
) -> CliResult<Vec<bool>> {
    let local_width = thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_LOCAL_PROBES);
    let probe = |root: &Root| exact_reopen(paths, request, scratch, &root.id, allow_advance);
    let mut verdicts = Vec::with_capacity(roots.len());
    let mut failures: Option<Box<ChildReopenRefusal>> = None;
    let mut collect = |result: CliResult<Proof>| -> CliResult<()> {
        match result? {
            Proof::Root(passed) => verdicts.push(passed),
            Proof::ChildRefused(children) => {
                if let Some(failures) = &mut failures {
                    failures.merge(*children);
                } else {
                    failures = Some(children);
                }
            }
        }
        Ok(())
    };
    for placement in roots.chunk_by(|a, b| a.local == b.local) {
        let width = if placement[0].local { local_width } else { 1 };
        for wave in placement.chunks(width) {
            if let [record] = wave {
                collect(probe(record))?;
            } else {
                thread::scope(|scope| {
                    let pending = wave
                        .iter()
                        .map(|record| scope.spawn(|| probe(record)))
                        .collect::<Vec<_>>();
                    // Join the already dispatched wave before returning a refusal.
                    let joined = pending
                        .into_iter()
                        .map(|task| {
                            task.join().unwrap_or_else(|_| {
                                Err(CliFailure::new("host upgrade exact-reopen worker failed"))
                            })
                        })
                        .collect::<Vec<_>>();
                    for result in joined {
                        collect(result)?;
                    }
                    Ok::<_, CliFailure>(())
                })?;
            }
        }
    }
    if let Some(children) = failures {
        Err(CliFailure::new(children.to_string()))
    } else {
        Ok(verdicts)
    }
}

enum Proof {
    Root(bool),
    ChildRefused(Box<ChildReopenRefusal>),
}

fn exact_reopen(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    session: &str,
    allow_advance: bool,
) -> CliResult<Proof> {
    let mut command =
        process::command(&request.destination, paths.home(), &request.destination_key);
    command
        .args(["version", "--host-upgrade-reopen", "--reopen-probe"])
        .arg(session);
    if let Some(scratch) = scratch {
        command.arg("--store-probe").arg(scratch);
    }
    let verdict: ReopenProbe = serde_json::from_slice(&process::run(command, PROBE_BOUND)?.stdout)
        .map_err(|_| {
            CliFailure::new("same-key upgrade destination did not answer its exact-reopen probe")
        })?;
    match verdict {
        ReopenProbe::Opens => Ok(Proof::Root(true)),
        ReopenProbe::Refused {
            children: Some(children),
            ..
        } => Ok(Proof::ChildRefused(children)),
        ReopenProbe::Unknown { reason }
            if reason == plakous_hosted::runtime::REOPEN_UNSETTLED_WORK =>
        {
            // Recheck under the holds; if still unfinished, the planned
            // handoff recovery law owns it rather than a qualification probe.
            Ok(Proof::Root(false))
        }
        ReopenProbe::Unknown { reason } if allow_advance && reason == REOPEN_HEAD_ADVANCED => {
            Ok(Proof::Root(false))
        }
        verdict => Err(CliFailure::new(format!(
            "host_upgrade_reopen_refused: {session}: {verdict:?}; same-key upgrade uses planned handoff, not carry",
        ))),
    }
}
