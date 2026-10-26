use super::{AgentSessions, Arc, PathBuf};
pub(super) async fn recorded_sidecars(
    sessions: &AgentSessions,
    locator: plakous::sessions::SessionLocator,
) -> Result<Option<Arc<dyn plakous::tools::SidecarLocator>>, plakous::sessions::AgentSessionsError>
{
    Ok(Some(Arc::new(RecordedSidecar {
        artifact: sessions.created_composition_artifact(locator).await?,
    })))
}
pub(super) struct RecordedSidecar {
    pub(super) artifact: Option<plakous::agent::LocalCompositionEngineArtifact>,
}

impl plakous::tools::SidecarLocator for RecordedSidecar {
    fn javascript_host(&self) -> Result<PathBuf, String> {
        self.host(plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1)
    }

    fn python_host(&self) -> Result<PathBuf, String> {
        self.host(plakous::agent::LocalCompositionEngineArtifact::monty_worker_v1)
    }
}

impl RecordedSidecar {
    fn host(
        &self,
        verify: impl FnOnce(
            PathBuf,
        ) -> Result<
            plakous::agent::LocalCompositionEngineArtifact,
            plakous::agent::AgentError,
        >,
    ) -> Result<PathBuf, String> {
        let artifact = self
            .artifact
            .as_ref()
            .ok_or_else(|| "stored session has no evaluator attachment".to_owned())?;
        let live = verify(artifact.path().to_path_buf()).map_err(|error| error.to_string())?;
        if live.identity() != artifact.identity()
            || live.engine_contract_identity() != artifact.engine_contract_identity()
        {
            return Err("recorded evaluator engine or bytes disagree".to_owned());
        }
        Ok(artifact.path().to_path_buf())
    }
}
