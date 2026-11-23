//! The composition root's service handles, constructed once at CLI entry.
//!
//! `Services` is what a command receives instead of reaching back into the
//! environment: the resolved [`Home`], the shared product-record service, and
//! the one extra telemetry sink this
//! build attaches to every product state root. Commands borrow; nothing here
//! is re-derived per verb, per attach round, or per turn.

use std::sync::{Arc, OnceLock};

use crate::home::paths::PlakPaths;

use crate::CliResult;
use crate::shell::home::Home;

/// Handles every product command shares for the life of the process.
pub(crate) struct Services {
    terminals: Arc<OnceLock<Arc<crate::terminal_registry::TerminalRegistry>>>,
    upgrade_placement: Option<UpgradePlacement>,
    runner_ownership: Option<Arc<crate::commands::runner::ownership::RunnerOwnership>>,
    runners: plakous::runner::RunnerRegistry,
    runner_overlap: Arc<OnceLock<plakous::runner::MachineOverlapGuard>>,
    local_daemon: Arc<
        std::sync::RwLock<
            Option<(
                plakous::tools::runner::LocalDaemonBackend,
                plakous::runner::RunnerClient,
            )>,
        >,
    >,
    clock: Arc<dyn plakous::model::Clock>,
    home: Home,
    session_records: OnceLock<Arc<crate::store::session_records::SqliteSessionStore>>,
    /// The process's one job bound, shared by every state root it opens.
    jobs: plakous_hosted::agent::ProcessJobs,
    /// Credential custody, latched by the first resolution and shared by
    /// every clone: a host resolves it at startup and never re-reads it.
    custody: Arc<OnceLock<crate::gateway::HostCustody>>,
    runtime_store_bounds: Arc<OnceLock<[std::num::NonZeroUsize; 4]>>,
    retire_finished_children: Arc<OnceLock<bool>>,
}

#[derive(Clone)]
struct UpgradePlacement {
    id: String,
    workspace: std::path::PathBuf,
    runner: plakous::runner::RunnerServices,
    client: Option<plakous::runner::RunnerClient>,
}

impl Services {
    pub(crate) fn with_store_probe(mut self, root: &std::path::Path) -> CliResult<Self> {
        let directory = crate::store::revisions::probe_stores(self.paths()?, root)?;
        self.home.use_probe_stores(directory)?;
        self.session_records = OnceLock::new();
        Ok(self)
    }

    /// Scoped only to qualification or host-owned dormant creation, never
    /// driver admission or publication of registry Ready.
    pub(crate) fn with_upgrade_runner(
        &self,
        id: &str,
        workspace: &std::path::Path,
        runner: plakous::runner::RunnerServices,
        client: Option<plakous::runner::RunnerClient>,
    ) -> Self {
        let mut services = self.clone();
        services.upgrade_placement = Some(UpgradePlacement {
            id: id.to_owned(),
            workspace: workspace.to_owned(),
            runner,
            client,
        });
        services
    }

    /// Composes standalone import runners on a process-owned executor.
    /// Refuses while any host owns the home, before any runner bind.
    #[allow(
        dead_code,
        reason = "standalone import is integrated by its product boundary"
    )]
    pub(crate) fn with_standalone_runners(
        &self,
        executor: &tokio::runtime::Handle,
    ) -> Result<Self, crate::commands::runner::services::StandaloneRunnerError> {
        crate::commands::runner::services::standalone(self, executor)
    }

    pub(crate) fn runner_ownership(
        &self,
    ) -> Option<&crate::commands::runner::ownership::RunnerOwnership> {
        self.runner_ownership.as_deref()
    }

    pub(crate) fn with_runner_ownership(
        &self,
        ownership: Arc<crate::commands::runner::ownership::RunnerOwnership>,
    ) -> Self {
        let mut services = self.clone();
        services.runner_ownership = Some(ownership);
        services
    }
    pub(crate) fn runner_probe_context(&self) -> Self {
        let mut services = self.clone();
        services.runners = plakous::runner::RunnerRegistry::default();
        services
    }

    pub(crate) fn with_runner_registry(&self, registry: plakous::runner::RunnerRegistry) -> Self {
        let mut services = self.clone();
        services.runners = registry;
        services
    }
    #[cfg(test)]
    pub(crate) fn for_home(paths: PlakPaths) -> Self {
        let state = paths.home().join("test-state");
        Self::for_home_with_state(paths, state)
            .with_test_custody(crate::gateway::HostCustody::Local)
    }

    #[cfg(test)]
    pub(crate) fn for_home_with_state(paths: PlakPaths, state: std::path::PathBuf) -> Self {
        Self {
            terminals: Arc::default(),
            runner_ownership: None,
            upgrade_placement: None,
            runners: plakous::runner::RunnerRegistry::default(),
            runner_overlap: Arc::default(),
            local_daemon: Arc::default(),
            clock: crate::commands::application::test_clock(),
            home: Home::explicit(paths, state),
            session_records: OnceLock::new(),
            jobs: plakous_hosted::agent::ProcessJobs::default(),
            custody: Arc::default(),
            runtime_store_bounds: Arc::default(),
            retire_finished_children: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_custody(mut self, custody: crate::gateway::HostCustody) -> Self {
        self.custody = Arc::new(OnceLock::from(custody));
        self
    }

    #[cfg(test)]
    pub(crate) fn product_store_opens(&self) -> usize {
        self.session_records
            .get()
            .map_or(0, |store| store.opened_connections())
    }

    /// Resolves the product home once.
    pub(crate) fn from_env(launch: Option<&std::path::Path>) -> Self {
        let home = Home::from_env(launch);
        if let Some(paths) = home.paths_if_resolved() {
            crate::build_identity::install_identity_cache_home(paths.home().to_path_buf());
        }
        if let Ok(state) = home.state() {
            crate::product_doctor::install_credential_state_home(state.to_path_buf());
        }
        Self {
            terminals: Arc::default(),
            runner_ownership: None,
            upgrade_placement: None,
            runners: plakous::runner::RunnerRegistry::default(),
            runner_overlap: Arc::default(),
            local_daemon: Arc::default(),
            clock: Arc::new(plakous::model::SystemClock),
            home,
            session_records: OnceLock::new(),
            jobs: plakous_hosted::agent::ProcessJobs::default(),
            custody: Arc::default(),
            runtime_store_bounds: Arc::default(),
            retire_finished_children: Arc::default(),
        }
    }

    pub(crate) const fn home(&self) -> &Home {
        &self.home
    }
}

/// These services for another thread of the process: the same home,
/// product store, telemetry sink and job bound, so a session it opens is one
/// this process's own commands would open.
impl Clone for Services {
    fn clone(&self) -> Self {
        let session_records = OnceLock::new();
        if let Some(store) = self.session_records.get() {
            let _ = session_records.set(Arc::clone(store));
        }
        Self {
            terminals: self.terminals.clone(),
            runner_ownership: self.runner_ownership.clone(),
            upgrade_placement: self.upgrade_placement.clone(),
            runners: self.runners.clone(),
            runner_overlap: self.runner_overlap.clone(),
            local_daemon: self.local_daemon.clone(),
            clock: Arc::clone(&self.clock),
            home: self.home.clone(),
            session_records,
            jobs: self.jobs.clone(),
            custody: Arc::clone(&self.custody),
            runtime_store_bounds: Arc::clone(&self.runtime_store_bounds),
            retire_finished_children: Arc::clone(&self.retire_finished_children),
        }
    }
}

impl Services {
    pub(crate) fn with_terminal_registry(
        &self,
        registry: Arc<crate::terminal_registry::TerminalRegistry>,
    ) -> Self {
        let services = self.clone();
        let _ = services.terminals.set(registry);
        services
    }

    pub(crate) fn terminal_registry(
        &self,
    ) -> Option<&Arc<crate::terminal_registry::TerminalRegistry>> {
        self.terminals.get()
    }
    pub(crate) const fn runners(&self) -> &plakous::runner::RunnerRegistry {
        &self.runners
    }

    pub(crate) fn with_runner_overlap(&self, guard: plakous::runner::MachineOverlapGuard) -> Self {
        let mut services = self.clone();
        let selected = OnceLock::new();
        let _ = selected.set(guard);
        services.runner_overlap = Arc::new(selected);
        services
    }

    pub(crate) fn runner_overlap(&self) -> &plakous::runner::MachineOverlapGuard {
        self.runner_overlap.get_or_init(|| {
            let Ok(paths) = self.paths() else {
                return plakous::runner::MachineOverlapGuard::new(None, []);
            };
            crate::commands::runner::registry::overlap_guard(paths).unwrap_or_else(|_| {
                plakous::runner::MachineOverlapGuard::new(None, []).with_admission_store(
                    paths.home().join("runners/admission.lock"),
                    || {
                        Err(plakous::runner::PlacementFailure::RunnerUnreachable {
                            phase: plakous::runner::ProbePhase::Reconcile,
                            timed_out: false,
                        })
                    },
                )
            })
        })
    }

    pub(crate) fn state_roots_on(
        &self,
        context: &PlakPaths,
        runner_id: &str,
    ) -> CliResult<plakous_hosted::agent::ProductStateRoots> {
        let roots = self.state_roots_for(context)?;
        if let Some(placement) = &self.upgrade_placement
            && placement.id == runner_id
        {
            if context.workspace().map_err(crate::CliFailure::new)? != placement.workspace {
                return Err(crate::CliFailure::new(
                    "upgrade placement services belong to a different workspace",
                ));
            }
            let roots = roots.with_runner_services(placement.runner.clone());
            return Ok(match &placement.client {
                Some(client) => roots.with_runner_client(client.clone()),
                None => roots,
            });
        }
        if runner_id == "local" && self.runners.rows().is_empty() {
            return Ok(roots);
        }
        let roots = match self.runners.services(runner_id).map_err(|failure| {
            crate::CliFailure::new(format!("runner placement refused: {failure:?}"))
        })? {
            Some(services) => {
                let roots = roots.with_runner_services(services);
                match self.runners.client(runner_id) {
                    Some(client) => roots.with_runner_client(client),
                    None => roots,
                }
            }
            None => roots,
        };
        Ok(roots.with_runner_readiness(self.runners.clone(), runner_id))
    }
    #[allow(dead_code, reason = "selected only by the optional hosting product")]
    pub(crate) fn with_local_daemon(
        &self,
        backend: plakous::tools::runner::LocalDaemonBackend,
        client: plakous::runner::RunnerClient,
    ) -> Self {
        let services = self.clone();
        *services
            .local_daemon
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((backend, client));
        services
    }

    pub(crate) fn local_runner_client(&self) -> Option<plakous::runner::RunnerClient> {
        self.local_daemon
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(_, client)| client.clone())
    }
    pub(crate) fn now_unix_seconds(&self) -> u64 {
        self.clock.now().seconds()
    }

    /// The product home layout, or the one resolution failure.
    pub(crate) fn paths(&self) -> CliResult<&PlakPaths> {
        self.home.paths()
    }

    pub(crate) fn session_records(
        &self,
    ) -> CliResult<Arc<dyn crate::store::session_records::SessionRecordStore>> {
        let paths = self.paths()?;
        Ok(self
            .session_records
            .get_or_init(|| {
                Arc::new(crate::store::session_records::SqliteSessionStore::new(
                    paths.clone(),
                ))
            })
            .clone())
    }

    /// Product state roots of the launch directory's context.
    pub(crate) fn state_roots(&self) -> CliResult<plakous_hosted::agent::ProductStateRoots> {
        self.state_roots_for(self.paths()?)
    }

    /// Product state roots for a session of `context`, its workspace's
    /// product context: its roster reads that context's configuration. A
    /// session's host builds them once and keeps them in its host config.
    pub(crate) fn state_roots_for(
        &self,
        context: &PlakPaths,
    ) -> CliResult<plakous_hosted::agent::ProductStateRoots> {
        let roots = crate::commands::support::product_state_roots_from(
            context,
            self.home.state()?.to_path_buf(),
            self.session_records()?,
            &self.jobs,
            &self.custody()?,
        )?
        .with_runtime_store_bounds(self.runtime_store_bounds()?)
        .with_finished_child_retirement(self.retire_finished_children()?);
        let roots = match &*self
            .local_daemon
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Some((backend, client)) => roots.with_local_daemon(backend.clone(), client.clone()),
            None => roots,
        };
        let roots = match crate::test_hooks::runner_services() {
            Some(services) => roots.with_runner_services(services),
            None if !self.runners.rows().is_empty() => {
                roots.with_runner_readiness(self.runners.clone(), "local")
            }
            None => roots,
        };
        let fallback = Arc::new(crate::terminal_registry::RunnerFence::new(
            context.clone(),
            self.local_runner_client(),
        ));
        if let Some(registry) = self.terminal_registry() {
            return Ok(roots.with_reclamation_fence(registry.clone()));
        }
        Ok(roots.with_reclamation_fence(fallback))
    }

    /// This process's credential custody. The first call reads the home's
    /// `config.toml`; every later call, on this value or any clone, returns
    /// that same custody. A failed read is an error and latches nothing.
    pub(crate) fn custody(&self) -> CliResult<crate::gateway::HostCustody> {
        if let Some(custody) = self.custody.get() {
            return Ok(custody.clone());
        }
        let home = self.paths()?;
        let config = crate::product_config::ProductConfig::load_home_file(home)
            .map_err(crate::CliFailure::new)?;
        let resolved = crate::gateway::HostCustody::from_config(home, &config)?;
        self.runtime_store_bounds
            .get_or_init(|| config.runtime_store_bounds);
        Ok(self.custody.get_or_init(|| resolved).clone())
    }

    pub(crate) fn runtime_store_bounds(&self) -> CliResult<[std::num::NonZeroUsize; 4]> {
        if let Some(bounds) = self.runtime_store_bounds.get() {
            return Ok(*bounds);
        }
        let bounds = crate::product_config::ProductConfig::load_home_file(self.paths()?)
            .map_err(crate::CliFailure::new)?
            .runtime_store_bounds;
        Ok(*self.runtime_store_bounds.get_or_init(|| bounds))
    }

    fn retire_finished_children(&self) -> CliResult<bool> {
        if let Some(enabled) = self.retire_finished_children.get() {
            return Ok(*enabled);
        }
        let enabled = crate::product_config::ProductConfig::load_home_file(self.paths()?)
            .map_err(crate::CliFailure::new)?
            .retire_finished_children;
        Ok(*self.retire_finished_children.get_or_init(|| enabled))
    }

    /// The process's one job bound.
    pub(crate) const fn process_jobs(&self) -> &plakous_hosted::agent::ProcessJobs {
        &self.jobs
    }
}
