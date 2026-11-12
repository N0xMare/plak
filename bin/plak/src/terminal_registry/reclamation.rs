//! Runner-owned leases protect host-local effects without persisted pin state.

use super::{
    OperatorAnswer, OperatorNamespace, OperatorOperation, Path, PlakPaths, ReclamationAdmission,
    ReclamationFence, RunnerClient, TerminalRegistry, VcsError, id,
};
use plakous_hosted::workspace_reclamation::ReclamationFuture;

fn unavailable() -> VcsError {
    VcsError::mint_unavailable("operator terminal workspace authority unavailable")
}

async fn lease(
    client: &RunnerClient,
    namespace: OperatorNamespace,
    path: &Path,
) -> Result<Box<dyn ReclamationAdmission>, VcsError> {
    let guard = client
        .operator_reclaim(namespace, path.to_str().ok_or_else(unavailable)?.into())
        .await
        .map_err(|_| unavailable())?;
    Ok(Box::new(guard))
}

impl ReclamationFence for TerminalRegistry {
    fn admit<'a>(&'a self, runner: &'a str, path: &'a Path) -> ReclamationFuture<'a> {
        Box::pin(async move {
            let local = self.admit_local(runner, path)?;
            let binding = self.binding(runner).map_err(|_| unavailable())?;
            let namespace = OperatorNamespace {
                state_key: self.state_key.clone(),
                principal: id("host-reclamation".into()).map_err(|_| unavailable())?,
            };
            let remote = lease(&binding.client, namespace, path).await?;
            if !binding.revision.is_current() {
                return Err(unavailable());
            }
            Ok(Box::new((remote, local)) as Box<dyn ReclamationAdmission>)
        })
    }
}

pub(crate) struct RunnerFence {
    paths: PlakPaths,
    local: Option<RunnerClient>,
}

impl RunnerFence {
    pub(crate) const fn new(paths: PlakPaths, local: Option<RunnerClient>) -> Self {
        Self { paths, local }
    }
}

impl ReclamationFence for RunnerFence {
    fn admit<'a>(&'a self, runner: &'a str, path: &'a Path) -> ReclamationFuture<'a> {
        Box::pin(async move {
            // Remote VCS execution already holds the same runner's reclaim guard.
            if runner != "local" {
                return Ok(Box::new(()) as Box<dyn ReclamationAdmission>);
            }
            let client = match &self.local {
                Some(client) if client.is_connected() => client.clone(),
                _ => match crate::commands::runner::local_reclamation(&self.paths)
                    .await
                    .map_err(|_| unavailable())?
                {
                    crate::commands::runner::LocalReclamation::Quiet(lock) => {
                        return Ok(Box::new(lock) as Box<dyn ReclamationAdmission>);
                    }
                    crate::commands::runner::LocalReclamation::Live(client) => client,
                },
            };
            lease(
                &client,
                OperatorNamespace {
                    state_key: id(crate::home::state::state_key()).map_err(|_| unavailable())?,
                    principal: id("host-reclamation".into()).map_err(|_| unavailable())?,
                },
                path,
            )
            .await
        })
    }
}

pub(crate) fn session_pinned(
    services: &crate::shell::services::Services,
    session: &str,
    runner: &str,
) -> Result<bool, String> {
    if let Some(registry) = services.terminal_registry() {
        registry
            .binding(runner)
            .map_err(|_| "terminal inventory unavailable")?;
        return Ok(registry.resident_reason(session).is_some());
    }
    let client = services.runners().client(runner).or_else(|| {
        if runner == "local" {
            services.local_runner_client()
        } else {
            None
        }
    });
    let paths = services
        .paths()
        .map_err(|_| "terminal home unavailable")?
        .clone();
    let local = runner == "local";
    let session = session.to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "terminal inventory worker unavailable")?;
        runtime.block_on(async move {
            let client = match client {
                Some(client) => client,
                None if local => match crate::commands::runner::local_reclamation(&paths)
                    .await
                    .map_err(|_| "terminal inventory unavailable")?
                {
                    crate::commands::runner::LocalReclamation::Quiet(_lock) => return Ok(false),
                    crate::commands::runner::LocalReclamation::Live(client) => client,
                },
                None => return Err("terminal inventory unavailable".to_owned()),
            };
            let answer = client
                .operator_control(
                    OperatorNamespace {
                        state_key: id(crate::home::state::state_key())
                            .map_err(|_| "terminal state unavailable")?,
                        principal: id("host-inventory".into())
                            .map_err(|_| "terminal principal unavailable")?,
                    },
                    OperatorOperation::Inventory,
                )
                .await
                .map_err(|_| "terminal inventory unavailable")?;
            match answer {
                OperatorAnswer::Inventory { instance, pins }
                    if instance.as_str() == client.instance() =>
                {
                    Ok(pins
                        .iter()
                        .any(|pin| pin.terminal.key.session.as_str() == session))
                }
                _ => Err("terminal inventory unavailable".to_owned()),
            }
        })
    })
    .join()
    .map_err(|_| "terminal inventory worker failed")?
}
