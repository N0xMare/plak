//! Promoted-job metadata and number floors; never process authority, PIDs or handles.
use plakous::model::Timestamp;
use plakous::sessions::host_move::{
    MoveJob, MoveJobCut, MoveJobStatus, MoveJobTerminal, MoveJobs, MoveLineTerminal,
    MoveProcessGeneration, MoveProcessId,
};
use plakous::tools::{JobOwner, JobStopReason};
use serde::{Deserialize, Serialize};

mod inspection;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobCut {
    Settled,
    Retained,
}

impl JobCut {
    pub(crate) fn validate(self, jobs: &Jobs) -> Result<(), String> {
        if matches!(self, Self::Settled) && jobs.rows.iter().any(|row| row.terminal.is_none()) {
            return Err("settled move contains a retained job".into());
        }
        Ok(())
    }

    pub(crate) fn native(self) -> MoveJobCut {
        match self {
            Self::Settled => MoveJobCut::Settled,
            Self::Retained => MoveJobCut::Retained,
        }
    }
    pub(crate) fn from_native(cut: MoveJobCut) -> Self {
        match cut {
            MoveJobCut::Settled => Self::Settled,
            MoveJobCut::Retained => Self::Retained,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Jobs {
    next_number: u32,
    rows: Vec<Row>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Row {
    number: u32,
    command: String,
    started_seconds: u64,
    started_nanos: u32,
    owner: Owner,
    session: [u8; 16],
    owner_generation: u64,
    terminal: Option<Terminal>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    line: String,
    generation: u64,
    session: String,
    branch: String,
    fence: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Terminal {
    number: u32,
    process: [u8; 16],
    generation: u64,
    status: Status,
    elapsed_seconds: u64,
    elapsed_nanos: u32,
    tail: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Status {
    Exited { code: Option<i32> },
    Stopped { by: Stopper },
    Released {},
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Stopper {
    Model,
    User,
    SessionExit,
    AttemptEnded,
    HardCap,
}

impl Jobs {
    pub(crate) fn capture(jobs: &MoveJobs) -> Result<Self, String> {
        Ok(Self {
            next_number: jobs.next_id(),
            rows: jobs
                .rows()
                .iter()
                .map(|row| {
                    Ok(Row {
                        number: row.number,
                        command: row.command.clone(),
                        started_seconds: row.started_at.seconds(),
                        started_nanos: row.started_at.nanoseconds(),
                        owner: Owner {
                            line: row.owner.line.to_string(),
                            generation: row.owner.generation,
                            session: row.owner.session_id.to_string(),
                            branch: row.owner.branch_id.to_string(),
                            fence: row.owner.fence,
                        },
                        session: row.session_id.as_bytes(),
                        owner_generation: row.owner_generation.get(),
                        terminal: row
                            .terminal
                            .clone()
                            .map(|terminal| {
                                Terminal::capture(&MoveLineTerminal {
                                    number: row.number,
                                    session_id: row.session_id,
                                    owner_generation: row.owner_generation,
                                    terminal,
                                })
                            })
                            .transpose()?,
                    })
                })
                .collect::<Result<_, String>>()?,
        })
    }

    pub(crate) fn decode(self) -> Result<MoveJobs, String> {
        let rows = self
            .rows
            .into_iter()
            .map(|row| {
                let session_id = MoveProcessId::new(row.session).map_err(|e| e.to_string())?;
                let owner_generation =
                    MoveProcessGeneration::new(row.owner_generation).map_err(|e| e.to_string())?;
                let evidence = row.terminal.map(Terminal::decode).transpose()?;
                if evidence.as_ref().is_some_and(|evidence| {
                    row.number != evidence.number
                        || session_id != evidence.session_id
                        || owner_generation != evidence.owner_generation
                }) {
                    return Err("move job terminal allocation mismatch".into());
                }
                Ok(MoveJob {
                    number: row.number,
                    command: row.command,
                    started_at: Timestamp::new(row.started_seconds, row.started_nanos)
                        .map_err(|e| e.to_string())?,
                    owner: JobOwner {
                        line: row
                            .owner
                            .line
                            .parse()
                            .map_err(|_| "invalid move job line")?,
                        generation: row.owner.generation,
                        session_id: row
                            .owner
                            .session
                            .parse()
                            .map_err(|_| "invalid move job session")?,
                        branch_id: row
                            .owner
                            .branch
                            .parse()
                            .map_err(|_| "invalid move job branch")?,
                        fence: row.owner.fence,
                    },
                    session_id,
                    owner_generation,
                    terminal: evidence.map(|evidence| evidence.terminal),
                })
            })
            .collect::<Result<_, String>>()?;
        MoveJobs::new(self.next_number, rows).map_err(|e| e.to_string())
    }
}

impl Terminal {
    pub(crate) fn capture(evidence: &MoveLineTerminal) -> Result<Self, String> {
        let status = match evidence.terminal.status {
            MoveJobStatus::Exited => Status::Exited {
                code: evidence.terminal.exit_code,
            },
            MoveJobStatus::Released => Status::Released {},
            MoveJobStatus::Stopped => Status::Stopped {
                by: match evidence
                    .terminal
                    .stopper
                    .ok_or("stopped move job has no attribution")?
                {
                    JobStopReason::Model => Stopper::Model,
                    JobStopReason::User => Stopper::User,
                    JobStopReason::SessionExit => Stopper::SessionExit,
                    JobStopReason::AttemptEnded => Stopper::AttemptEnded,
                    JobStopReason::HardCap => Stopper::HardCap,
                },
            },
        };
        Ok(Self {
            number: evidence.number,
            process: evidence.session_id.as_bytes(),
            generation: evidence.owner_generation.get(),
            status,
            elapsed_seconds: evidence.terminal.elapsed.as_secs(),
            elapsed_nanos: evidence.terminal.elapsed.subsec_nanos(),
            tail: evidence.terminal.tail.clone(),
        })
    }

    pub(crate) fn decode(self) -> Result<MoveLineTerminal, String> {
        if self.number == 0
            || self.elapsed_nanos >= 1_000_000_000
            || self.tail.len() > 4096
            || self.tail.contains('\0')
        {
            return Err("invalid move job evidence".into());
        }
        let (status, exit_code, stopper) = match self.status {
            Status::Exited { code } => (MoveJobStatus::Exited, code, None),
            Status::Released {} => (MoveJobStatus::Released, None, None),
            Status::Stopped { by } => (
                MoveJobStatus::Stopped,
                None,
                Some(match by {
                    Stopper::Model => JobStopReason::Model,
                    Stopper::User => JobStopReason::User,
                    Stopper::SessionExit => JobStopReason::SessionExit,
                    Stopper::AttemptEnded => JobStopReason::AttemptEnded,
                    Stopper::HardCap => JobStopReason::HardCap,
                }),
            ),
        };
        Ok(MoveLineTerminal {
            number: self.number,
            session_id: MoveProcessId::new(self.process).map_err(|e| e.to_string())?,
            owner_generation: MoveProcessGeneration::new(self.generation)
                .map_err(|e| e.to_string())?,
            terminal: MoveJobTerminal {
                status,
                exit_code,
                stopper,
                elapsed: std::time::Duration::new(self.elapsed_seconds, self.elapsed_nanos),
                tail: self.tail,
            },
        })
    }
}
