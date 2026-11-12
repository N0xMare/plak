use super::{ReadRefusal, WorkspaceReadFailure, WorkspaceTarget, failure};
use plakous::tools::{ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope};
use plakous_host_protocol::topology::WorkspaceStatus;
use plakous_host_protocol::wire::RequestRefusal;
use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore};
use std::path::{Path, PathBuf};

pub(super) struct Workspace {
    pub environment: ExecutionEnvironment,
    pub identity: String,
    pub placement: Option<Placement>,
    pub target: WorkspaceTarget,
}

#[derive(Clone)]
pub(super) struct Placement {
    registry: plakous::runner::RunnerRegistry,
    runner: String,
}
impl Placement {
    pub(super) fn refusal(&self) -> Option<RequestRefusal> {
        self.registry.services(&self.runner).err().map(|reason| {
            super::super::super::sessions::runner_views::refusal(
                &super::super::super::sessions::runner_views::label(
                    &self.runner,
                    &self.registry.rows(),
                ),
                reason,
                &self.registry.rows(),
            )
        })
    }
}
pub(super) enum Failure {
    Product(ReadRefusal),
    Placement(RequestRefusal),
}

fn location(
    services: &crate::shell::services::Services,
    store: &plakous_recursive_subagents_store::SqliteTreeStore,
    target: &WorkspaceTarget,
) -> Result<(PathBuf, String, WorkspaceTarget), Failure> {
    let missing = || Failure::Product(ReadRefusal::WorkspaceUnavailable {});
    let record = services
        .session_records()
        .map_err(|_| missing())?
        .load(&target.session)
        .map_err(|_| missing())?;
    let root_node = record.node.agent_id.as_str();
    let node = target.node.as_deref().unwrap_or(root_node);
    let (path, runner) = if node == root_node {
        (
            record.workspace_root.clone(),
            record
                .runner
                .as_ref()
                .map_or("local", |r| r.runner_id.as_str())
                .to_owned(),
        )
    } else {
        let tree = store
            .load(&record.node.tree_id)
            .map_err(|_| missing())?
            .map(ProductAgentTree::reopen)
            .ok_or_else(missing)?;
        let node = tree.node(node).map_err(|_| missing())?;
        let workspace = tree.workspace(&node.workspace_id).ok_or_else(missing)?;
        if matches!(
            workspace.status,
            WorkspaceStatus::CleanRemoved
                | WorkspaceStatus::Discarded
                | WorkspaceStatus::Discarding
        ) {
            return Err(missing());
        }
        let path = node
            .placement
            .workspace
            .as_ref()
            .map_or_else(|| workspace.path.clone(), |w| w.root.clone());
        let runner = node
            .placement
            .runner_id
            .as_deref()
            .unwrap_or("local")
            .to_owned();
        (path, runner)
    };
    Ok((
        path,
        runner,
        WorkspaceTarget {
            session: target.session.clone(),
            node: Some(node.to_owned()),
        },
    ))
}

pub(super) fn saved(
    services: &crate::shell::services::Services,
    store: &plakous_recursive_subagents_store::SqliteTreeStore,
    target: &WorkspaceTarget,
    expires: std::time::Instant,
    cancellation: plakous::model::CancellationToken,
) -> Result<Workspace, Failure> {
    let missing = || Failure::Product(ReadRefusal::WorkspaceUnavailable {});
    let (path, runner, target) = location(services, store, target)?;
    let registry = services.runners();
    let placement = Placement {
        registry: registry.clone(),
        runner: runner.clone(),
    };
    let remote = registry.services(&runner).map_err(|reason| {
        Failure::Placement(super::super::super::sessions::runner_views::refusal(
            &super::super::super::sessions::runner_views::label(&runner, &registry.rows()),
            reason,
            &registry.rows(),
        ))
    })?;
    let scope = ExecutionEnvironmentScope::Run(
        plakous::model::RunId::try_from(uuid::Uuid::new_v4()).map_err(|_| missing())?,
    );
    let (environment, identity) = if let Some(remote) = remote {
        let backend = remote.backend();
        let link = std::sync::Arc::new(BoundedProbe {
            link: backend.link(),
            expires,
            cancellation,
        });
        let backend = std::sync::Arc::new(
            plakous::tools::runner::RunnerBackend::new(
                serde_json::to_value(backend.descriptor()).map_err(|_| missing())?,
                link,
            )
            .map_err(|_| missing())?,
        );
        let instance = registry.client(&runner).ok_or_else(missing)?.instance();
        let identity = format!("{runner}:{instance}");
        let environment = plakous::tools::runner::runner_environment(
            scope,
            &path,
            &path,
            ExecutionEnvironmentConfig::default(),
            backend,
        )
        .map_err(|_| placement.refusal().map_or_else(missing, Failure::Placement))?;
        if environment.root() != path {
            return Err(missing());
        }
        (environment, identity)
    } else {
        let paths = services.paths().map_err(|_| missing())?;
        let state = services.home().state().map_err(|_| missing())?.join("plak");
        protect(&path, &[paths.home().to_path_buf(), state]).map_err(Failure::Product)?;
        let environment =
            ExecutionEnvironment::host(scope, &path, &path, ExecutionEnvironmentConfig::default())
                .map_err(|_| missing())?;
        if environment.root() != path {
            return Err(missing());
        }
        protect(
            environment.root(),
            &[
                paths.home().to_path_buf(),
                services.home().state().map_err(|_| missing())?.join("plak"),
            ],
        )
        .map_err(Failure::Product)?;
        // Local environments are rebuilt per request. Bind to physical root identity,
        // not their freshly allocated backend authority.
        let object = environment.descriptor().root_object();
        let identity = format!(
            "local:{}:{}:{}",
            path.display(),
            object.device(),
            object.inode()
        );
        (environment, identity)
    };
    Ok(Workspace {
        environment,
        identity,
        placement: Some(placement),
        target,
    })
}

// Construction uses synchronous probe ports. Bound their underlying futures too,
// so dropping the async requester cannot leave its quota pinned in spawn_blocking.
struct BoundedProbe {
    link: std::sync::Arc<dyn plakous::tools::runner::RunnerLink>,
    expires: std::time::Instant,
    cancellation: plakous::model::CancellationToken,
}
impl plakous::tools::runner::RunnerLink for BoundedProbe {
    fn call(
        &self,
        call: plakous::tools::runner::RunnerCall,
    ) -> plakous::tools::runner::RunnerCallFuture {
        if std::time::Instant::now() >= self.expires || self.cancellation.is_cancelled() {
            return Box::pin(std::future::ready(Err(
                plakous::tools::runner::RunnerFailure::ReadInterrupted(
                    "inspection deadline".into(),
                ),
            )));
        }
        let call_cancellation = call.cancellation().clone();
        let answer = self.link.call(call);
        let expires = tokio::time::Instant::from_std(self.expires);
        let cancellation = self.cancellation.clone();
        Box::pin(async move {
            let _cancel = super::Cancel(call_cancellation);
            tokio::select! {
                result = answer => result,
                () = cancellation.cancelled() => Err(plakous::tools::runner::RunnerFailure::ReadInterrupted("inspection cancelled".into())),
                () = tokio::time::sleep_until(expires) => Err(plakous::tools::runner::RunnerFailure::ReadInterrupted("inspection deadline".into())),
            }
        })
    }
}

pub(crate) fn protect(root: &Path, protected: &[PathBuf]) -> Result<(), ReadRefusal> {
    let root = std::fs::canonicalize(root).map_err(|_| ReadRefusal::WorkspaceUnavailable {})?;
    for path in protected {
        let path = resolve_missing(path)?;
        if root.starts_with(&path) || path.starts_with(&root) {
            return Err(failure(WorkspaceReadFailure::ProtectedPath));
        }
    }
    Ok(())
}

fn resolve_missing(path: &Path) -> Result<PathBuf, ReadRefusal> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut base) => {
                for part in suffix.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| failure(WorkspaceReadFailure::ProtectedPath))?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| failure(WorkspaceReadFailure::ProtectedPath))?;
            }
            Err(_) => return Err(failure(WorkspaceReadFailure::ProtectedPath)),
        }
    }
}
