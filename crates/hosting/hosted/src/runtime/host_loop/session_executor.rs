//! The host loop's executor, and the session tasks that end with it.

use std::time::Duration;

use plakous::application::{ApplicationExecutor, ApplicationExecutorError};

/// How long the end of a session waits for its aborted tasks to drop. A task
/// that does not yield in time is left to the executor's own bounded shutdown.
const SESSION_TASKS_JOIN_BOUND: Duration = Duration::from_millis(250);

/// One attachment's executor and the tasks its session owns on it.
///
/// Dropping it — at the end of release, or on any failed open — aborts and
/// joins every session task before the executor itself shuts down, so no
/// task the session spawned outlives it.
pub(in crate::runtime) struct SessionExecutor {
    tasks: crate::TaskSet,
    executor: ApplicationExecutor,
}

impl SessionExecutor {
    pub(in crate::runtime) fn new() -> Result<Self, ApplicationExecutorError> {
        Ok(Self {
            tasks: crate::TaskSet::new(),
            executor: ApplicationExecutor::new()?,
        })
    }

    /// Ends the session's tasks from inside the actor at release: aborts each
    /// and waits for them within the join bound. Dropping the executor does
    /// the same for an actor that never reached release.
    pub(in crate::runtime) async fn end_tasks(&self) {
        let _ = self.tasks.close(SESSION_TASKS_JOIN_BOUND).await;
    }

    /// The session's task set; every clone names the same set.
    pub(in crate::runtime) fn tasks(&self) -> &crate::TaskSet {
        &self.tasks
    }
}

impl std::ops::Deref for SessionExecutor {
    type Target = ApplicationExecutor;

    fn deref(&self) -> &ApplicationExecutor {
        &self.executor
    }
}

impl Drop for SessionExecutor {
    fn drop(&mut self) {
        let joined = self
            .executor
            .block_on(self.tasks.close(SESSION_TASKS_JOIN_BOUND));
        // Dropped inside a runtime: abort only; the executor's shutdown joins.
        if joined.is_err() {
            self.tasks.abort_all();
        }
    }
}

#[cfg(test)]
mod tests;
