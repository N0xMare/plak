//! Re-adoption of a runner's lines by a returning host (spec 01 §8.4): a
//! host process of the same home that comes back, within the old holder's
//! owner lease or after it, takes over each line of its store the runner
//! still holds. It takes a line with a newer owner, which only the holder of
//! the line's head lease can mint, so a live holder is never taken over.
//!
//! For each line the host opens the line's head lease-only (no driver,
//! route or credential, so it works while the session cannot fully open),
//! transfers the line on the runner to that lease's `(generation, fence)` so
//! the runner re-keys the line's processes to it, records the terminal jobs
//! the runner reports into the head's line terminal table under that fence,
//! and releases the lease. The head's next full open folds those terminals
//! into its ledger and transfers the line again under its own, newer fence.

use crate::test_support_runner::cut;
use std::str::FromStr as _;

use plakous::model::LineId;
use plakous::runner::RunnerClient;
use plakous::sessions::{AgentSessions, AgentSessionsErrorKind, SessionOperationId};
use plakous::tools::runner::{
    RunnerCall, RunnerFailure, RunnerLink, RunnerVerb, decode_jobs_snapshot,
};
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
    /// A live holder prevented acquisition; the sweep must skip its line,
    /// not block unrelated placement readiness or transfer its authority.
    pub held_elsewhere: bool,
}

struct AdoptionFailure {
    reason: String,
    held_elsewhere: bool,
}

impl From<String> for AdoptionFailure {
    fn from(reason: String) -> Self {
        Self {
            reason,
            held_elsewhere: false,
        }
    }
}

/// The host-mode start sweep: re-adopts every line `client`'s runner holds
/// for this home that `sessions`' store knows. Only the process that will
/// serve the lines runs it: a host process at its start, never an embedded
/// instance, which re-adopts a session's line by opening that session (its
/// first runner call transfers the line to the session's newer owner) and
/// never sweeps. The runner lists the lines of every process of the home,
/// of every state key: a line this store does not know is another store's,
/// and is skipped, never transferred, stopped or reported. Each known line
/// is independent: one that fails (its head is held by another owner, or the
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
    if crate::test_support_runner::selected("skip-readopt") {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut readopted = Vec::new();
    let mut failed = Vec::new();
    for status in client.lines().await? {
        if let Ok(line) = LineId::from_str(&status.line)
            && sessions
                .line_head(line)
                .await
                .is_err_and(|error| error.kind() == AgentSessionsErrorKind::NotFound)
        {
            continue;
        }
        match readopt_line(client, sessions, operation_prefix, &status.line, None).await {
            Ok(line) => readopted.push(line),
            Err(failure) => failed.push(ReadoptFailure {
                line: status.line,
                reason: failure.reason,
                held_elsewhere: failure.held_elsewhere,
            }),
        }
    }
    Ok((readopted, failed))
}

/// Reports inspected jobs of an obsolete runner without transferring it.
/// Existing full-open recovery settles missing promoted jobs as unknown.
///
pub async fn report_obsolete_lines(
    sessions: &AgentSessions,
    lines: Vec<plakous::runner::protocol::LineStatus>,
) {
    for status in lines {
        let line = match LineId::from_str(&status.line) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", line = %status.line, error = %error, "runner_recovery_failed");
                continue;
            }
        };
        match sessions.line_head(line).await {
            Ok(_) => {}
            Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", line = %line, error = %error, "runner_recovery_failed");
                continue;
            }
        }
        let jobs = match decode_jobs_snapshot(status.jobs) {
            Ok((_, jobs)) => jobs,
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "inspection", line = %line, error = %error, "runner_recovery_failed");
                continue;
            }
        };
        for job in jobs {
            tracing::info!(target: "plak.runner", line = %line, job = job.number, status = "outcome_unknown", "obsolete_protocol");
        }
    }
}

/// Finishes a recorded stop only while holding and transferring the line's head.
/// The intent identity keys the stop across crashes; another holder is never stopped.
///
/// # Errors
/// The lease, transfer, stop or terminal recording failed.
pub async fn finish_recorded_stop(
    client: &RunnerClient,
    sessions: &AgentSessions,
    line: LineId,
    intent: &str,
) -> Result<Readopted, String> {
    readopt_line(
        client,
        sessions,
        "plak:host:stop",
        &line.to_string(),
        Some(intent),
    )
    .await
    .map_err(|failure| failure.reason)
}

async fn readopt_line(
    client: &RunnerClient,
    sessions: &AgentSessions,
    operation_prefix: &str,
    line: &str,
    stop: Option<&str>,
) -> Result<Readopted, AdoptionFailure> {
    let line = LineId::from_str(line).map_err(|error| format!("line id: {error}"))?;
    let operation = SessionOperationId::new(format!("{operation_prefix}:{line}"))
        .map_err(|error| format!("operation id: {error}"))?;
    let head = sessions
        .open_lease_only(&operation, line)
        .await
        .map_err(|error| AdoptionFailure {
            held_elsewhere: matches!(
                error.kind(),
                AgentSessionsErrorKind::Ownership | AgentSessionsErrorKind::Busy
            ),
            reason: format!("the line's head could not be held: {error}"),
        })?;
    cut("lease");
    let adopted = readopt_held(client, &head, stop).await;
    let released = head
        .release()
        .await
        .map_err(|error| format!("the line's head lease could not be released: {error}"));
    let adopted = adopted?;
    released?;
    Ok(adopted)
}

pub(crate) async fn readopt_held(
    client: &RunnerClient,
    head: &plakous::sessions::LeaseOnlyHead,
    stop: Option<&str>,
) -> Result<Readopted, String> {
    let line = head.line();
    let owner = LineOwner::new(
        line,
        head.generation(),
        head.locator().branch_id(),
        head.fence(),
    );
    let (recorded, running) = async {
        let _ = client
            .transfer_owner(&owner)
            .await
            .map_err(|error| format!("the runner refused the transfer: {error:?}"))?;
        cut("transfer");
        if let Some(intent) = stop {
            cut("before-stop");
            let stopped = client
                .call(RunnerCall::recorded_control(
                    RunnerVerb::JobsStop,
                    serde_json::json!({"session": null, "reason": "user", "grace_ms": 200}),
                    owner,
                    intent.to_owned(),
                ))
                .await;
            if let Err(error) = stopped
                && error != RunnerFailure::Spent
            {
                return Err(format!("the recorded stop failed: {error:?}"));
            }
            cut("after-stop");
        }
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
        cut("terminals");
        let running = jobs
            .iter()
            .filter(|job| job.status == JobStatus::Running)
            .count();
        if stop.is_some() && running != 0 {
            return Err("the recorded stop left running jobs".to_owned());
        }
        Ok::<_, String>((recorded, running))
    }
    .await?;
    Ok(Readopted {
        line,
        owner,
        recorded,
        running,
    })
}

#[cfg(test)]
mod tests;
