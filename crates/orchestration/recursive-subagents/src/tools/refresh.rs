//! Prepare and settle a follow-up's catch-up before opening its next attempt.

use super::*;

impl TopologyTool {
    pub(super) async fn refresh_followup(
        &self,
        caller: &str,
        request: &SendAgentMessageRequest,
        operation: &str,
    ) -> Result<Option<ToolOutcome>, ToolRuntimeError> {
        loop {
            let running = self
                .driver
                .job_service(&request.agent_id)
                .is_some_and(|service| {
                    service
                        .jobs()
                        .iter()
                        .any(|job| job.status == plakous::tools::JobStatus::Running)
                });
            let driver = self.driver.clone();
            let (caller, request, operation) =
                (caller.to_owned(), request.clone(), operation.to_owned());
            let (prepared, effects) = self
                .driver
                .mutate(move |tree| {
                    driver.fold_and_commit(tree, |tree| {
                        tree.prepare_followup_refresh(&caller, &request, &operation, running)
                    })
                })
                .await
                .map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
            if let Err(error) = prepared {
                return Self::topology_failure(&error).map(Some);
            }
            if effects.is_empty() {
                return Ok(None);
            }
            let settled = match self.settle_typed(effects).await {
                Ok(settled) => settled,
                Err(error) => {
                    return Self::topology_failure(&error).map(Some);
                }
            };
            if let Some(error) = settled.cleanup_failures.first() {
                return Self::topology_failure(error).map(Some);
            }
        }
    }
}
