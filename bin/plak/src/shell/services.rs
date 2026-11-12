//! The composition root's service handles, constructed once at CLI entry.
//!
//! `Services` is what a command receives instead of reaching back into the
//! environment: the resolved [`Home`], the shared product-record service, and
//! the one extra telemetry sink this
//! build attaches to every product state root. Commands borrow; nothing here
//! is re-derived per verb, per attach round, or per turn.

use std::sync::{Arc, OnceLock};

use crate::home::paths::PlakPaths;
use plakous_hosted::agent::ExtraTelemetrySink;

use crate::CliResult;
use crate::shell::home::Home;

/// How this build makes its extra telemetry sink, when it has one.
///
/// A plain `fn` pointer, not a value: constructing the OTLP bridge opens a
/// socket and spawns an exporter thread, and most verbs (`--version`,
/// `help`, `config *`, `doctor`) never emit telemetry at all.
pub(crate) type TelemetrySinkFactory = fn() -> Option<Arc<dyn ExtraTelemetrySink>>;

/// Handles every product command shares for the life of the process.
pub(crate) struct Services {
    terminals: Arc<OnceLock<Arc<crate::terminal_registry::TerminalRegistry>>>,
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
    telemetry_sink_factory: Option<TelemetrySinkFactory>,
    extra_telemetry_sink: OnceLock<Option<Arc<dyn ExtraTelemetrySink>>>,
    /// The process's one job bound, shared by every state root it opens.
    jobs: plakous_hosted::agent::ProcessJobs,
}

impl Services {
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
    }

    #[cfg(test)]
    pub(crate) fn for_home_with_state(paths: PlakPaths, state: std::path::PathBuf) -> Self {
        Self {
            terminals: Arc::default(),
            runner_ownership: None,
            runners: plakous::runner::RunnerRegistry::default(),
            runner_overlap: Arc::default(),
            local_daemon: Arc::default(),
            clock: crate::commands::application::test_clock(),
            home: Home::explicit(paths, state),
            session_records: OnceLock::new(),
            telemetry_sink_factory: None,
            extra_telemetry_sink: OnceLock::new(),
            jobs: plakous_hosted::agent::ProcessJobs::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn product_store_opens(&self) -> usize {
        self.session_records
            .get()
            .map_or(0, |store| store.opened_connections())
    }

    /// Resolves the home once and captures how this build makes its extra
    /// telemetry sink.
    ///
    /// The composition root passes the sink explicitly to every state root.
    /// It is built on first use rather than at CLI
    /// entry, so a command that opens no state root pays nothing for it.
    pub(crate) fn from_env(
        telemetry_sink_factory: Option<TelemetrySinkFactory>,
        launch: Option<&std::path::Path>,
    ) -> Self {
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
            runners: plakous::runner::RunnerRegistry::default(),
            runner_overlap: Arc::default(),
            local_daemon: Arc::default(),
            clock: Arc::new(plakous::model::SystemClock),
            home,
            session_records: OnceLock::new(),
            telemetry_sink_factory,
            extra_telemetry_sink: OnceLock::new(),
            jobs: plakous_hosted::agent::ProcessJobs::default(),
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
        let extra_telemetry_sink = OnceLock::new();
        if let Some(sink) = self.extra_telemetry_sink.get() {
            let _ = extra_telemetry_sink.set(sink.clone());
        }
        Self {
            terminals: self.terminals.clone(),
            runner_ownership: self.runner_ownership.clone(),
            runners: self.runners.clone(),
            runner_overlap: self.runner_overlap.clone(),
            local_daemon: self.local_daemon.clone(),
            clock: Arc::clone(&self.clock),
            home: self.home.clone(),
            session_records,
            telemetry_sink_factory: self.telemetry_sink_factory,
            extra_telemetry_sink,
            jobs: self.jobs.clone(),
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

    /// The extra telemetry sink for a state root opened by this process,
    /// built on the first call and shared by every later one.
    pub(crate) fn extra_telemetry_sink(&self) -> Option<Arc<dyn ExtraTelemetrySink>> {
        self.extra_telemetry_sink
            .get_or_init(|| self.telemetry_sink_factory.and_then(|build| build()))
            .clone()
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

    /// Product state roots of the launch directory's context, with the
    /// extra sink attached.
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
            self.extra_telemetry_sink(),
            self.session_records()?,
            &self.jobs,
        )?;
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
        let fallback = Arc::new(crate::store::terminal_pins::StoredTerminalFence(
            context.clone(),
        ));
        if let Some(registry) = self.terminal_registry() {
            return Ok(roots.with_reclamation_fence(registry.clone()));
        }
        Ok(roots.with_reclamation_fence(fallback))
    }

    /// The process's one job bound.
    pub(crate) const fn process_jobs(&self) -> &plakous_hosted::agent::ProcessJobs {
        &self.jobs
    }
}
