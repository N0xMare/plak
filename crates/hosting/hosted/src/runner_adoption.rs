//! Re-adoption of a runner's lines by a returning host (spec 01 §8.5): a
//! host with the same identity that comes back, within its owner lease or
//! after it, takes over each line the runner still holds for it.
//!
//! For each line the host opens the line's head lease-only (no driver,
//! route or credential, so it works while the session cannot fully open),
//! transfers the line on the runner to that lease's `(generation, fence)` so
//! the runner re-keys the line's processes to it, records the terminal jobs
//! the runner reports into the head's line terminal table under that fence,
//! and releases the lease. The head's next full open folds those terminals
//! into its ledger and transfers the line again under its own, newer fence.

use std::str::FromStr as _;

use plakous::model::LineId;
use plakous::runner::RunnerClient;
use plakous::sessions::{AgentSessions, SessionOperationId};
use plakous::tools::runner::{RunnerFailure, decode_jobs_snapshot};
use plakous::tools::{JobStatus, LineOwner};

/// One line a returning host re-adopted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Readopted {
    /// The line.
    pub line: LineId,
    /// The owner the runner now accepts for it: the lease-only head's.
    pub owner: LineOwner,
    /// The jobs whose terminals were recorded into the head now.
    pub recorded: Vec<u32>,
    /// The jobs still running on the runner.
    pub running: usize,
}

/// Why one line was not re-adopted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadoptFailure {
    /// The line, as the runner named it.
    pub line: String,
    /// Why.
    pub reason: String,
}

/// Re-adopts every line `client`'s runner holds for this host. Each line is
/// independent: one that fails (its head is held by another owner, or the
/// runner refused the transfer) is reported and the rest go on.
///
/// # Errors
///
/// The runner could not list its lines.
pub async fn readopt_lines(
    client: &RunnerClient,
    sessions: &AgentSessions,
    operation_prefix: &str,
) -> Result<(Vec<Readopted>, Vec<ReadoptFailure>), RunnerFailure> {
    let mut readopted = Vec::new();
    let mut failed = Vec::new();
    for status in client.lines().await? {
        match readopt_line(client, sessions, operation_prefix, &status.line).await {
            Ok(line) => readopted.push(line),
            Err(reason) => failed.push(ReadoptFailure {
                line: status.line,
                reason,
            }),
        }
    }
    Ok((readopted, failed))
}

async fn readopt_line(
    client: &RunnerClient,
    sessions: &AgentSessions,
    operation_prefix: &str,
    line: &str,
) -> Result<Readopted, String> {
    let line = LineId::from_str(line).map_err(|error| format!("line id: {error}"))?;
    let operation = SessionOperationId::new(format!("{operation_prefix}:{line}"))
        .map_err(|error| format!("operation id: {error}"))?;
    let head = sessions
        .open_lease_only(&operation, line)
        .await
        .map_err(|error| format!("the line's head could not be held: {error}"))?;
    let owner = LineOwner::new(
        line,
        head.generation(),
        head.locator().branch_id(),
        head.fence(),
    );
    let adopted = async {
        client
            .transfer_owner(&owner)
            .await
            .map_err(|error| format!("the runner refused the transfer: {error:?}"))?;
        // The line table read after the transfer is the one this owner holds.
        let jobs = client
            .lines()
            .await
            .map_err(|error| format!("the runner's lines: {error:?}"))?
            .into_iter()
            .find(|status| status.line == line.to_string())
            .map_or_else(
                || Ok(Vec::new()),
                |status| {
                    decode_jobs_snapshot(status.jobs)
                        .map(|(_, jobs)| jobs)
                        .map_err(|error| format!("the line's jobs: {error}"))
                },
            )?;
        let recorded = head
            .record_job_terminals(&jobs)
            .await
            .map_err(|error| format!("the job terminals could not be recorded: {error}"))?;
        let running = jobs
            .iter()
            .filter(|job| job.status == JobStatus::Running)
            .count();
        Ok::<_, String>((recorded, running))
    }
    .await;
    let released = head
        .release()
        .await
        .map_err(|error| format!("the line's head lease could not be released: {error}"));
    let (recorded, running) = adopted?;
    released?;
    Ok(Readopted {
        line,
        owner,
        recorded,
        running,
    })
}

#[cfg(test)]
mod tests;
