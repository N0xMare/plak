//! The process-lifetime state roots one hosted session shares: the runtime
//! session store, the topology tree store, and the telemetry it emits
//! through.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use plakous::sessions::{AgentSessions, SessionBackend};

use crate::agent_tree::TreeStore;
use crate::{CliFailure, CliResult};

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

#[derive(Clone)]
pub struct ProductStateRoots {
    child_runtime: Arc<dyn super::children::runtime_factory::ChildRuntimeFactory>,
    job_registry: Arc<crate::jobs::JobRegistry>,
    job_config: plakous::tools::JobConfig,
    exchange_observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
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
    /// The topology store the hub publishes trees through: opened once per
    /// process, on the first root attach, and never per compile.
    tree_store: Arc<Mutex<Option<Arc<dyn TreeStore>>>>,
    tree_store_factory: Arc<dyn TreeStoreFactory>,
    roster_source: Arc<dyn RosterSource>,
    /// The OpenRouter credential the product binds: the
    /// hosted binder asks for a key source, never for the secret store under a
    /// product home.
    openrouter_credentials: Arc<dyn crate::openrouter::OpenRouterCredentials>,
    platform_credentials: Arc<dyn crate::platform::PlatformCredentials>,
    /// The named secrets MCP server definitions refer to.
    named_secrets: Arc<dyn crate::mcp_binding::NamedSecrets>,
    /// Whether API model lists are fetched live, and the window selection a
    /// new API session binds (product `[api] full_window`).
    api_model_lists: crate::platform::ApiModelLists,
    api_window: crate::platform::ApiWindow,
    /// The semantic evidence an exact reopen was created with; `None` for a
    /// new session.
    created_semantic_evidence: Option<Arc<[u8]>>,
    sidecars: Arc<dyn plakous::tools::SidecarLocator>,
    reopen_sidecars: Option<Arc<dyn plakous::tools::SidecarLocator>>,
    telemetry_factory: Arc<dyn TelemetrySinkFactory>,
    telemetry: Arc<Mutex<Option<Arc<dyn ProductTelemetry>>>>,
    telemetry_unavailable_logged: Arc<Mutex<bool>>,
    extra_telemetry_sink: Arc<Mutex<Option<Arc<dyn ExtraTelemetrySink>>>>,
}

impl ProductStateRoots {
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
            exchange_observer: None,
            session_records,
            product: std::fs::canonicalize(&product).unwrap_or(product),
            runtime_database: std::fs::canonicalize(&runtime_database).unwrap_or(runtime_database),
            state_home,
            subscriptions: std::fs::canonicalize(&subscriptions).unwrap_or(subscriptions),
            runtime_sessions: Arc::new(Mutex::new(None)),
            tree_store: Arc::new(Mutex::new(None)),
            tree_store_factory: tree_store,
            roster_source: roster,
            openrouter_credentials,
            platform_credentials,
            named_secrets: Arc::new(crate::mcp_binding::NoNamedSecrets),
            api_model_lists: crate::platform::ApiModelLists::Offline,
            api_window: crate::platform::ApiWindow::Working,
            created_semantic_evidence: None,
            sidecars,
            reopen_sidecars: None,
            telemetry_factory: telemetry,
            telemetry: Arc::new(Mutex::new(None)),
            telemetry_unavailable_logged: Arc::new(Mutex::new(false)),
            extra_telemetry_sink: Arc::new(Mutex::new(None)),
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
        self.api_model_lists = lists;
        self.api_window = window;
        self
    }

    pub(crate) fn platform_admission(&self) -> crate::platform::PlatformAdmission {
        crate::platform::PlatformAdmission {
            lists: self.api_model_lists,
            window: self.api_window,
            created_evidence: self
                .created_semantic_evidence
                .as_deref()
                .map(<[u8]>::to_vec),
        }
    }

    /// Binds the private directory promoted jobs spool their output files under.
    #[must_use]
    pub fn with_job_output_root(self, root: PathBuf) -> Self {
        self.job_registry.set_output_root(root);
        self
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
        observer: Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>>,
    ) -> Self {
        self.exchange_observer = observer;
        self
    }

    pub(crate) fn exchange_observer(
        &self,
    ) -> Option<Arc<dyn plakous::exchange_observation::ExchangeObserver>> {
        self.exchange_observer.clone()
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
        Ok(plakous::subscriptions::NativeSubscriptions::new(paths)
            .with_exchange_observer(self.exchange_observer()))
    }

    pub(super) fn paths(&self) -> [PathBuf; 2] {
        [self.product.clone(), self.subscriptions.clone()]
    }

    /// The process runtime-session handle, opened once.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe failure when the runtime database cannot open.
    pub fn runtime_sessions(&self) -> CliResult<Arc<AgentSessions>> {
        let _span = tracing::info_span!("runtime_sessions.open").entered();
        let mut retained = self
            .runtime_sessions
            .lock()
            .map_err(|_| CliFailure::new("plak runtime service ownership is unavailable"))?;
        if let Some(sessions) = retained.as_ref() {
            return Ok(Arc::clone(sessions));
        }
        let backend = SessionBackend::sqlite(&self.runtime_database).map_err(|error| {
            let raw = format!("plak runtime database could not be opened: {error}");
            CliFailure::new(
                crate::storage_pressure::sessions_storage_pressure(&error, &raw).unwrap_or(raw),
            )
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
        self.reopen_sidecars = Some(Arc::new(RecordedSidecar {
            artifact: sessions.created_composition_artifact(locator).await?,
        }));
        Ok(self)
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

    pub(crate) fn platform_credentials(&self) -> &dyn crate::platform::PlatformCredentials {
        self.platform_credentials.as_ref()
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
            exchange_observer: None,
            session_records: Arc::new(crate::session_store::MemoryStore::new()),
            subscriptions: state_home.join("plak/subscriptions"),
            runtime_database: product.join("runtime.sqlite3"),
            product,
            state_home,
            runtime_sessions: Arc::new(Mutex::new(None)),
            tree_store: Arc::new(Mutex::new(None)),
            tree_store_factory: Arc::new(doubles::MemoryTreeStoreFactory),
            roster_source: Arc::new(doubles::UnpinnedRosterSource),
            openrouter_credentials: Arc::new(doubles::NoOpenRouterCredential),
            platform_credentials: Arc::new(crate::platform::UnavailableCredentials),
            named_secrets: Arc::new(crate::mcp_binding::NoNamedSecrets),
            api_model_lists: crate::platform::ApiModelLists::Offline,
            api_window: crate::platform::ApiWindow::Working,
            created_semantic_evidence: None,
            sidecars: Arc::new(doubles::NoSidecars),
            reopen_sidecars: None,
            telemetry_factory: Arc::new(doubles::NoTelemetry),
            telemetry: Arc::new(Mutex::new(None)),
            telemetry_unavailable_logged: Arc::new(Mutex::new(false)),
            extra_telemetry_sink: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_test_platform_credentials(
        mut self,
        credentials: Arc<dyn crate::platform::PlatformCredentials>,
    ) -> Self {
        self.platform_credentials = credentials;
        self
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

    /// The topology observer this process publishes tree transitions through.
    pub(crate) fn topology_observer(&self) -> Option<Arc<dyn crate::agent_tree::TopologyObserver>> {
        self.open_telemetry_cache().topology_observer()
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

struct RecordedSidecar {
    artifact: Option<plakous::agent::LocalCompositionEngineArtifact>,
}

impl plakous::tools::SidecarLocator for RecordedSidecar {
    fn javascript_host(&self) -> Result<PathBuf, String> {
        self.host(plakous::agent::LocalCompositionEngineArtifact::rquickjs_sidecar_v1)
    }

    fn python_host(&self) -> Result<PathBuf, String> {
        self.host(plakous::agent::LocalCompositionEngineArtifact::monty_pool_v1)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use std::sync::Arc;

    use super::ProductStateRoots;

    struct UnreadableRoster;

    impl super::RosterSource for UnreadableRoster {
        fn roster(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> crate::CliResult<crate::orchestration::RosterInput> {
            Err(crate::CliFailure::new("named config error"))
        }

        fn account_name(&self, _: plakous::subscriptions::NativeProvider) -> &'static str {
            "codex"
        }

        fn topology_admitted(&self) -> crate::CliResult<bool> {
            Err(crate::CliFailure::new("named config error"))
        }
    }

    #[test]
    fn unreadable_roster_refuses_fresh_genesis_without_publishing_a_tree() {
        let home = tempfile::tempdir().unwrap();
        let mut roots =
            ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
        roots.roster_source = Arc::new(UnreadableRoster);
        let tree_id = uuid::Uuid::new_v4().to_string();
        let parent = crate::agent::SpawnParent::Api(crate::agent::ApiTopologyParent {
            prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
            created_output_bound: None,
            workspace_root: home.path().to_path_buf(),
            cwd: home.path().to_path_buf(),
            plan_state_authority: tree_id.clone(),
            composition: crate::binding::CompositionSelection::Direct,
            topology_tree_id: None,
            topology_caller_id: None,
            state_roots: roots.clone(),
            model: "offline-model".into(),
            effort: None,
            target: crate::binding::ApiTarget::OpenRouter {
                wire: crate::binding::ResolvedProtocol::Chat,
            },
            scheduled_compaction: true,
            max_model_rounds: 8,
            topology_enabled: true,
            product_home: Some(home.path().to_path_buf()),
            tool_selection: crate::tool_plan::NodeToolSelection::default(),
        });
        let result = crate::agent::build::topology_extensions_if_enabled(
            &parent,
            "offline-model",
            crate::topology_runtime::TopologyHubAttach::AttachRoot,
            false,
            &mut None,
        );
        assert!(
            result
                .err()
                .is_some_and(|error| error.message() == "named config error")
        );
        assert!(!roots.tree_store().unwrap().exists(&tree_id).unwrap());
    }

    #[test]
    fn reopen_pin_is_exact_engine_scoped_and_cleared_for_new_children() {
        use plakous::tools::SidecarLocator;
        struct Current(std::path::PathBuf);
        impl SidecarLocator for Current {
            fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
                Ok(self.0.clone())
            }
            fn python_host(&self) -> Result<std::path::PathBuf, String> {
                Ok(self.0.clone())
            }
        }
        let home = tempfile::tempdir().unwrap();
        let historical = home.path().join("historical");
        let current = home.path().join("current");
        std::fs::write(&historical, b"identical bytes").unwrap();
        std::fs::copy(&historical, &current).unwrap();
        let historical = historical.canonicalize().unwrap();
        let current = current.canonicalize().unwrap();
        let mut roots =
            ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
        roots.sidecars = Arc::new(Current(current.clone()));
        roots.reopen_sidecars = Some(Arc::new(super::RecordedSidecar {
            artifact: Some(
                plakous::agent::LocalCompositionEngineArtifact::rquickjs_sidecar_v1(&historical)
                    .unwrap(),
            ),
        }));
        assert_eq!(roots.sidecars().javascript_host().unwrap(), historical);
        assert!(roots.sidecars().python_host().is_err());
        assert_eq!(
            roots
                .clone()
                .without_reopen_artifact()
                .sidecars()
                .javascript_host()
                .unwrap(),
            current
        );
        std::fs::write(&historical, b"changed").unwrap();
        assert!(roots.sidecars().javascript_host().is_err());
        std::fs::remove_file(&historical).unwrap();
        assert!(roots.sidecars().javascript_host().is_err());
    }

    /// The roots open the topology store on the
    /// first ask and hand that same handle to every attach after it — on
    /// every clone of the roots, since the cache is shared.
    #[test]
    fn the_tree_store_is_opened_once_and_shared() {
        let home = tempfile::tempdir().unwrap();
        let roots =
            ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
        let first = roots.tree_store().unwrap();
        let second = roots.tree_store().unwrap();
        let third = roots.clone().tree_store().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first, &third));
    }

    /// The other half of fix 6, and the tree crate's own contract rather than
    /// a test name cited from here: an attach the hub can already answer asks
    /// the roots for no store at all.
    #[tokio::test]
    async fn a_hub_hit_asks_the_roots_for_no_store() {
        let home = tempfile::tempdir().unwrap();
        crate::topology_runtime::test_support::hub_hit_opens_no_store(home.path().to_path_buf())
            .await;
    }
}

/// In-process doubles of the product ports, for hosted tests.
#[cfg(any(test, feature = "test-support"))]
pub mod doubles {
    use std::sync::Arc;

    use super::{
        ProductTelemetry, RosterSource, TelemetryHealth, TelemetrySinkFactory, TreeStoreFactory,
    };
    use crate::CliResult;
    use crate::agent_tree::{MemoryTreeStore, TopologyObserver, TreeStore};

    /// The conformance-tested in-memory tree store, opened once per factory.
    pub struct MemoryTreeStoreFactory;

    impl TreeStoreFactory for MemoryTreeStoreFactory {
        fn open(&self) -> CliResult<Arc<dyn TreeStore>> {
            Ok(Arc::new(MemoryTreeStore::default()))
        }
    }

    /// A roster that freezes the tuple it is handed and reads no configuration.
    pub struct UnpinnedRosterSource;

    impl RosterSource for UnpinnedRosterSource {
        fn roster(
            &self,
            root_account: &str,
            root_tuple: &str,
            root_effort: Option<&str>,
        ) -> CliResult<crate::orchestration::RosterInput> {
            let mut input = crate::orchestration::RosterInput::unpinned(root_tuple);
            input.root.account = root_account.to_owned();
            input.root.effort = root_effort.map(str::to_owned);
            Ok(input)
        }

        fn account_name(&self, provider: plakous::subscriptions::NativeProvider) -> &'static str {
            match provider {
                plakous::subscriptions::NativeProvider::Codex => "codex",
                plakous::subscriptions::NativeProvider::Claude => "claude",
                _ => "grok",
            }
        }

        fn topology_admitted(&self) -> CliResult<bool> {
            Ok(true)
        }
    }

    /// Telemetry that records nothing and is never unavailable.
    pub struct NoTelemetry;

    impl TelemetrySinkFactory for NoTelemetry {
        fn open(
            &self,
            _extra_sink: Option<Arc<dyn super::ExtraTelemetrySink>>,
        ) -> Arc<dyn ProductTelemetry> {
            Arc::new(Self)
        }

        fn log_unavailable(&self, _session_id: &str, _reason: &str) {}
    }

    impl ProductTelemetry for NoTelemetry {
        fn agent_emitter(&self) -> Option<Arc<plakous::agent::TelemetryEmitter>> {
            None
        }

        fn topology_observer(&self) -> Option<Arc<dyn TopologyObserver>> {
            None
        }

        fn emit(&self, _observation: crate::product_observation::ProductObservation) {}

        fn health(&self) -> TelemetryHealth {
            TelemetryHealth::Missing
        }

        fn unavailable_reason(&self) -> Option<String> {
            None
        }
    }

    /// No evaluator workers are installed.
    pub struct NoSidecars;

    impl plakous::tools::SidecarLocator for NoSidecars {
        fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
            Err("no JavaScript evaluator worker is installed in this test".to_owned())
        }

        fn python_host(&self) -> Result<std::path::PathBuf, String> {
            Err("no Python evaluator worker is installed in this test".to_owned())
        }
    }

    /// No OpenRouter key is configured.
    pub struct NoOpenRouterCredential;

    impl crate::openrouter::OpenRouterCredentials for NoOpenRouterCredential {
        fn key_label(&self) -> CliResult<String> {
            Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
        }

        fn preflight(&self) -> CliResult<()> {
            Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
        }

        fn source(&self) -> CliResult<Arc<dyn plakous::openrouter::OpenRouterApiKeySource>> {
            Err(crate::CliFailure::new("OpenRouter API key is unavailable"))
        }
    }
}
