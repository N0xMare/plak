//! Verify while live, then recheck changed or unproven heads at the step hold.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

use super::{PROBE_BOUND, Request, process, progress};
use crate::carry::{
    ChildReopenRefusal, REOPEN_HEAD_ADVANCED, ReopenProbe, UpgradeReopenProbe,
    child_reopen::{
        ChildBaseline, ChildReopenInspection, ChildReopenQualification, LegacyChildBaseline,
    },
};
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
    source: SourceImage,
}

pub(super) struct SourceImage {
    pub(super) path: PathBuf,
    pub(super) verified: process::VerifiedImage,
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
    source: SourceImage,
    report: &progress::Report<'_>,
) -> CliResult<Preflight> {
    // Capture before starting probes: even if a probe follows the latest head,
    // an advance during its execution can't make the held check optional.
    let roots = if scratch.is_some() {
        before_rehearsal
    } else {
        snapshot(paths)?
    };
    let verdicts = probe_roots(paths, request, scratch, &roots, &source, true, report)?;
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
        source,
    })
}

pub(super) fn verify_held(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    roots: &[Root],
    preflight: &Preflight,
    report: &progress::Report<'_>,
) -> CliResult<()> {
    probe_roots(
        paths,
        request,
        scratch,
        roots,
        &preflight.source,
        false,
        report,
    )
    .map(drop)
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared qualification at the live and held boundaries"
)]
fn probe_roots(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    roots: &[Root],
    source: &SourceImage,
    allow_advance: bool,
    report: &progress::Report<'_>,
) -> CliResult<Vec<bool>> {
    let local_width = thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_LOCAL_PROBES);
    let probe =
        |root: &Root| exact_reopen(paths, request, scratch, &root.id, source, allow_advance);
    let mut verdicts = Vec::with_capacity(roots.len());
    let mut failures = ChildReopenRefusal::default();
    let mut collect = |root: &Root, result: CliResult<Proof>| -> CliResult<()> {
        let proof = result?;
        failures.merge(proof.children.regressions);
        report(progress::Event::ChildFaults(
            &root.id,
            proof.children.pre_existing,
        ))?;
        if !allow_advance && proof.children.unproven {
            return Err(CliFailure::new(
                "host_upgrade_child_reopen_unproven: an exact child head could not be compared under the holds",
            ));
        }
        verdicts.push(proof.root? && !proof.children.unproven);
        Ok(())
    };
    for placement in roots.chunk_by(|a, b| a.local == b.local) {
        let width = if placement[0].local { local_width } else { 1 };
        for wave in placement.chunks(width) {
            if let [record] = wave {
                collect(record, probe(record))?;
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
                    for (record, result) in wave.iter().zip(joined) {
                        collect(record, result)?;
                    }
                    Ok::<_, CliFailure>(())
                })?;
            }
        }
    }
    if let Some(children) = failures.into_report() {
        Err(CliFailure::new(children.to_string()))
    } else {
        Ok(verdicts)
    }
}

struct Proof {
    root: CliResult<bool>,
    children: ChildReopenQualification,
}

fn exact_reopen(
    paths: &PlakPaths,
    request: &Request,
    scratch: Option<&Path>,
    session: &str,
    source: &SourceImage,
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
    let verdict: UpgradeReopenProbe =
        serde_json::from_slice(&process::run(command, PROBE_BOUND)?.stdout).map_err(|_| {
            CliFailure::new("same-key upgrade destination did not answer its exact-reopen probe")
        })?;
    let verdict = if let Some(children) = verdict.child_inspection {
        let baseline = children
            .needs_source()
            .then(|| {
                source.verified.recheck(&source.path)?;
                let baseline = child_probe(
                    &source.path,
                    paths,
                    &request.source_key,
                    None,
                    session,
                    PROBE_BOUND,
                )?;
                source.verified.recheck(&source.path)?;
                Ok::<_, CliFailure>(baseline)
            })
            .transpose()?;
        let children = children.qualify(
            baseline
                .as_ref()
                .map_or(ChildBaseline::Legacy(None), ChildBaseline::Exact),
        );
        return Ok(Proof {
            // As before, aggregate child refusals across roots before reporting.
            root: if children.regressions.is_empty() {
                root_verdict(
                    verdict.root.unwrap_or(verdict.verdict),
                    session,
                    allow_advance,
                )
            } else {
                Ok(false)
            },
            children,
        });
    } else {
        verdict.verdict
    };
    match verdict {
        ReopenProbe::Refused {
            children: Some(children),
            ..
        } => Ok(Proof {
            root: Ok(false),
            children: ChildReopenQualification {
                regressions: *children,
                ..Default::default()
            },
        }),
        verdict => Ok(Proof {
            root: root_verdict(verdict, session, allow_advance),
            children: ChildReopenQualification::default(),
        }),
    }
}

fn root_verdict(verdict: ReopenProbe, session: &str, allow_advance: bool) -> CliResult<bool> {
    match verdict {
        ReopenProbe::Opens => Ok(true),
        ReopenProbe::Unknown { reason }
            if reason == plakous_hosted::runtime::REOPEN_UNSETTLED_WORK =>
        {
            // Recheck under the holds; if still unfinished, the planned
            // handoff recovery law owns it rather than a qualification probe.
            Ok(false)
        }
        ReopenProbe::Unknown { reason } if allow_advance && reason == REOPEN_HEAD_ADVANCED => {
            Ok(false)
        }
        verdict => Err(CliFailure::new(format!(
            "host_upgrade_reopen_refused: {session}: {verdict:?}; same-key upgrade uses planned handoff, not carry",
        ))),
    }
}

fn child_probe(
    image: &Path,
    paths: &PlakPaths,
    key: &str,
    scratch: Option<&Path>,
    session: &str,
    bound: std::time::Duration,
) -> CliResult<ChildReopenInspection> {
    let mut command = process::command(image, paths.home(), key);
    command
        .args([
            "version",
            "--host-upgrade-reopen",
            "--child-binding-probe",
            "--reopen-probe",
        ])
        .arg(session);
    if let Some(scratch) = scratch {
        command.arg("--store-probe").arg(scratch);
    }
    serde_json::from_slice(&process::run(command, bound)?.stdout)
        .map_err(|_| CliFailure::new("host upgrade child binding probe did not answer"))
}

pub(super) fn legacy_preflight(
    paths: &PlakPaths,
    image: &Path,
    key: &str,
    scratch: Option<&Path>,
    deadline: Instant,
    audit: &LegacyChildBaseline,
) -> CliResult<()> {
    let store_paths = scratch.map(|scratch| paths.clone().with_probe_stores(scratch.to_path_buf()));
    let records = ProductStore::inspect_upgrade_roots(store_paths.as_ref().unwrap_or(paths))
        .map_err(CliFailure::new)?;
    let mut report = ChildReopenQualification::default();
    for record in records {
        if record.archived || record.plakous_locator.is_none() {
            continue;
        }
        let bound = deadline
            .saturating_duration_since(Instant::now())
            .min(PROBE_BOUND);
        if bound.is_zero() {
            return Err(CliFailure::new(
                "host_upgrade_preflight_timeout: no surrender was requested",
            ));
        }
        let children = child_probe(image, paths, key, scratch, &record.id, bound)?
            .qualify(ChildBaseline::Legacy(Some(audit)));
        report.regressions.merge(children.regressions);
        report.pre_existing.merge(children.pre_existing);
    }
    if !report.pre_existing.is_empty() {
        eprintln!("host upgrade: {}", report.pre_existing);
    }
    if let Some(refusal) = report.regressions.into_report() {
        return Err(CliFailure::new(refusal.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
