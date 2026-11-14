//! Original-worker proofs, never an observation interpreted as consumption.
use super::{InspectionAnswer, LineOwner, RunnerClient, RunnerFailure, lock};
use plakous_runner_protocol::retention::{Job, PAGE_SIZE, Status};

const MAX_ROWS: usize = 4096;

impl RunnerClient {
    /// Reads every terminal page for one exact accepted owner, without effects or acknowledgement.
    ///
    /// # Errors
    /// Refuses inspection/qualification, owner drift, incomplete/malformed pages or interrupted reads.
    #[doc(hidden)]
    pub async fn job_terminals(&self, owner: &LineOwner) -> Result<Vec<Job>, RunnerFailure> {
        if self.inner.qualification || self.inner.bind.inspect {
            return Err(RunnerFailure::NotSent(
                "job terminals require a normal bound host".into(),
            ));
        }
        let mut after = 0;
        let mut jobs = Vec::new();
        loop {
            let (page, next) = self.terminal_page(owner, after).await?;
            validate_page(after, &page, next)?;
            jobs.extend(page);
            if jobs.len() > MAX_ROWS {
                return Err(incomplete());
            }
            let Some(next) = next else { return Ok(jobs) };
            after = next;
        }
    }

    async fn terminal_page(
        &self,
        expected: &LineOwner,
        after: u32,
    ) -> Result<(Vec<Job>, Option<u32>), RunnerFailure> {
        let id = self.inner.next_id("terminals");
        let (waiter, answer) = tokio::sync::oneshot::channel();
        lock(&self.inner.state).questions.insert(id.clone(), waiter);
        let _question = Question {
            client: self,
            id: &id,
        };
        if !self
            .inner
            .send(plakous_runner_protocol::HostFrame::JobTerminals {
                request_id: id.clone(),
                owner: expected.clone(),
                after,
            })
        {
            return Err(incomplete());
        }
        match tokio::time::timeout(self.inner.windows.read, answer).await {
            Ok(Ok(Ok(InspectionAnswer::JobTerminals { owner, jobs, next })))
                if owner == *expected =>
            {
                Ok((jobs, next))
            }
            Ok(Ok(Err(error))) => Err(error),
            _ => Err(incomplete()),
        }
    }
}

struct Question<'a> {
    client: &'a RunnerClient,
    id: &'a str,
}

impl Drop for Question<'_> {
    fn drop(&mut self) {
        lock(&self.client.inner.state).questions.remove(self.id);
    }
}

fn validate_page(after: u32, jobs: &[Job], next: Option<u32>) -> Result<(), RunnerFailure> {
    let mut previous = after;
    for job in jobs {
        if job.number <= previous
            || !job.terminal
            || matches!(job.status, Status::Running {} | Status::OutcomeUnknown {})
        {
            return Err(incomplete());
        }
        previous = job.number;
    }
    if jobs.len() > PAGE_SIZE
        || next.is_some_and(|next| jobs.len() != PAGE_SIZE || next != previous || next <= after)
    {
        return Err(incomplete());
    }
    Ok(())
}

fn incomplete() -> RunnerFailure {
    RunnerFailure::ReadInterrupted(
        "job-terminal proof read is incomplete, changed or unavailable".into(),
    )
}
