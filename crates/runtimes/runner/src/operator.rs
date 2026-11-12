//! Operator PTYs: private namespaces, bounded byte rings and no agent line state.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use plakous_runner_protocol::operator::{
    OperatorAnswer, OperatorId, OperatorNamespace, OperatorOperation, OperatorRefusal,
    TerminalCursor, TerminalDimensions, TerminalEvent, TerminalKey, TerminalPin,
    TerminalProcessIdentity, TerminalRow, TerminalStatus,
};

#[cfg(unix)]
mod pty;
#[cfg(unix)]
pub use pty::account_launch;
mod collection;
mod inventory;
mod stream;
mod table;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "operator lifecycle fixtures")]
mod tests;

const RING_BYTES: usize = 1024 * 1024;
const CHUNK_BYTES: usize = 16 * 1024;
const WINDOW_BYTES: u64 = 64 * 1024;
const IDLE: Duration = Duration::from_secs(15 * 60);
const DETACH: Duration = Duration::from_secs(5 * 60);
const NAMESPACE: Duration = Duration::from_secs(90);

/// Validated server-side launch context, never supplied as browser environment.
pub struct OperatorLaunch {
    /// Canonical, admitted workspace.
    pub cwd: PathBuf,
    /// Curated runner PATH, including an already-validated copy shim where needed.
    pub path: String,
    /// Runner user's home.
    pub home: PathBuf,
    /// Runner user's account name.
    pub user: String,
    /// Runner user's passwd shell (absolute), or validated SHELL fallback.
    pub shell: PathBuf,
    /// Copy-only discovery fence.
    pub discovery_ceiling: Option<PathBuf>,
}

/// Explicit product-boundary fallback when the runner account database is unavailable.
#[derive(Clone)]
pub struct OperatorAccountFallback {
    /// Validated absolute shell captured on the runner, never from a browser.
    pub shell: PathBuf,
    /// Runner-local home.
    pub home: PathBuf,
    /// Runner-local account name.
    pub user: String,
}

/// Validates workspace and constructs a runner-owned launch context on every open.
pub type OperatorWorkspace = dyn Fn(&Path) -> Result<OperatorLaunch, OperatorRefusal> + Send + Sync;

/// A separate operator service, usable in-process for host-local placement.
/// Its workspace policy is mandatory; absent policy never means unrestricted execution.
#[derive(Clone)]
pub struct OperatorTerminalService {
    inner: Arc<Inner>,
}

struct Inner {
    instance: OperatorId,
    workspace: Arc<OperatorWorkspace>,
    clock: Arc<crate::RunnerLeaseClock>,
    table: Mutex<Table>,
    backend: Arc<dyn Backend>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct NamespaceKey {
    host: String,
    generation: String,
    namespace: OperatorNamespace,
}

struct Namespace {
    loss_reported: HashSet<TerminalKey>,
    evicted: HashSet<[u8; 32]>,
    renewed: Instant,
    reconciled: u64,
    closed: bool,
    terminals: HashMap<TerminalKey, Terminal>,
    refused_opens: HashMap<TerminalKey, (String, OperatorRefusal)>,
    refused_order: VecDeque<TerminalKey>,
    refused_fence: HashMap<[u8; 32], ([u8; 32], OperatorRefusal)>,
}

impl Namespace {
    fn refused_open(&self, key: &TerminalKey, digest: &str) -> Option<OperatorRefusal> {
        if let Some((prior, refusal)) = self.refused_opens.get(key) {
            return Some(if prior == digest {
                *refusal
            } else {
                OperatorRefusal::Conflict
            });
        }
        self.refused_fence
            .get(&collection::terminal_hash(key))
            .map(|(prior, refusal)| {
                if *prior == collection::hash(digest.as_bytes()) {
                    *refusal
                } else {
                    OperatorRefusal::Conflict
                }
            })
    }
}

struct Table {
    leases: HashMap<u64, ReclamationLease>,
    scopes: HashMap<(String, OperatorId), collection::Generations>,
    namespaces: HashMap<NamespaceKey, Namespace>,
    next_stream: u64,
    reclaiming: HashMap<PathBuf, usize>,
    output_budgets: HashMap<(String, OperatorId), RateBudget>,
}

impl Default for Table {
    fn default() -> Self {
        Self {
            leases: HashMap::new(),
            scopes: HashMap::new(),
            namespaces: HashMap::new(),
            next_stream: 1 << 63,
            reclaiming: HashMap::new(),
            output_budgets: HashMap::new(),
        }
    }
}

/// Admission held across one reclamation future, mutually exclusive with terminal
/// opens in that workspace or its descendants. Dropping it releases only admission.
pub struct OperatorReclamation {
    service: OperatorTerminalService,
    workspace: PathBuf,
}

impl Drop for OperatorReclamation {
    fn drop(&mut self) {
        let mut table = lock(&self.service.inner.table);
        if let Some(count) = table.reclaiming.get_mut(&self.workspace) {
            *count -= 1;
            if *count == 0 {
                table.reclaiming.remove(&self.workspace);
            }
        }
    }
}

struct Terminal {
    digest: String,
    workspace: PathBuf,
    status: TerminalStatus,
    last_input: Instant,
    detached: Option<Instant>,
    controller: Option<Controller>,
    process: Option<Box<dyn Process>>,
    output: Arc<Mutex<Output>>,
    input_next: u64,
    input_rate: RateBudget,
    acknowledged: VecDeque<(u64, Vec<u8>)>,
    acknowledged_bytes: usize,
    pending_size: Option<TerminalDimensions>,
    exit_code: Option<u32>,
}

struct Controller {
    connection: u64,
    stream: u64,
    inbound_seq: u64,
    outbound_seq: u64,
    credit: u64,
    cursor: u64,
    output_rate: RateBudget,
    exit_status: Option<TerminalStatus>,
    warned_at: Option<Instant>,
}

struct RateBudget {
    at: Instant,
    remaining: u64,
}

impl RateBudget {
    fn new(at: Instant, limit: u64) -> Self {
        Self {
            at,
            remaining: limit,
        }
    }

    fn available(&mut self, now: Instant, period: Duration, limit: u64) -> u64 {
        if now.duration_since(self.at) >= period {
            self.at = now;
            self.remaining = limit;
        }
        self.remaining
    }
}

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    next: u64,
}

impl Output {
    fn push(&mut self, bytes: &[u8]) {
        self.next += bytes.len() as u64;
        let keep = bytes.len().min(RING_BYTES);
        let remove = (self.bytes.len() + keep).saturating_sub(RING_BYTES);
        self.bytes.drain(..remove);
        self.bytes.extend(&bytes[bytes.len() - keep..]);
    }

    fn oldest(&self) -> u64 {
        self.next - self.bytes.len() as u64
    }
}

trait Backend: Send + Sync {
    fn spawn(
        &self,
        launch: OperatorLaunch,
        dimensions: TerminalDimensions,
        output: Arc<Mutex<Output>>,
    ) -> Result<Box<dyn Process>, OperatorRefusal>;
}

trait Process: Send {
    /// No blocking writes: acceptance is into a bounded queue only.
    fn input(&mut self, bytes: Vec<u8>) -> Result<(), OperatorRefusal>;
    fn resize(&mut self, size: TerminalDimensions) -> Result<(), OperatorRefusal>;
    fn poll(&mut self) -> Option<(TerminalStatus, Option<u32>)>;
    /// Starts cleanup; never reports exited without the two absence scans.
    fn close(&mut self);
    fn reprove(&mut self, _acknowledge_loss: bool) {}
    fn survivors(&self) -> Vec<TerminalProcessIdentity> {
        Vec::new()
    }
}

impl OperatorTerminalService {
    /// Creates the independent terminal service with runner-owned workspace policy.
    ///
    /// # Errors
    /// Instance id is invalid.
    #[cfg(unix)]
    pub fn new(
        instance: String,
        workspace: Arc<OperatorWorkspace>,
        clock: Option<Arc<crate::RunnerLeaseClock>>,
    ) -> Result<Self, &'static str> {
        pty::probe_sessions()?;
        let service = Self {
            inner: Arc::new(Inner {
                instance: OperatorId::try_from(instance)?,
                workspace,
                clock: clock.unwrap_or_else(|| Arc::new(Instant::now)),
                table: Mutex::default(),
                backend: Arc::new(pty::PtyBackend),
            }),
        };
        let weak = Arc::downgrade(&service.inner);
        std::thread::Builder::new()
            .name("operator-lifetimes".into())
            .spawn(move || {
                loop {
                    let Some(inner) = weak.upgrade() else {
                        break;
                    };
                    OperatorTerminalService { inner }.tick();
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
            .map_err(|_| "operator lifecycle worker unavailable")?;
        Ok(service)
    }

    /// Applies one fixed operation. Host identity and generation come from Bind.
    /// Never use an agent owner, job table, accounting record or model observation.
    #[must_use]
    pub fn execute(
        &self,
        host: &str,
        generation: &str,
        connection: u64,
        namespace: OperatorNamespace,
        digest: &str,
        operation: OperatorOperation,
    ) -> OperatorAnswer {
        let key = NamespaceKey {
            host: host.into(),
            generation: generation.into(),
            namespace,
        };
        let result = self.apply(key, connection, digest, operation);
        result.unwrap_or_else(|refusal| OperatorAnswer::Refused { refusal })
    }

    /// Resolves an ambiguous open only from this instance's retained reservation.
    /// A missing receipt, including after restart, never authorizes a new spawn.
    #[must_use]
    pub fn replay_open(
        &self,
        host: &str,
        generation: &str,
        namespace: OperatorNamespace,
        digest: &str,
        key: &TerminalKey,
    ) -> OperatorAnswer {
        let table = lock(&self.inner.table);
        let owner = NamespaceKey {
            host: host.into(),
            generation: generation.into(),
            namespace,
        };
        let Some(terminal) = table
            .namespaces
            .get(&owner)
            .and_then(|ns| ns.terminals.get(key))
        else {
            if let Some(refusal) = table
                .namespaces
                .get(&owner)
                .and_then(|ns| ns.refused_open(key, digest))
            {
                return OperatorAnswer::Refused { refusal };
            }
            return OperatorAnswer::Refused {
                refusal: OperatorRefusal::TerminalLost,
            };
        };
        if terminal.digest != digest {
            return OperatorAnswer::Refused {
                refusal: OperatorRefusal::Conflict,
            };
        }
        OperatorAnswer::Terminal {
            terminal: terminal.row(key, &self.inner.instance),
            stream: None,
        }
    }

    /// Advances runner-enforced awake deadlines and asynchronous cleanup.
    /// Call even when no host connection exists.
    pub fn tick(&self) {
        let now = (self.inner.clock)();
        let mut table = lock(&self.inner.table);
        for namespace in table.namespaces.values_mut() {
            let expired = namespace.closed || now.duration_since(namespace.renewed) >= NAMESPACE;
            namespace.closed = expired;
            for terminal in namespace.terminals.values_mut() {
                if expired
                    || now.duration_since(terminal.last_input) >= IDLE
                    || terminal
                        .detached
                        .is_some_and(|at| now.duration_since(at) >= DETACH)
                {
                    terminal.close();
                }
                terminal.poll();
            }
        }
        collection::collect(&mut table);
    }

    /// Detaches only this connection's controllers; shells retain bounded lifetime.
    pub fn disconnect(&self, host: &str, connection: u64) {
        let now = (self.inner.clock)();
        let mut table = lock(&self.inner.table);
        table.release_leases(host, connection);
        for (key, namespace) in &mut table.namespaces {
            if key.host != host {
                continue;
            }
            for terminal in namespace.terminals.values_mut() {
                if terminal
                    .controller
                    .as_ref()
                    .is_some_and(|c| c.connection == connection)
                {
                    terminal.controller = None;
                    terminal.detached = Some(now);
                }
            }
        }
    }

    /// Whether live/starting/closing/unknown terminals prevent daemon idle exit.
    #[must_use]
    pub fn has_live_terminals(&self) -> bool {
        self.tick();
        let table = lock(&self.inner.table);
        !table.leases.is_empty()
            || table
                .namespaces
                .values()
                .any(|ns| ns.terminals.values().any(Terminal::pinned))
    }

    /// A pin includes descendant copies and unknown termination. Call under host
    /// admission serialization before dormancy, hand-off, delete or reclamation.
    #[must_use]
    pub fn workspace_pinned(&self, workspace: &Path) -> bool {
        let Ok(workspace) = resolved_workspace(workspace) else {
            return true;
        };
        lock(&self.inner.table).namespaces.values().any(|ns| {
            ns.terminals.values().any(|t| {
                t.pinned()
                    && (t.workspace.starts_with(&workspace) || workspace.starts_with(&t.workspace))
            })
        })
    }

    /// Holds reclamation admission until the caller's actual removal/trim finishes.
    ///
    /// # Errors
    /// A live or unresolved terminal pins this workspace or a descendant.
    pub fn reclaim(&self, workspace: &Path) -> Result<OperatorReclamation, OperatorRefusal> {
        let workspace = resolved_workspace(workspace)?;
        let mut table = lock(&self.inner.table);
        if table.namespaces.values().any(|ns| {
            ns.terminals.values().any(|t| {
                t.pinned()
                    && (t.workspace.starts_with(&workspace) || workspace.starts_with(&t.workspace))
            })
        }) {
            return Err(OperatorRefusal::TerminalBusy);
        }
        *table.reclaiming.entry(workspace.clone()).or_default() += 1;
        Ok(OperatorReclamation {
            service: self.clone(),
            workspace,
        })
    }

    /// Begins cleanup for all namespaces; unresolved pins remain visible.
    pub fn close_all(&self) {
        for ns in lock(&self.inner.table).namespaces.values_mut() {
            ns.closed = true;
            for terminal in ns.terminals.values_mut() {
                terminal.close();
            }
        }
    }
}

struct ReclamationLease {
    host: String,
    generation: String,
    connection: u64,
    workspace: PathBuf,
}

impl Table {
    fn release_lease(&mut self, stream: u64) {
        if let Some(lease) = self.leases.remove(&stream)
            && let Some(count) = self.reclaiming.get_mut(&lease.workspace)
        {
            *count -= 1;
            if *count == 0 {
                self.reclaiming.remove(&lease.workspace);
            }
        }
    }
    fn release_leases(&mut self, host: &str, connection: u64) {
        let streams: Vec<_> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.host == host && lease.connection == connection)
            .map(|(stream, _)| *stream)
            .collect();
        for stream in streams {
            self.release_lease(stream);
        }
    }
}

impl Terminal {
    fn pinned(&self) -> bool {
        !matches!(self.status, TerminalStatus::Exited | TerminalStatus::Lost)
    }

    fn close(&mut self) {
        if matches!(
            self.status,
            TerminalStatus::Starting | TerminalStatus::Running
        ) {
            self.status = TerminalStatus::Closing;
            if let Some(process) = &mut self.process {
                process.close();
            }
        }
    }

    fn poll(&mut self) {
        let Some(process) = &mut self.process else {
            return;
        };
        if let Some(size) = self.pending_size.take()
            && process.resize(size).is_err()
        {
            self.status = TerminalStatus::TerminationUnknown;
        }
        if let Some((status, code)) = process.poll() {
            self.status = status;
            self.exit_code = code;
        }
        if !self.pinned() && self.controller.is_none() {
            // Keep the lightweight idempotency receipt, not an unbounded archive
            // of closed shells. A later attach observes the honest output gap.
            lock(&self.output).bytes = VecDeque::new();
        }
    }

    fn row(&self, key: &TerminalKey, instance: &OperatorId) -> TerminalRow {
        let output = lock(&self.output);
        TerminalRow {
            survivors: self
                .process
                .as_ref()
                .map_or_else(Vec::new, |p| p.survivors()),
            key: key.clone(),
            status: self.status,
            oldest: TerminalCursor {
                instance: instance.clone(),
                offset: output.oldest(),
            },
            next: TerminalCursor {
                instance: instance.clone(),
                offset: output.next,
            },
            input_next: self.input_next,
            attached: self.controller.is_some(),
            exit_code: self.exit_code,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn resolved_workspace(path: &Path) -> Result<PathBuf, OperatorRefusal> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(OperatorRefusal::WorkspaceUnavailable);
    }
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or(OperatorRefusal::WorkspaceUnavailable)?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or(OperatorRefusal::WorkspaceUnavailable)?;
            }
            Err(_) => return Err(OperatorRefusal::WorkspaceUnavailable),
        }
    }
}
