//! Exact original-worker proofs supplement native observations without consuming output.
use super::{Inventory, Services, SessionLocator, failure};
use crate::{CliFailure, CliResult};
use plakous::runner::protocol::LineOwner;
use plakous::runner::protocol::retention::{Job, Status, StopReason};
use plakous::sessions::host_move::{MoveLineTerminal, MoveSource};

pub(crate) struct Proof {
    runner: String,
    owner: LineOwner,
    jobs: Vec<Job>,
}

fn client(services: &Services, runner: &str) -> CliResult<plakous::runner::RunnerClient> {
    if runner == "local" {
        services.local_runner_client()
    } else {
        services.runners().client(runner)
    }
    .ok_or_else(|| failure("original runner terminal proof is unavailable"))
}

impl Proof {
    pub(crate) fn recheck(
        &self,
        services: &Services,
        runtime: &tokio::runtime::Handle,
        before_surrender: bool,
    ) -> CliResult<()> {
        let mut current = runtime
            .block_on(client(services, &self.runner)?.job_terminals(&self.owner))
            .map_err(|error| {
                failure(format!(
                    "original runner terminal proof is unknown: {error:?}"
                ))
            })?;
        let fault = if before_surrender {
            "terminal-proof-before-surrender"
        } else {
            "terminal-proof-before-prepare"
        };
        if crate::test_hooks::upgrade_move_failure(fault) {
            current.clear();
        }
        if self.jobs.iter().any(|job| !current.contains(job)) {
            return Err(failure(
                "sampled original runner terminal changed or disappeared; no source surrendered",
            ));
        }
        Ok(())
    }
}

impl Inventory {
    pub(super) fn sample_terminals(
        &self,
        services: &Services,
        source: &mut MoveSource,
        runtime: &tokio::runtime::Handle,
    ) -> CliResult<Vec<Proof>> {
        let mut proofs = Vec::new();
        for line in &mut source.lines {
            let required = source
                .sessions
                .iter()
                .flat_map(|session| &session.checkpoints)
                .filter(|checkpoint| checkpoint.manifest().topology().line == line.line.line())
                .flat_map(|checkpoint| checkpoint.manifest().jobs().rows())
                .filter(|row| {
                    row.terminal.is_none()
                        && !line.terminals.iter().any(|terminal| {
                            terminal.number == row.number
                                && terminal.session_id == row.session_id
                                && terminal.owner_generation == row.owner_generation
                        })
                })
                .collect::<Vec<_>>();
            if required.is_empty() {
                continue;
            }
            let runner = self.runner_for(services.paths()?, line.line.head())?;
            let client = client(services, &runner)?;
            let owner = runtime
                .block_on(client.owner_floors(&[line.line.line().to_string()]))
                .map_err(|error| failure(format!("terminal owner proof is unknown: {error:?}")))?
                .into_iter()
                .next()
                .and_then(|floor| floor.owner)
                .ok_or_else(|| failure("terminal allocation has no exact original runner owner"))?;
            let branch = source
                .sessions
                .iter()
                .flat_map(|session| &session.branches)
                .find(|branch| branch.session.branch_id().to_string() == owner.branch)
                .ok_or_else(|| failure("terminal owner names an unrepresented source branch"))?;
            let checkpoint = source
                .sessions
                .iter()
                .flat_map(|session| &session.checkpoints)
                .find(|checkpoint| checkpoint.id() == branch.checkpoint)
                .ok_or_else(|| failure("terminal owner has no source checkpoint"))?;
            let topology = checkpoint.manifest().topology();
            if owner.line != line.line.line().to_string()
                || owner.generation != topology.line_generation
                || owner.fence > branch.fence_floor
                || topology.line != line.line.line()
            {
                return Err(failure(
                    "terminal witness is outside the exact carried source",
                ));
            }
            let mut all = runtime
                .block_on(client.job_terminals(&owner))
                .map_err(|error| {
                    failure(format!(
                        "original runner terminal proof is unknown: {error:?}"
                    ))
                })?;
            sample_reply_faults(&mut all)?;
            let mut jobs = Vec::new();
            for row in required {
                let job = all
                    .iter()
                    .find(|job| {
                        job.number == row.number
                            && job.session.as_str()
                                == plakous::tools::runner::hex(&row.session_id.as_bytes())
                            && job.owner_generation == row.owner_generation.get()
                    })
                    .ok_or_else(|| {
                        failure("unconsumed job lacks an exact original-worker terminal")
                    })?;
                if !jobs.contains(job) {
                    line.terminals.push(evidence(job)?);
                    jobs.push(job.clone());
                }
            }
            proofs.push(Proof {
                runner,
                owner,
                jobs,
            });
        }
        Ok(proofs)
    }

    pub(super) fn runner_for(
        &self,
        paths: &super::PlakPaths,
        head: plakous::sessions::host_move::MoveSessionRef,
    ) -> CliResult<String> {
        if let Some((record, _)) =
            super::ProductStore::inspect_move_context(paths, head.session_id(), head.branch_id())
                .map_err(failure)?
        {
            return Ok(record
                .runner
                .as_ref()
                .map_or("local", |runner| runner.runner_id.as_str())
                .into());
        }
        for node in self
            .trees
            .values()
            .flat_map(|tree| tree.cut.nodes().values())
        {
            if node.session_locator.as_ref().is_some_and(|locator| {
                locator.parse::<SessionLocator>().is_ok_and(|locator| {
                    locator.session_id() == head.session_id()
                        && locator.branch_id() == head.branch_id()
                })
            }) {
                return Ok(node
                    .placement
                    .runner_id
                    .clone()
                    .unwrap_or_else(|| "local".into()));
            }
        }
        Err(failure("terminal line has no represented runner placement"))
    }
}

fn sample_reply_faults(jobs: &mut [Job]) -> CliResult<()> {
    for job in jobs {
        if crate::test_hooks::upgrade_move_failure("terminal-proof-wrong-session") {
            job.session = plakous::runner::protocol::retention::Id::try_from(
                "ffffffffffffffffffffffffffffffff".to_owned(),
            )
            .map_err(failure)?;
        }
        if crate::test_hooks::upgrade_move_failure("terminal-proof-wrong-generation") {
            job.owner_generation = if job.owner_generation == 1 { 2 } else { 1 };
        }
    }
    Ok(())
}

fn evidence(job: &Job) -> CliResult<MoveLineTerminal> {
    use plakous::sessions::host_move::{
        MoveJobStatus, MoveJobTerminal, MoveProcessGeneration, MoveProcessId,
    };
    use plakous::tools::JobStopReason;
    let (status, exit_code, stopper) = match job.status {
        Status::Exited { code } => (MoveJobStatus::Exited, code, None),
        Status::Stopped { reason } => (
            MoveJobStatus::Stopped,
            None,
            Some(match reason {
                StopReason::Model => JobStopReason::Model,
                StopReason::User => JobStopReason::User,
                StopReason::SessionExit => JobStopReason::SessionExit,
                StopReason::AttemptEnded => JobStopReason::AttemptEnded,
                StopReason::HardCap => JobStopReason::HardCap,
            }),
        ),
        Status::Released {} => (MoveJobStatus::Released, None, None),
        Status::Running {} | Status::OutcomeUnknown {} => {
            return Err(failure("job outcome is not an original terminal"));
        }
    };
    let bytes: [u8; 16] = plakous::tools::runner::unhex(job.session.as_str())
        .map_err(failure)?
        .try_into()
        .map_err(|_| failure("invalid terminal allocation"))?;
    let terminal = MoveLineTerminal {
        number: job.number,
        session_id: MoveProcessId::new(bytes).map_err(failure)?,
        owner_generation: MoveProcessGeneration::new(job.owner_generation).map_err(failure)?,
        terminal: MoveJobTerminal {
            status,
            exit_code,
            stopper,
            elapsed: std::time::Duration::from_millis(job.elapsed_ms),
            tail: job.output.clone(),
        },
    };
    if !job.terminal {
        return Err(CliFailure::new("job lacks original-worker terminal proof"));
    }
    terminal.encode().map_err(failure)?;
    Ok(terminal)
}
