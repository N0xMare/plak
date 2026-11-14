//! Normal-host proof reads neither transfer native ownership nor consume a job.
use super::{RunnerFrame, RunnerService, lock, refused};
use plakous_runner_protocol::retention::{Job, PAGE_SIZE, Status};
use plakous_runner_protocol::{LineOwner, Refusal};

const TAIL_BYTES: usize = 4096;

impl RunnerService {
    pub(super) fn receive_job_terminals(
        &self,
        host: &str,
        instance: &str,
        request_id: String,
        owner: LineOwner,
        after: u32,
        sender: &tokio::sync::mpsc::UnboundedSender<RunnerFrame>,
    ) {
        let service = self.clone();
        let host = host.to_owned();
        let instance = instance.to_owned();
        let sender = sender.clone();
        tokio::spawn(async move {
            let _ = sender.send(
                service
                    .job_terminals(&host, &instance, request_id, &owner, after)
                    .await,
            );
        });
    }

    pub(super) async fn job_terminals(
        &self,
        host: &str,
        instance: &str,
        request_id: String,
        owner: &LineOwner,
        after: u32,
    ) -> RunnerFrame {
        let result = self
            .terminal_rows(host, instance, owner)
            .await
            .map(|mut jobs| {
                jobs.retain(|job| job.number > after);
                let more = jobs.len() > PAGE_SIZE;
                jobs.truncate(PAGE_SIZE);
                let next = more.then(|| jobs.last().map(|job| job.number)).flatten();
                RunnerFrame::JobTerminals {
                    request_id: request_id.clone(),
                    owner: owner.clone(),
                    jobs,
                    next,
                }
            });
        result.unwrap_or_else(|refusal| refused(Some(request_id), refusal))
    }

    async fn terminal_rows(
        &self,
        host: &str,
        instance: &str,
        owner: &LineOwner,
    ) -> Result<Vec<Job>, Refusal> {
        self.require_terminal_owner(host, owner)?;
        let native = lock(&self.inner.lines)
            .get(&(host.into(), owner.line.clone()))
            .cloned();
        let mut jobs = std::collections::BTreeMap::new();
        if let Some((sessions, _)) = native {
            for view in sessions.jobs() {
                let terminal = sessions
                    .session_for_number(view.number)
                    .is_some_and(|id| sessions.joined_terminal_status(id).is_some());
                if terminal {
                    let job =
                        super::retention::job(view, true).map_err(|_| Refusal::Malformed {
                            detail: "invalid original job terminal".into(),
                        })?;
                    jobs.insert(job.number, job);
                }
            }
        }
        if let Some(context) = self.retention_context() {
            let scope = super::delegation::scope(host, instance)
                .map_err(|detail| Refusal::Malformed { detail })?;
            for entry in context.registry.entries(host, Some(&owner.line)) {
                if entry.confirmed && entry.inventory.jobs.is_empty() {
                    continue;
                }
                if entry.inventory.lost || super::retention::native_owner(&entry.to) != *owner {
                    return Err(unavailable());
                }
                let fresh = self
                    .refresh_retained(&scope, &entry.inventory.origin, &entry.to)
                    .await
                    .map_err(|_| unavailable())?;
                for job in fresh {
                    if job.terminal
                        && !matches!(job.status, Status::Running {} | Status::OutcomeUnknown {})
                        && let Some(current) = jobs.insert(job.number, job.clone())
                        && current != job
                    {
                        return Err(Refusal::Malformed {
                            detail: "conflicting original job terminals".into(),
                        });
                    }
                }
            }
        }
        self.require_terminal_owner(host, owner)?;
        let mut jobs: Vec<_> = jobs
            .into_values()
            .filter(|job| !matches!(job.status, Status::Running {} | Status::OutcomeUnknown {}))
            .collect();
        for job in &mut jobs {
            if job.output.len() > TAIL_BYTES {
                let mut start = job.output.len() - TAIL_BYTES;
                while !job.output.is_char_boundary(start) {
                    start += 1;
                }
                job.output = job.output[start..].to_owned();
            }
        }
        Ok(jobs)
    }

    fn require_terminal_owner(&self, host: &str, owner: &LineOwner) -> Result<(), Refusal> {
        let tables = lock(&self.inner.tables);
        let floors = tables
            .read_owner_floors(host, std::slice::from_ref(&owner.line))
            .map_err(|_| Refusal::Unsupported {
                what: "durable owner floor cannot be read for terminal proof".into(),
            })?;
        if floors.first().and_then(|floor| floor.owner.as_ref()) != Some(owner)
            || tables
                .owner(host, &owner.line)
                .as_ref()
                .map(|(current, _)| current)
                != Some(owner)
        {
            return Err(Refusal::OwnerNotTransferred);
        }
        Ok(())
    }
}

fn unavailable() -> Refusal {
    Refusal::Unsupported {
        what: "original retained worker cannot prove its current terminal set".into(),
    }
}
