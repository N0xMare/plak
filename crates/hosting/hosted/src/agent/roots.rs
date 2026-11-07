//! The process-lifetime state roots one hosted session shares: the runtime
//! session store, the topology tree store, and the telemetry it emits
//! through.

mod local_runner;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use plakous::sessions::{AgentSessions, SessionBackend};

use crate::agent_tree::TreeStore;
use crate::{CliFailure, CliResult};

#[cfg(feature = "js")]
mod recorded_sidecar;
#[cfg(not(feature = "js"))]
#[path = "roots/recorded_sidecar_unavailable.rs"]
mod recorded_sidecar;
#[cfg(all(test, feature = "js"))]
use recorded_sidecar::RecordedSidecar;

/// Where a hosted session's agent tree is persisted: the product opens its
/// store (`subagents.sqlite3` writer) and the roots hold the port,
/// caching one opening per process.
///
/// The twin of [`TelemetrySinkFactory`], and for the same reason: the roots
/// know that there is one store per process, not where it is or what it is
/// made of.
pub trait TreeStoreFactory: Send + Sync {
    /// Open the tree store once; the roots keep the result.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the store cannot be opened.
    fn open(&self) -> CliResult<Arc<dyn TreeStore>>;
}

/// The product policy one tree epoch freezes, and the gate that admits a fresh
/// one.
///
/// `plakous-recursive-subagents` freezes a `RosterInput` and reads no
/// configuration; turning the product's `config.toml` into one is the
/// product's job, and so is answering "may a fresh root open a tree *now*".
/// Both are asked here so nothing under `agent/**` names the schema.
pub trait RosterSource: Send + Sync {
    /// The epoch a genesis freezes for the root bind it is given.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the product policy cannot be read
    /// — never the schema defaults, which would freeze an empty roster as if
    /// the operator had configured one.
    fn roster(
        &self,
        root_account: &str,
        root_tuple: &str,
        root_effort: Option<&str>,
    ) -> CliResult<crate::orchestration::RosterInput>;

    /// The product's own spelling of one provider's account.
    ///
    /// `grok` for xAI, `claude` for Anthropic: the roster is written in these
    /// names, so the source that freezes a roster is the one that knows them.
    fn account_name(&self, provider: plakous::subscriptions::NativeProvider) -> &'static str;

    /// Whether a fresh root may open a topology hub *now*.
    ///
    /// Re-asked on every recompile, so a change made from another shell is in
    /// force for the next one; an unreadable policy refuses creation.
    fn topology_admitted(&self) -> CliResult<bool>;
}

#[must_use]
/// Where a hosted session's telemetry goes: the product opens its sink (the
/// SQLite writer, fanned out with any extra sink) and answers for its health.
/// The roots hold the port and cache one opening per process.
pub trait TelemetrySinkFactory: Send + Sync {
    /// Open the telemetry sink once; the roots keep the result.
    fn open(&self, extra_sink: Option<Arc<dyn ExtraTelemetrySink>>) -> Arc<dyn ProductTelemetry>;
    /// Record that telemetry is unavailable for `session_id`.
    fn log_unavailable(&self, session_id: &str, reason: &str);
}

/// An optional exporter of both agent diagnostics and product observations.
/// Product usage is carried above the facade, never through the agent sink.
pub trait ExtraTelemetrySink: plakous::agent::TelemetrySink {
    /// Nonblocking admission of one disposable product observation.
    ///
    /// # Errors
    /// A full or closed exporter queue.
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "Copy product observation"
    )]
    fn try_emit_product(
        &self,
        observation: crate::product_observation::ProductObservation,
    ) -> Result<(), plakous::agent::TelemetrySinkError>;
}

/// One process's opened product telemetry, as a hosted session uses it.
///
/// The session says what happened and asks whether anything is recording; the
/// writer, the database file and the fan-out are the product's business, so
/// nothing above this line names a `TelemetryWriter` or a telemetry store.
pub trait ProductTelemetry: Send + Sync {
    /// The agent-side emitter a compiled runtime publishes through, when one
    /// opened.
    fn agent_emitter(&self) -> Option<Arc<plakous::agent::TelemetryEmitter>>;

    /// The observer the topology hub publishes node transitions through.
    fn topology_observer(&self) -> Option<Arc<dyn crate::agent_tree::TopologyObserver>>;

    /// Record one product observation. A sink that failed to open drops it.
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "Copy product observation enum"
    )]
    fn emit(&self, observation: crate::product_observation::ProductObservation);

    /// Doctor-facing health of this opening.
    fn health(&self) -> TelemetryHealth;

    /// Why the sink is unavailable, when it is.
    fn unavailable_reason(&self) -> Option<String>;
}

/// Doctor-facing telemetry health.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TelemetryHealth {
    Missing,
    Unavailable(String),
    Ok {
        rows: u64,
        write_errors: u64,
        dropped: u64,
    },
}

#[cfg(any(feature = "openai", feature = "anthropic", feature = "xai-api"))]
mod api;
#[cfg(not(any(feature = "openai", feature = "anthropic", feature = "xai-api")))]
#[path = "roots/api_unavailable.rs"]
mod api;

/// What a host process's sessions share of their jobs: the one bound on
/// running process starts (128 for the process). Each session's state roots
/// keep a job registry of their own, so one session's release, counts and
/// `/processes` never reach another's jobs.
#[derive(Clone, Default)]
pub struct ProcessJobs(Arc<plakous::tools::JobHostCapacity>);

#[derive(Clone)]
pub struct ProductStateRoots {
    runner_services: Option<plakous::runner::RunnerServices>,
    runner_readiness: Option<Arc<(plakous::runner::RunnerRegistry, String)>>,
    local_daemon: Option<plakous::tools::runner::LocalDaemonBackend>,
    daemon_client: Option<plakous::runner::RunnerClient>,
    child_runtime: Arc<dyn super::children::runtime_factory::ChildRuntimeFactory>,
    job_registry: Arc<crate::jobs::JobRegistry>,
    job_config: plakous::tools::JobConfig,
    skills: crate::skills::SkillsSetting,
    /// What an exact reopen read back about the session's skills; `None`
    /// for a new construction, which follows `skills`.
    frozen_skills: Option<crate::skills::FrozenSkills>,
    /// A rebind or switch successor's skills on/off, inherited from its
    /// source; `None` outside one.
    inherited_skills: Option<bool>,
    /// The successor epoch a profile switch's builds compile against: the
    /// root's tools see the tree with this epoch appended, not its current
    /// one. `None` outside a switch.
    pending_epoch: Option<Arc<crate::orchestration::OrchestrationSnapshot>>,
    /// Skill notices new constructions raised, drained by the host loop.
    skill_notices: Arc<Mutex<Vec<String>>>,
    exchange_observer: crate::exchange_observation::Observer,
    session_records: Arc<dyn crate::session_store::SessionRecordStore>,
    product: PathBuf,
    /// The runtime database the product bound at construction. It is an
    /// explicit root, never derived from `product`: the hosting boundary
    /// names where the semantic store lives.
    runtime_database: PathBuf,
    /// The XDG state root the product bound, kept beside the layout root the
    /// paths below name: `SubscriptionPaths` derives the whole Plak layout
    /// from this one directory, so a hosted caller that needs the native
    /// subscription service asks here instead of re-reading the environment.
    state_home: PathBuf,
    subscriptions: PathBuf,
    // Clones are process-lifetime handles: every attach and workflow uses the
    // same bounded SQLite actor instead of reopening its database.
    runtime_sessions: Arc<Mutex<Option<Arc<AgentSessions>>>>,
    /// This process's last reading of the shared runtime store's fill
    /// ([`Self::read_store_fill`]), for the session facts.
    store_fill: Arc<Mutex<Option<crate::storage_pressure::StoreFill>>>,
    /// The shutdown of the attachment these roots were handed to. Every
    /// runtime-store handle and tree they open waits on it, and on nothing
    /// another attachment bound.
    storage_shutdown: Option<crate::agent_tree_tools::HostShutdown>,
    /// The topology store the hub publishes trees through: opened once per
    /// process, on the first root attach, and never per compile.
    tree_store: Arc<Mutex<Option<Arc<dyn TreeStore>>>>,
    tree_store_factory: Arc<dyn TreeStoreFactory>,
    roster_source: Arc<dyn RosterSource>,
    /// The OpenRouter credential the product binds: the
    /// hosted binder asks for a key source, never for the secret store under a
    /// product home.
    openrouter_credentials: Arc<dyn crate::openrouter::OpenRouterCredentials>,
    api: api::ApiState,
    /// The named secrets MCP server definitions refer to.
    named_secrets: Arc<dyn crate::mcp_binding::NamedSecrets>,
    /// Whether API model lists are fetched live, and the window selection a
    /// new API session binds (product `[api] full_window`).
    /// The semantic evidence an exact reopen was created with; `None` for a
    /// new session.
    created_semantic_evidence: Option<Arc<[u8]>>,
    sidecars: Arc<dyn plakous::tools::SidecarLocator>,
    reopen_sidecars: Option<Arc<dyn plakous::tools::SidecarLocator>>,
    telemetry_factory: Arc<dyn TelemetrySinkFactory>,
    telemetry: Arc<Mutex<Option<Arc<dyn ProductTelemetry>>>>,
    telemetry_unavailable_logged: Arc<Mutex<bool>>,
    extra_telemetry_sink: Arc<Mutex<Option<Arc<dyn ExtraTelemetrySink>>>>,
    /// The task set of the session these roots were handed to: the
    /// attachment's own background work spawns into it. Tree work uses the
    /// tree's sets instead.
    session_tasks: crate::TaskSet,
}

impl ProductStateRoots {
    /// These roots as one attachment hands them out: storage waits through
    /// them stop on that attachment's shutdown.
    pub(crate) fn with_storage_shutdown(
        mut self,
        shutdown: crate::agent_tree_tools::HostShutdown,
    ) -> Self {
        self.storage_shutdown = Some(shutdown);
        self
    }

    /// The shutdown of the attachment these roots belong to.
    pub(crate) fn host_shutdown(&self) -> Option<crate::agent_tree_tools::HostShutdown> {
        self.storage_shutdown.clone()
    }

    /// These roots as one session hands them out: what they spawn belongs to
    /// `tasks`.
    pub(crate) fn with_session_tasks(mut self, tasks: crate::TaskSet) -> Self {
        self.session_tasks = tasks;
        self
    }

    /// The task set of the session these roots belong to.
    pub(crate) fn session_tasks(&self) -> &crate::TaskSet {
        &self.session_tasks
    }

    pub(crate) fn with_child_runtime_factory(
        mut self,
        factory: Arc<dyn super::children::runtime_factory::ChildRuntimeFactory>,
    ) -> Self {
        self.child_runtime = factory;
        self
    }

    pub(crate) fn child_runtime_factory(
        &self,
    ) -> Arc<dyn super::children::runtime_factory::ChildRuntimeFactory> {
        self.child_runtime.clone()
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Takes the three absolute state roots and each injected product seam (telemetry, tree store, roster, sidecars, OpenRouter and platform credentials, session records) as separate trait objects so tests can replace each one."
    )]
    pub fn new(
        product: PathBuf,
        runtime_database: PathBuf,
        state_home: PathBuf,
        telemetry: Arc<dyn TelemetrySinkFactory>,
        tree_store: Arc<dyn TreeStoreFactory>,
        roster: Arc<dyn RosterSource>,
        sidecars: Arc<dyn plakous::tools::SidecarLocator>,
        openrouter_credentials: Arc<dyn crate::openrouter::OpenRouterCredentials>,
        platform_credentials: Arc<dyn crate::platform::PlatformCredentials>,
        session_records: Arc<dyn crate::session_store::SessionRecordStore>,
    ) -> CliResult<Self> {
        if !product.is_absolute() || !runtime_database.is_absolute() || !state_home.is_absolute() {
            return Err(CliFailure::new("product state roots must be absolute"));
        }
        let subscriptions = state_home.join("plak/subscriptions");
        Ok(Self {
            child_runtime: Arc::new(
                super::children::runtime_factory::ProductionChildRuntimeFactory,
            ),
            job_registry: Arc::default(),
            job_config: plakous::tools::JobConfig::default(),
            skills: crate::skills::SkillsSetting::default(),
            frozen_skills: None,
            inherited_skills: None,
            pending_epoch: None,
            skill_notices: Arc::default(),
            exchange_observer: None,
            session_records,
            product: std::fs::canonicalize(&product).unwrap_or(product),
            runtime_database: std::fs::canonicalize(&runtime_database).unwrap_or(runtime_database),
            state_home,
            subscriptions: std::fs::canonicalize(&subscriptions).unwrap_or(subscriptions),
            local_daemon: None,
            runner_services: None,
            runner_readiness: None,
            daemon_client: None,
            runtime_sessions: Arc::new(Mutex::new(None)),
            store_fill: Arc::default(),
            storage_shutdown: None,
            tree_store: Arc::new(Mutex::new(None)),
            tree_store_factory: tree_store,
            roster_source: roster,
            openrouter_credentials,
            api: api::ApiState::new(platform_credentials),
            named_secrets: Arc::new(crate::mcp_binding::NoNamedSecrets),
            created_semantic_evidence: None,
            sidecars,
            reopen_sidecars: None,
            telemetry_factory: telemetry,
            telemetry: Arc::new(Mutex::new(None)),
            telemetry_unavailable_logged: Arc::new(Mutex::new(false)),
            extra_telemetry_sink: Arc::new(Mutex::new(None)),
            session_tasks: crate::TaskSet::new(),
        })
    }

    /// Binds the product's process budgets for the runtimes built from these roots.
    #[must_use]
    pub fn with_job_config(mut self, config: plakous::tools::JobConfig) -> Self {
        self.job_config = config;
        self
    }

    /// Binds how API routes read their model lists and which window a new API
    /// session is admitted under. An exact reopen keeps the window it was
    /// created with.
    #[must_use]
    pub fn with_api_models(
        mut self,
        lists: crate::platform::ApiModelLists,
        window: crate::platform::ApiWindow,
    ) -> Self {
        self.api.lists = lists;
        self.api.window = window;
        self
    }

    pub(crate) fn platform_admission(&self) -> crate::platform::PlatformAdmission {
        crate::platform::PlatformAdmission {
            lists: self.api.lists,
            window: self.api.window,
            created_evidence: self
                .created_semantic_evidence
                .as_deref()
                .map(<[u8]>::to_vec),
        }
    }

    /// These roots' own job registry, under `jobs`, the process's bound on
    /// running process starts.
    #[must_use]
    pub fn with_process_jobs(mut self, jobs: &ProcessJobs) -> Self {
        self.job_registry = Arc::new(crate::jobs::JobRegistry::under(Arc::clone(&jobs.0)));
        self
    }

    /// Binds the serving host's local process transport to every node.
    #[must_use]
    pub fn with_local_daemon(
        mut self,
        backend: plakous::tools::runner::LocalDaemonBackend,
        client: plakous::runner::RunnerClient,
    ) -> Self {
        self.local_daemon = Some(backend);
        self.runner_services = None;
        self.runner_readiness = None;
        self.daemon_client = Some(client);
        self
    }

    pub(crate) async fn finish_child_process_stops(&self, tree_id: &str) -> CliResult<()> {
        let Some(client) = &self.daemon_client else {
            return Ok(());
        };
        let stops = self
            .tree_store()?
            .load(tree_id)
            .map_err(|error| CliFailure::new(error.detail))?
            .map_or_else(Vec::new, |tree| {
                let rows = tree.rows();
                rows.recorded_process_stops()
                    .into_iter()
                    .map(|(locator, intent)| {
                        let agent = rows
                            .nodes()
                            .values()
                            .find(|node| node.session_locator.as_deref() == Some(locator.as_str()))
                            .map(|node| node.agent_id.clone());
                        (agent, locator, intent)
                    })
                    .collect()
            });
        let sessions = self.runtime_sessions()?;
        let mut failures: Vec<String> = Vec::new();
        for (agent, locator, intent) in stops {
            let result = async {
                if let Some(agent) = agent
                    && let Some(runtime) = crate::topology_runtime::lookup(tree_id)
                    && runtime.is_recovering(&agent)
                {
                    return Err(CliFailure::new(
                        "child recovery still owns the process-stop line",
                    ));
                }
                let locator = locator
                    .parse::<plakous::sessions::SessionLocator>()
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                let head = sessions
                    .line_head_of(locator)
                    .await
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                let stopped = crate::runner_adoption::finish_recorded_stop(
                    client,
                    &sessions,
                    head.line(),
                    &intent,
                )
                .await
                .map_err(CliFailure::new)?;
                self.job_registry.retire_stopped_line(stopped.owner);
                Ok::<_, CliFailure>(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(target: "plak.runner", phase = "child_stop", locator = %locator, error = %error, "runner_recovery_failed");
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CliFailure::new(failures.join("; ")))
        }
    }

    pub(crate) async fn finish_dormant_root_stop(
        &self,
        head: &plakous::sessions::LeaseOnlyHead,
        intent: &str,
    ) -> CliResult<()> {
        if let Some(client) = &self.daemon_client {
            crate::runner_adoption::readopt_held(client, head, Some(intent))
                .await
                .map_err(CliFailure::new)?;
        }
        Ok(())
    }

    /// Applies this host's transport without changing a session's binding.
    #[must_use]
    pub fn bind_process_transport(
        &self,
        runtime: plakous::sessions::SessionRuntimeBinding,
    ) -> plakous::sessions::SessionRuntimeBinding {
        if let Some(services) = &self.runner_services {
            return runtime.with_runner_services(services.clone());
        }
        match &self.local_daemon {
            Some(backend) => runtime.with_local_daemon(backend.clone()),
            None => runtime,
        }
    }

    /// Pins this node's runner services, rather than selecting a process-wide
    /// backend. Local process transport is not applied to a nonlocal node.
    #[must_use]
    pub fn with_runner_services(mut self, services: plakous::runner::RunnerServices) -> Self {
        self.runner_services = Some(services);
        self.runner_readiness = None;
        self.local_daemon = None;
        self.daemon_client = None;
        self
    }

    /// Explicit services for this node, absent for the local runner.
    #[must_use]
    pub const fn runner_services(&self) -> Option<&plakous::runner::RunnerServices> {
        self.runner_services.as_ref()
    }

    /// Supplies the shared client's reconciliation/control port separately
    /// from the facade's ordinary new-call services.
    #[must_use]
    pub fn with_runner_client(mut self, client: plakous::runner::RunnerClient) -> Self {
        self.daemon_client = Some(client);
        self
    }

    /// A durable stop must remain pending while its runner barrier/link is
    /// unavailable; this is not evidence that any remote process has ended.
    #[must_use]
    pub fn runner_pending(&self) -> bool {
        self.runner_readiness
            .as_deref()
            .is_some_and(|(registry, id)| {
                !registry.rows().iter().any(|row| {
                    row.runner.runner_id == *id
                        && row.status == plakous::runner::RunnerStatus::Ready
                })
            })
            || self
                .runner_services
                .as_ref()
                .is_some_and(|services| !services.is_available())
            || self
                .daemon_client
                .as_ref()
                .is_some_and(|client| !client.is_connected())
    }

    /// Retains the selected placement's live first-bind/reconnect barrier.
    /// A connected local daemon is not proof its recorded stops reconciled.
    #[must_use]
    pub fn with_runner_readiness(
        mut self,
        registry: plakous::runner::RunnerRegistry,
        runner_id: impl Into<String>,
    ) -> Self {
        self.runner_readiness = Some(Arc::new((registry, runner_id.into())));
        self
    }

    /// Workspace authority of this node; no process-global runner lookup.
    #[must_use]
    pub fn workspace_probe(&self) -> Arc<dyn plakous::tools::WorkspaceProbe> {
        self.runner_services.as_ref().map_or_else(
            || {
                Arc::new(plakous::tools::LocalWorkspaceProbe)
                    as Arc<dyn plakous::tools::WorkspaceProbe>
            },
            plakous::runner::RunnerServices::workspace_probe,
        )
    }

    pub(crate) fn daemon_processes(&self) -> bool {
        self.local_daemon.is_some()
    }

    /// Whether these roots' registry is under `jobs`, the process's bound.
    #[must_use]
    pub fn shares_jobs(&self, jobs: &ProcessJobs) -> bool {
        self.job_registry.is_under(&jobs.0)
    }

    /// Binds the private directory promoted jobs spool their output files under.
    #[must_use]
    pub fn with_job_output_root(self, root: PathBuf) -> Self {
        self.job_registry.set_output_root(root);
        self
    }

    /// Binds whether sessions built from these roots discover Agent Skills,
    /// and the home the user skills root is under.
    #[must_use]
    pub fn with_skills(mut self, skills: crate::skills::SkillsSetting) -> Self {
        self.skills = skills;
        self
    }

    pub(crate) const fn skills(&self) -> &crate::skills::SkillsSetting {
        &self.skills
    }

    /// Which skills authority a node built from these roots follows.
    pub(crate) fn skills_mode(&self) -> crate::skills::SkillsMode<'_> {
        match (&self.frozen_skills, self.inherited_skills) {
            (Some(frozen), _) => crate::skills::SkillsMode::Frozen(frozen),
            (None, Some(on)) => crate::skills::SkillsMode::Inherited(on),
            (None, None) => crate::skills::SkillsMode::New,
        }
    }

    /// Makes the builds that follow rebuild exactly a session's frozen skills
    /// (`None` ends that).
    pub(crate) fn set_frozen_skills(&mut self, frozen: Option<crate::skills::FrozenSkills>) {
        self.frozen_skills = frozen;
    }

    /// Marks the builds that follow as a fork, rebind or switch successor of a
    /// source whose skills are `on` (`None` ends that).
    pub(crate) fn set_inherited_skills(&mut self, on: Option<bool>) {
        self.inherited_skills = on;
    }

    /// Makes the builds that follow compile a switch successor against
    /// `epoch` appended to its tree (`None` ends that).
    pub(crate) fn set_pending_epoch(
        &mut self,
        epoch: Option<Arc<crate::orchestration::OrchestrationSnapshot>>,
    ) {
        self.pending_epoch = epoch;
    }

    /// The successor epoch the builds that follow compile against.
    pub(crate) fn pending_epoch(&self) -> Option<&crate::orchestration::OrchestrationSnapshot> {
        self.pending_epoch.as_deref()
    }

    /// Queues one skills notice a new construction raised.
    pub(crate) fn push_skill_notice(&self, notice: String) {
        self.skill_notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(notice);
    }

    /// The skills notices raised since the last call, oldest first.
    pub(crate) fn take_skill_notices(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .skill_notices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Reads back whether the session at `locator` has its skills on (its
    /// sealed registry holds `Skill`) and its skills listing, from one exact
    /// load, so its tools are rebuilt exactly as sealed.
    pub(crate) async fn with_frozen_skills(
        mut self,
        sessions: &AgentSessions,
        locator: plakous::sessions::SessionLocator,
    ) -> Result<Self, plakous::sessions::AgentSessionsError> {
        self.frozen_skills = Some(crate::skills::frozen_skills(sessions, locator).await?);
        Ok(self)
    }

    pub(crate) const fn job_config(&self) -> plakous::tools::JobConfig {
        self.job_config
    }

    pub(crate) fn job_registry(&self) -> &Arc<crate::jobs::JobRegistry> {
        &self.job_registry
    }

    /// Supplies the real process registry to injected offline child bindings.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn test_job_host(&self, agent_id: String) -> Arc<dyn plakous::tools::JobHost> {
        let tree = self
            .job_registry
            .current_tree()
            .unwrap_or_else(|| "root".to_owned());
        self.job_registry.for_agent(agent_id, tree)
    }

    /// Supplies an optional passive diagnostic observer from the product boundary.
    #[must_use]
    pub fn with_exchange_observer(
        mut self,
        observer: crate::exchange_observation::Observer,
    ) -> Self {
        self.exchange_observer = observer;
        self
    }

    pub(crate) fn exchange_observer(&self) -> crate::exchange_observation::Observer {
        self.exchange_observer.clone()
    }

    /// The runtime database every session of this home shares.
    pub(crate) fn runtime_database(&self) -> &std::path::Path {
        &self.runtime_database
    }

    /// The product record service shared by every command and attach root.
    pub fn session_records(&self) -> Arc<dyn crate::session_store::SessionRecordStore> {
        self.session_records.clone()
    }

    /// Attaches an additional typed sink fanned in beside the SQLite writer.
    pub fn set_extra_telemetry_sink(&self, sink: Arc<dyn ExtraTelemetrySink>) {
        *self
            .extra_telemetry_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    /// The product's native-subscription service, opened on the state root the
    /// product bound at construction.
    ///
    /// The spawn-a-child path needs one to admit the target account before it
    /// binds, and used to reach the product's own `application_state_home()` for
    /// it: an ad-hoc environment read from inside the agent builder. The fact
    /// is on the roots already.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the bound state root is not a
    /// usable Plak storage root.
    pub fn native_subscriptions(&self) -> CliResult<plakous::subscriptions::NativeSubscriptions> {
        let paths =
            plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(self.state_home.clone())
                .map_err(|_| CliFailure::new("native subscription storage is unavailable"))?;
        Ok(crate::exchange_observation::observe(
            plakous::subscriptions::NativeSubscriptions::new(paths),
            self.exchange_observer(),
        ))
    }

    /// Constructs only the selected account's native source, without fallback.
    ///
    /// # Errors
    /// Refuses API selections and invalid native source construction.
    pub fn native_subscriptions_for(
        &self,
        account: &crate::account::AccountRef,
    ) -> CliResult<plakous::subscriptions::NativeSubscriptions> {
        let provider = account
            .vertical()
            .capability()
            .native_provider()
            .ok_or_else(
                || crate::capabilities::AccountAdmissionError::AccountTargetMismatch {
                    account: account.to_string(),
                },
            )?;
        let subscriptions = self.native_subscriptions()?;
        if let Some(name) = account.name() {
            return subscriptions
                .for_named_account(provider, name.as_str())
                .map_err(|error| CliFailure::new(format!("{account}: {error}")));
        }
        Ok(subscriptions)
    }

    pub(super) fn paths(&self) -> [PathBuf; 2] {
        [self.product.clone(), self.subscriptions.clone()]
    }

    /// The process runtime-session service, opened once, as a handle of the
    /// attachment these roots belong to: its storage waits stop on that
    /// attachment's shutdown.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the runtime database cannot open.
    pub fn runtime_sessions(&self) -> CliResult<Arc<AgentSessions>> {
        let shared = self.shared_runtime_sessions()?;
        Ok(match &self.storage_shutdown {
            Some(shutdown) => {
                let shutdown = shutdown.clone();
                Arc::new(shared.with_storage_interrupt(Arc::new(move || shutdown.is_signalled())))
            }
            None => shared,
        })
    }

    /// Reads the shared runtime store's fill through the open backend (one
    /// usage read, no file inspection) and keeps it as this process's last
    /// reading. `None` when the store or its reading is unavailable: a usage
    /// read never refuses work on its own.
    pub(crate) async fn read_store_fill(&self) -> Option<crate::storage_pressure::StoreFill> {
        let sessions = self.runtime_sessions().ok()?;
        let fill = crate::storage_pressure::StoreFill::of(&sessions.store_usage().await.ok()?)?;
        *self
            .store_fill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fill);
        Some(fill)
    }

    /// The fill [`Self::read_store_fill`] last read in this process.
    pub(crate) fn last_store_fill(&self) -> Option<crate::storage_pressure::StoreFill> {
        *self
            .store_fill
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`Self::runtime_sessions`] for spawned children: their new work stops
    /// one store reserve before the root's, so a tree that fills the shared
    /// store leaves the root headroom to finish its turn, compact and clean up
    /// (spec 03 §10.1).
    pub(crate) fn child_runtime_sessions(&self) -> CliResult<Arc<AgentSessions>> {
        Ok(Arc::new(
            self.runtime_sessions()?.with_subordinate_admission(),
        ))
    }

    fn shared_runtime_sessions(&self) -> CliResult<Arc<AgentSessions>> {
        let _span = tracing::info_span!("runtime_sessions.open").entered();
        let mut retained = self
            .runtime_sessions
            .lock()
            .map_err(|_| CliFailure::new("plak runtime service ownership is unavailable"))?;
        if let Some(sessions) = retained.as_ref() {
            return Ok(Arc::clone(sessions));
        }
        let product_store = self.session_records.clone();
        let tree_store = self.tree_store.clone();
        let probe = Arc::new(move |retry| {
            if retry && product_store.storage_unavailable() {
                let _ = product_store.recover_storage();
            }
            let tree_store = tree_store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if retry
                && let Some(store) = &tree_store
                && store.storage_unavailable()
            {
                let _ = store.recover_storage();
            }
            product_store.storage_unavailable()
                || tree_store
                    .as_ref()
                    .is_some_and(|store| store.storage_unavailable())
        });
        let backend = SessionBackend::sqlite_with_storage_probe(&self.runtime_database, probe)
            .map_err(|error| {
                let raw = format!("plak runtime database could not be opened: {error}");
                CliFailure::new(
                    crate::storage_pressure::sessions_storage_pressure(&error, &raw).unwrap_or(raw),
                )
                .with_optional_remedy(crate::storage_pressure::sessions_remedy(&error).as_ref())
            })?;
        let sessions = Arc::new(AgentSessions::new(backend).map_err(|_| {
            CliFailure::new("plak durable session runtime could not be initialized")
        })?);
        *retained = Some(Arc::clone(&sessions));
        Ok(sessions)
    }

    /// The process's one [`TreeStore`] over the product home, opened on the
    /// first call and shared by every root attach after it.
    pub(crate) fn tree_store(&self) -> CliResult<Arc<dyn TreeStore>> {
        let mut retained = self
            .tree_store
            .lock()
            .map_err(|_| CliFailure::new("plak topology store ownership is unavailable"))?;
        if let Some(store) = retained.as_ref() {
            return Ok(Arc::clone(store));
        }
        let store = self.tree_store_factory.open()?;
        *retained = Some(Arc::clone(&store));
        Ok(store)
    }

    /// Where the product installs its evaluator workers.
    pub(crate) fn sidecars(&self) -> &Arc<dyn plakous::tools::SidecarLocator> {
        self.reopen_sidecars.as_ref().unwrap_or(&self.sidecars)
    }

    pub(super) fn without_reopen_artifact(mut self) -> Self {
        self.reopen_sidecars = None;
        self.created_semantic_evidence = None;
        self.frozen_skills = None;
        self.inherited_skills = None;
        self.pending_epoch = None;
        self
    }

    #[cfg(test)]
    pub(super) fn with_test_sidecars(
        mut self,
        sidecars: Arc<dyn plakous::tools::SidecarLocator>,
    ) -> Self {
        self.sidecars = sidecars;
        self
    }

    pub(super) async fn for_exact_reopen(
        mut self,
        sessions: &AgentSessions,
        locator: plakous::sessions::SessionLocator,
    ) -> Result<Self, plakous::sessions::AgentSessionsError> {
        self.job_config = sessions.created_job_config(locator).await?;
        self.created_semantic_evidence = Some(
            sessions
                .created_provider_semantic_evidence(locator)
                .await?
                .into(),
        );
        self.reopen_sidecars = recorded_sidecar::recorded_sidecars(sessions, locator).await?;
        self.with_frozen_skills(sessions, locator).await
    }

    /// Replaces the roster a genesis built from these roots freezes. A carry
    /// keeps its source's own profile this way rather than the home default.
    #[must_use]
    /// These roots over another tree store, for a test that needs the tree
    /// in a file another process can reach.
    #[cfg(test)]
    pub(crate) fn with_tree_store_factory(mut self, factory: Arc<dyn TreeStoreFactory>) -> Self {
        self.tree_store = Arc::new(Mutex::new(None));
        self.tree_store_factory = factory;
        self
    }

    pub fn with_roster_source(mut self, roster: Arc<dyn RosterSource>) -> Self {
        self.roster_source = roster;
        self
    }

    /// The product's roster and topology gate.
    pub(crate) fn roster_source(&self) -> &Arc<dyn RosterSource> {
        &self.roster_source
    }

    /// Binds the product's named-secret store; without one every secret an
    /// MCP server definition names is unset.
    #[must_use]
    pub fn with_named_secrets(
        mut self,
        secrets: Arc<dyn crate::mcp_binding::NamedSecrets>,
    ) -> Self {
        self.named_secrets = secrets;
        self
    }

    pub(crate) fn named_secrets(&self) -> Arc<dyn crate::mcp_binding::NamedSecrets> {
        Arc::clone(&self.named_secrets)
    }

    /// The OpenRouter credential this product bound.
    pub(crate) fn openrouter_credentials(
        &self,
    ) -> Arc<dyn crate::openrouter::OpenRouterCredentials> {
        Arc::clone(&self.openrouter_credentials)
    }

    /// State roots over in-process doubles of every product port.
    ///
    /// The hosted tests need roots that *work* -- a tree store with the
    /// conformance-tested memory implementation behind it, a roster that
    /// freezes, telemetry that records nothing -- without a product home, a
    /// SQLite file or a secret store behind them. The product's own roots come
    /// from `new`, and the product's test helper supplies its implementations.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_doubles(product: PathBuf, state_home: PathBuf) -> Self {
        Self {
            child_runtime: Arc::new(
                super::children::runtime_factory::ProductionChildRuntimeFactory,
            ),
            job_registry: Arc::default(),
            job_config: plakous::tools::JobConfig::default(),
            skills: crate::skills::SkillsSetting::default(),
            frozen_skills: None,
            inherited_skills: None,
            pending_epoch: None,
            skill_notices: Arc::default(),
            exchange_observer: None,
            session_records: Arc::new(crate::session_store::MemoryStore::new()),
            subscriptions: state_home.join("plak/subscriptions"),
            runtime_database: product.join("runtime.sqlite3"),
            product,
            state_home,
            local_daemon: None,
            runner_services: None,
            runner_readiness: None,
            daemon_client: None,
            runtime_sessions: Arc::new(Mutex::new(None)),
            store_fill: Arc::default(),
            storage_shutdown: None,
            tree_store: Arc::new(Mutex::new(None)),
            tree_store_factory: Arc::new(doubles::MemoryTreeStoreFactory),
            roster_source: Arc::new(doubles::UnpinnedRosterSource),
            openrouter_credentials: Arc::new(doubles::NoOpenRouterCredential),
            api: api::ApiState::new(Arc::new(crate::platform::UnavailableCredentials)),
            named_secrets: Arc::new(crate::mcp_binding::NoNamedSecrets),
            created_semantic_evidence: None,
            sidecars: Arc::new(doubles::NoSidecars),
            reopen_sidecars: None,
            telemetry_factory: Arc::new(doubles::NoTelemetry),
            telemetry: Arc::new(Mutex::new(None)),
            telemetry_unavailable_logged: Arc::new(Mutex::new(false)),
            extra_telemetry_sink: Arc::new(Mutex::new(None)),
            session_tasks: crate::TaskSet::new(),
        }
    }

    fn open_telemetry_cache(&self) -> Arc<dyn ProductTelemetry> {
        let mut retained = self
            .telemetry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cache) = retained.as_ref() {
            return Arc::clone(cache);
        }
        let extra_sink = self
            .extra_telemetry_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let cache = self.telemetry_factory.open(extra_sink);
        *retained = Some(Arc::clone(&cache));
        cache
    }

    #[must_use]
    pub fn telemetry_emitter(&self) -> Option<Arc<plakous::agent::TelemetryEmitter>> {
        self.open_telemetry_cache().agent_emitter()
    }

    /// Record one product observation, if this process has a telemetry sink.
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "Copy product observation enum"
    )]
    pub(crate) fn emit_product(&self, observation: crate::product_observation::ProductObservation) {
        self.open_telemetry_cache().emit(observation);
    }

    /// The topology observer this process publishes tree transitions through:
    /// the product's, behind the children's usage ledgers.
    pub(crate) fn topology_observer(&self) -> Option<Arc<dyn crate::agent_tree::TopologyObserver>> {
        Some(Arc::new(crate::node_usage::LedgerObserver(
            self.open_telemetry_cache().topology_observer(),
        )))
    }

    /// This process's opened telemetry, for the product's own emitters.
    pub fn product_telemetry(&self) -> Arc<dyn ProductTelemetry> {
        self.open_telemetry_cache()
    }

    pub fn telemetry_health(&self) -> TelemetryHealth {
        self.open_telemetry_cache().health()
    }

    pub fn log_telemetry_unavailable_once(&self, session_id: &str) {
        let Some(reason) = self.open_telemetry_cache().unavailable_reason() else {
            return;
        };
        let mut logged = self
            .telemetry_unavailable_logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *logged {
            return;
        }
        self.telemetry_factory.log_unavailable(session_id, &reason);
        *logged = true;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
pub(crate) mod tests;

/// In-process doubles of the product ports, for hosted tests.
#[cfg(any(test, feature = "test-support"))]
pub mod doubles;
