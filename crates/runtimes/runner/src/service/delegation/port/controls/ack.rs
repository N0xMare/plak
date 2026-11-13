use plakous_runner_protocol::RunnerFrame;
use plakous_tools::JobStatus;

use super::{RunnerService, refused, unknown};
use crate::service::lock;

impl RunnerService {
    pub(super) async fn relay_ack_numbers(
        &self,
        host: &str,
        instance: &str,
        request: &plakous_runner_protocol::Request,
        verb: plakous_tools::internal::runner::RunnerVerb,
    ) -> Result<(Vec<u32>, Vec<u32>), Box<RunnerFrame>> {
        if verb != plakous_tools::internal::runner::RunnerVerb::JobsAck {
            return Ok((Vec::new(), Vec::new()));
        }
        let fail = || unknown(request.request_id.clone(), "invalid job acknowledgement");
        let numbers: Vec<u32> = serde_json::from_value(
            request
                .operation
                .payload
                .get("numbers")
                .cloned()
                .ok_or_else(fail)?,
        )
        .map_err(|_| fail())?;
        let owner = request.owner.as_ref().ok_or_else(fail)?;
        let local = self
            .local_ack_numbers(host, instance, &owner.line, &numbers, &request.request_id)
            .await?;
        Ok((numbers, local))
    }

    pub(super) async fn local_ack_numbers(
        &self,
        host: &str,
        instance: &str,
        line: &str,
        numbers: &[u32],
        id: &str,
    ) -> Result<Vec<u32>, Box<RunnerFrame>> {
        let invalid = || {
            Box::new(refused(
                Some(id.into()),
                plakous_runner_protocol::Refusal::Malformed {
                    detail: "job acknowledgement needs distinct terminal numbers, at most 128"
                        .into(),
                },
            ))
        };
        if numbers.is_empty() || numbers.len() > 128 || numbers.contains(&0) {
            return Err(invalid());
        }
        let unique = numbers.iter().collect::<std::collections::BTreeSet<_>>();
        if unique.len() != numbers.len() {
            return Err(invalid());
        }
        let jobs = self
            .retained_jobs(host, instance, line)
            .await
            .map_err(|error| unknown(id.into(), error))?;
        if jobs
            .iter()
            .any(|job| numbers.contains(&job.number) && !job.terminal)
        {
            return Err(invalid());
        }
        let local = lock(&self.inner.lines)
            .get(&(host.into(), line.into()))
            .map(|(sessions, _)| sessions.clone());
        if local.is_some_and(|sessions| {
            sessions.jobs().iter().any(|job| {
                numbers.contains(&job.number)
                    && matches!(job.status, JobStatus::Running | JobStatus::OutcomeUnknown)
            })
        }) {
            return Err(invalid());
        }
        Ok(numbers
            .iter()
            .copied()
            .filter(|number| !jobs.iter().any(|job| job.number == *number))
            .collect())
    }
}
