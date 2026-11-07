//! Shared runner connections keyed by pinned identity, never configured alias or path.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use plakous_tools::ExecutionBackend as _;
use serde::{Deserialize, Serialize};

use crate::{
    RunnerClient, RunnerServices, WorkspaceCatalog, WorkspaceDiscovery, WorkspaceOptions,
    WorkspaceRefusal,
};

mod refresh;
pub use refresh::{RunnerProbe, RunnerProbeFuture};

/// Progress/failure phase of a runner probe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbePhase {
    /// Waiting for the bounded probe pool, not a transport timeout.
    Queued,
    /// Strict SSH transport.
    Ssh,
    /// Cold daemon start.
    Daemon,
    /// Authenticated bind.
    Bind,
    /// Effective roots read.
    Options,
    /// Bounded checkout discovery.
    Discovery,
    /// First-bind sweep, recovery and recorded controls.
    Reconcile,
    /// Previously live connection ended.
    LinkLost,
}

/// Neutral placement failures; product/wire map these without parsing prose.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlacementFailure {
    /// Exact remote protocol/revision/flavor mismatch.
    RunnerVersionMismatch {
        /// Required triple.
        expected: plakous_runner_protocol::VersionTriple,
        /// Endpoint triple.
        actual: plakous_runner_protocol::VersionTriple,
    },
    /// Same-machine execution authorities overlap.
    RunnerRootsOverlap {
        /// Stable physical-machine id.
        machine_id: String,
        /// Conflicting runner id, or `local`.
        conflicting_runner: String,
        /// Authenticated operator path detail.
        path: String,
    },
    /// No configured alias remains for this pinned identity.
    RunnerNotConfigured {
        /// Saved runner identity.
        saved_id: String,
    },
    /// Pairing/machine evidence is absent.
    RunnerNotPaired,
    /// Endpoint identity differs from the saved pairing.
    RunnerIdentityMismatch {
        /// Pinned identity.
        expected: String,
        /// Endpoint identity.
        actual: String,
    },
    /// A connection or reconciliation phase is unavailable.
    RunnerUnreachable {
        /// Failed phase.
        phase: ProbePhase,
        /// Whether its budget elapsed.
        timed_out: bool,
    },
    /// Fresh runner-side workspace refusal.
    WorkspaceRefused {
        /// Typed path refusal.
        reason: WorkspaceRefusal,
    },
}

/// Configured identity and display aliases. Aliases never retarget a saved session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredRunner {
    /// Pinned identity (`local` is the host's own runner).
    pub runner_id: String,
    /// All configured aliases, sorted/deduplicated by registration.
    pub names: Vec<String>,
}

/// Latest completed probe, not a promise of continuous reachability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunnerStatus {
    /// No completed probe since host start.
    Unknown,
    /// Bind and first-bind reconciliation completed.
    Ready,
    /// A typed failure; other runners remain usable.
    Blocked(PlacementFailure),
}

/// Cached picker/status data. Reads never block on SSH.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunnerRow {
    /// Configured pinned identity and aliases.
    pub runner: ConfiguredRunner,
    /// Latest readiness fact.
    pub status: RunnerStatus,
    /// Host Unix seconds of the latest completed probe.
    pub checked_at: Option<u64>,
    /// Start of the uninterrupted blocked interval.
    pub blocked_since: Option<u64>,
    /// Effective daemon roots/default, absent before a successful options read.
    pub options: Option<WorkspaceOptions>,
    /// Latest bounded discovery snapshot.
    pub discovery: WorkspaceDiscovery,
}

/// Bounded process-local picker progress; one final row set remains authoritative.
#[derive(Clone, Debug)]
pub struct RunnerProgress {
    /// Pinned identity shared by aliases.
    pub runner_id: String,
    /// Probe phase.
    pub phase: ProbePhase,
    /// Completed row, absent for phase-only progress.
    pub row: Option<RunnerRow>,
}

struct Entry {
    row: RunnerRow,
    connection: Option<Connection>,
    refresh: refresh::ProbeState,
    required: bool,
}

#[derive(Clone)]
enum Connection {
    Remote(RunnerClient, RunnerServices),
    Local(Arc<WorkspaceCatalog>),
}

/// Process-owned registry: aliases of one id share one existing client and link
/// executor. It does not dial or construct a second inspecting connection.
#[derive(Clone)]
pub struct RunnerRegistry {
    entries: Arc<RwLock<BTreeMap<String, Entry>>>,
    changed: Arc<tokio::sync::watch::Sender<u64>>,
    progress: Arc<tokio::sync::broadcast::Sender<RunnerProgress>>,
    probe: Arc<RwLock<Option<Arc<RunnerProbe>>>>,
    probe_pool: Arc<tokio::sync::Semaphore>,
}

impl Default for RunnerRegistry {
    fn default() -> Self {
        Self {
            entries: Arc::default(),
            changed: Arc::new(tokio::sync::watch::channel(0).0),
            progress: Arc::new(tokio::sync::broadcast::channel(64).0),
            probe: Arc::default(),
            probe_pool: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}

/// Validated canonical placement and explicit node services, before product commit.
pub struct BoundWorkspace {
    /// Frozen pinned identity.
    pub runner_id: String,
    /// Canonical root, byte-identical to the request.
    pub root: String,
    /// Initial canonical cwd (equal to root).
    pub cwd: String,
    /// Runner services for the facade builder.
    pub services: Option<RunnerServices>,
}

impl RunnerRegistry {
    /// Observes shared probes without opening another connection. Lagging
    /// readers use final cached rows.
    #[must_use]
    pub fn progress_events(&self) -> tokio::sync::broadcast::Receiver<RunnerProgress> {
        self.progress.subscribe()
    }

    /// Publishes a phase without changing completed-probe timestamps.
    pub fn progress(&self, id: &str, phase: ProbePhase) {
        if let Some(entry) = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            entry.refresh.phase = phase;
        }
        let _ = self.progress.send(RunnerProgress {
            runner_id: id.into(),
            phase,
            row: None,
        });
    }
    /// Process-local readiness changes, including delayed first bind. A
    /// continuation scanner waits here rather than blocking other runners.
    #[must_use]
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }
    /// Registers aliases of one pin. Re-registration preserves live probe state.
    pub fn configure(&self, mut runner: ConfiguredRunner) {
        runner.names.sort();
        runner.names.dedup();
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get_mut(&runner.runner_id) {
            entry.row.runner = runner;
        } else {
            entries.insert(
                runner.runner_id.clone(),
                Entry {
                    row: RunnerRow {
                        runner,
                        status: RunnerStatus::Unknown,
                        checked_at: None,
                        blocked_since: None,
                        options: None,
                        discovery: WorkspaceDiscovery::default(),
                    },
                    connection: None,
                    refresh: refresh::ProbeState::default(),
                    required: false,
                },
            );
        }
    }

    /// Instantaneous configured-runner rows in pinned-id order. Canonical
    /// display name is `names.first()`; other names are aliases.
    #[must_use]
    pub fn rows(&self) -> Vec<RunnerRow> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|entry| !entry.row.runner.names.is_empty())
            .map(|entry| entry.row.clone())
            .collect()
    }

    /// Publishes a fully reconciled connection. The caller must complete
    /// sweep → recover → recorded stops → settled-foreground reconciliation
    /// first. A read timeout must not cancel that separately owned work.
    ///
    /// # Errors
    /// Rejects unknown ids or backend identity mismatch before publication.
    pub fn ready(
        &self,
        id: &str,
        client: RunnerClient,
        mut services: RunnerServices,
        options: WorkspaceOptions,
        discovery: WorkspaceDiscovery,
        now: u64,
    ) -> Result<(), PlacementFailure> {
        if services.backend().runner_id().as_str() != id {
            return Err(PlacementFailure::RunnerIdentityMismatch {
                expected: id.into(),
                actual: services.backend().runner_id().as_str().into(),
            });
        }
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get_mut(id).ok_or_else(|| missing(id))?;
        if entry.row.runner.names.is_empty() {
            return Err(missing(id));
        }
        if let Some(Connection::Remote(_, previous)) = &entry.connection
            && previous.backend().identity() == services.backend().identity()
        {
            services = previous.clone();
        }
        services.set_available(true);
        entry.row.status = RunnerStatus::Ready;
        entry.row.checked_at = Some(now);
        entry.row.blocked_since = None;
        entry.row.options = Some(options);
        entry.row.discovery = discovery;
        entry.connection = Some(Connection::Remote(client, services));
        let _ = self.progress.send(RunnerProgress {
            runner_id: id.into(),
            phase: ProbePhase::Reconcile,
            row: Some(entry.row.clone()),
        });
        self.changed
            .send_modify(|sequence| *sequence = sequence.wrapping_add(1));
        Ok(())
    }

    /// Marks local ready only after the home-local process daemon has bound
    /// and reconciled. Workspace reads remain direct on the host.
    ///
    /// # Errors
    /// Local must have been explicitly configured first.
    pub fn ready_local(
        &self,
        catalog: Arc<WorkspaceCatalog>,
        now: u64,
    ) -> Result<(), PlacementFailure> {
        let options = catalog.options();
        let discovery = catalog.discover();
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get_mut("local").ok_or_else(|| missing("local"))?;
        entry.row.status = RunnerStatus::Ready;
        entry.row.checked_at = Some(now);
        entry.row.blocked_since = None;
        entry.row.options = Some(options);
        entry.row.discovery = discovery;
        entry.connection = Some(Connection::Local(catalog));
        let _ = self.progress.send(RunnerProgress {
            runner_id: "local".into(),
            phase: ProbePhase::Reconcile,
            row: Some(entry.row.clone()),
        });
        self.changed
            .send_modify(|sequence| *sequence = sequence.wrapping_add(1));
        Ok(())
    }

    /// Removes labels, not admitted calls or the client needed to reconcile
    /// them. New creation/open refuses rather than falling back to local.
    pub fn remove(&self, id: &str) {
        if let Some(entry) = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            entry.row.runner.names.clear();
            if let Some(Connection::Remote(_, services)) = &entry.connection {
                services.set_available(false);
            }
            entry.row.status = RunnerStatus::Blocked(missing(id));
        }
    }

    /// Records failure without erasing committed roots or replacing the shared
    /// client needed to reconcile admitted calls. Repeated failures keep since.
    pub fn blocked(&self, id: &str, failure: PlacementFailure, now: u64) {
        if let Some(entry) = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            entry.row.blocked_since.get_or_insert(now);
            if let Some(Connection::Remote(_, services)) = &entry.connection {
                services.set_available(false);
            }
            entry.row.checked_at = Some(now);
            entry.row.status = RunnerStatus::Blocked(failure);
            let phase = match &entry.row.status {
                RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable { phase, .. }) => *phase,
                _ => ProbePhase::Bind,
            };
            let _ = self.progress.send(RunnerProgress {
                runner_id: id.into(),
                phase,
                row: Some(entry.row.clone()),
            });
            self.changed
                .send_modify(|sequence| *sequence = sequence.wrapping_add(1));
        }
    }

    fn connection(&self, id: &str) -> Result<Connection, PlacementFailure> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(id).ok_or_else(|| missing(id))?;
        match &entry.row.status {
            RunnerStatus::Ready => entry.connection.clone().ok_or_else(reconciling),
            RunnerStatus::Unknown => Err(reconciling()),
            RunnerStatus::Blocked(reason) => Err(reason.clone()),
        }
    }

    /// Services for reopening a saved identity after the first-bind barrier.
    /// `None` denotes local; an absent/blocked id never falls back to local.
    ///
    /// # Errors
    /// Placement is not configured or reconciled.
    pub fn services(&self, id: &str) -> Result<Option<RunnerServices>, PlacementFailure> {
        Ok(match self.connection(id)? {
            Connection::Remote(_, services) => Some(services),
            Connection::Local(_) => None,
        })
    }

    /// Existing client for process reconciliation, including while blocked.
    /// This never creates a second bind and never grants new-call admission.
    #[must_use]
    pub fn client(&self, id: &str) -> Option<RunnerClient> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .and_then(|entry| match &entry.connection {
                Some(Connection::Remote(client, _)) => Some(client.clone()),
                _ => None,
            })
    }

    /// Retained services for an owned recovery/probe, without opening their
    /// new-call barrier. Removed identities still refuse.
    ///
    /// # Errors
    /// The identity is no longer configured or has never bound.
    pub fn reconciliation_services(&self, id: &str) -> Result<RunnerServices, PlacementFailure> {
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        let entry = entries
            .get(id)
            .filter(|entry| !entry.row.runner.names.is_empty())
            .ok_or_else(|| missing(id))?;
        match &entry.connection {
            Some(Connection::Remote(_, services)) => Ok(services.clone()),
            _ => Err(reconciling()),
        }
    }

    /// Keeps a deferred continuation/stop eligible for reconnect without an
    /// attached node. Clear after its owned reconciliation finishes.
    pub fn require_connection(&self, id: &str, required: bool) {
        if let Some(entry) = self
            .entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(id)
        {
            entry.required = required;
        }
    }

    /// Whether live node authority, a journal or a durable deferred action
    /// needs reconnect; idle picker rows alone do not maintain a connection.
    #[must_use]
    pub fn needs_connection(&self, id: &str) -> bool {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .is_some_and(|entry| {
                entry.required
                    || match &entry.connection {
                        Some(Connection::Remote(client, services)) => {
                            client.has_pending_calls() || services.has_node_users()
                        }
                        _ => false,
                    }
            })
    }

    /// Releases an idle link without misreporting a failed probe.
    pub fn idle(&self, id: &str) {
        if let Some(entry) = self
            .entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            && let Some(Connection::Remote(client, services)) = &entry.connection
        {
            services.set_available(false);
            client.disconnect();
        }
    }

    /// Local catalog remains host-owned; the process daemon serves no files.
    #[must_use]
    pub fn local_catalog(&self) -> Option<Arc<WorkspaceCatalog>> {
        self.entries
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get("local")
            .and_then(|entry| match &entry.connection {
                Some(Connection::Local(catalog)) => Some(catalog.clone()),
                _ => None,
            })
    }

    /// Fresh remote canonical validation, using only the shared connection.
    ///
    /// # Errors
    /// Returns typed placement/path failure without any product or runtime save.
    pub async fn validate_workspace(
        &self,
        id: &str,
        path: &str,
        require_canonical: bool,
    ) -> Result<String, PlacementFailure> {
        self.refresh(Some(&[id.to_owned()])).await?;
        validate(self.connection(id)?, path, require_canonical).await
    }

    /// Fresh creation validation followed by an explicit immutable service
    /// bundle. Caller commits it as creation intent; this method saves nothing.
    ///
    /// # Errors
    /// Refuses unavailable placement and noncanonical spelling before commit.
    pub async fn bind_session_root(
        &self,
        id: &str,
        path: &str,
    ) -> Result<BoundWorkspace, PlacementFailure> {
        self.refresh(Some(&[id.to_owned()])).await?;
        let connection = self.connection(id)?;
        let root = validate(connection.clone(), path, true).await?;
        let services = match connection {
            Connection::Remote(_, services) => Some(services),
            Connection::Local(_) => None,
        };
        Ok(BoundWorkspace {
            runner_id: id.into(),
            cwd: root.clone(),
            root,
            services,
        })
    }
}

async fn validate(
    connection: Connection,
    path: &str,
    require_canonical: bool,
) -> Result<String, PlacementFailure> {
    let result = match connection {
        Connection::Remote(client, _) => client
            .workspace_validate(path, require_canonical)
            .await
            .map_err(|_| PlacementFailure::RunnerUnreachable {
            phase: ProbePhase::Options,
            timed_out: false,
        })?,
        Connection::Local(catalog) => {
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || catalog.validate(&path, require_canonical))
                .await
                .map_err(|_| reconciling())?
        }
    };
    result.map_err(|reason| PlacementFailure::WorkspaceRefused { reason })
}

fn missing(id: &str) -> PlacementFailure {
    PlacementFailure::RunnerNotConfigured {
        saved_id: id.into(),
    }
}
fn reconciling() -> PlacementFailure {
    PlacementFailure::RunnerUnreachable {
        phase: ProbePhase::Reconcile,
        timed_out: false,
    }
}
