//! A host of many sessions (spec 02 §4.4, the session directory): every
//! session of its stores, served by id on one connection that holds at
//! most one node attachment and at most one host attachment. A session is
//! resident in a [`SessionSlot`] while anything keeps it, and dormant
//! otherwise; an attach of its root, or of a node of its tree, opens it
//! again. The node an attach names is looked up, never guessed: a resident
//! session whose tree it is, else the newest session of that tree as the
//! product's stores say.
//!
//! A connection's node attachment is one [`WireConnection`] on that
//! session's shared service, made when the connection first attaches the
//! session and kept while it holds an attachment there. It holds the
//! session resident from the moment the slot hands it over, so an idle
//! release never takes a session between its open and the attach. The
//! host attachment is the directory: a snapshot of every row, then each row
//! that changed, read again when a session or product summary changes.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_host_protocol::ProtocolExtension;
use plakous_host_protocol::wire::{
    Answer, AttachmentId, ClientFrame, HostFrame, NodeAddress, Op, Outcome, RequestRefusal,
    ResidentReason, SessionActivity, SessionEnvironment, SessionResidency, SessionRow, Welcome,
    WireError, decode_line,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::watch;

use super::super::ResidencyHold;
use super::slot::{OpenedSession, SessionSlot, SlotView};
use super::{BlobRefusalOutput, Face, WireConnection, WireSession, host_welcome, write_frame};
use crate::error::{CliFailure, CliResult};

/// How long `release_session` waits for its session to begin releasing.
pub const RELEASE_WAIT: Duration = Duration::from_secs(10);

/// How long `release_session` waits for a release that began to commit, as
/// a slot waits for a release's join.
const RELEASE_JOIN: Duration = Duration::from_secs(60);

/// A session `new_session` created.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Created {
    /// The durable session id.
    pub session: String,
    /// Its root node.
    pub node: NodeAddress,
    /// The environment its tools run with.
    pub environment: SessionEnvironment,
}

/// Persisted attach facts, without runtime ownership.
pub struct StoredSessionView<P: ProtocolExtension> {
    /// Archived sessions cannot retry execution until unarchived.
    pub archived: bool,
    /// The product session id.
    pub session: String,
    /// The persisted root address.
    pub root: NodeAddress,
    /// Complete bounded attach facts, including the root transcript snapshot.
    pub observations: Vec<plakous_host_protocol::SessionObservation<P>>,
}

/// One bounded picker phase or completed row, scoped to the caller's request.
pub struct PickerProgress {
    /// Canonical runner identity.
    pub runner: plakous_host_protocol::wire::RunnerLabel,
    /// Active phase; none means queued for a probe slot.
    pub phase: Option<plakous_host_protocol::wire::RunnerPhase>,
    /// Completed row, at most once per runner in a request.
    pub row: Option<plakous_host_protocol::wire::RunnerOptions>,
}

/// Borrowed complete creation input, resolved by the product before one record commit.
pub struct Creation<'a> {
    /// Explicit pinned runner selection.
    pub runner: &'a plakous_host_protocol::wire::RunnerLabel,
    /// Canonical runner-owned workspace bytes.
    pub workspace: &'a str,
    /// User-facing profile, exclusive with template/model.
    pub profile: Option<&'a str>,
    /// Advanced root template override.
    pub template: Option<&'a str>,
    /// Advanced root model override.
    pub model: Option<&'a plakous_host_protocol::wire::CreationModel>,
    /// Explicit account choices. Empty means the product fills each kind.
    pub accounts: &'a [plakous_host_protocol::wire::AccountChoice],
    /// Stable create-action UUID.
    pub request_id: &'a str,
}

mod attach;
mod blobs;
mod lifecycle;
mod product;
mod upgrade_controls;
mod verdicts;
pub use lifecycle::{LifecycleOutput, LifecycleState};
pub use product::HostSessions;

/// Host-owned overlay of one session, independent of a wire version.
pub struct DirectorySlot {
    /// Where the current service is in its open or release cycle.
    pub residency: SessionResidency,
    /// Open failed and the host is not automatically retrying.
    pub open_failed: bool,
    /// Live workload and typed attention, only while resident.
    pub resident: Option<(
        super::super::KeptActivity,
        super::super::KeptDirectoryAttention,
    )>,
}

/// The sessions one host serves.
pub struct SessionHost<P: ProtocolExtension> {
    lifecycle: Mutex<BTreeMap<u64, lifecycle::Delivery<lifecycle::Notice<P>>>>,
    lifecycle_latest: Mutex<Option<lifecycle::Notice<P>>>,
    upgrade_admission: Arc<std::sync::RwLock<bool>>,
    upgrade_controls: Mutex<admission::PausedControls>,
    upgrade_committed: Arc<std::sync::atomic::AtomicBool>,
    pending_stops: Mutex<Vec<Arc<super::StopSettlement<P>>>>,
    admission: admission::Admission,
    next_connection: AtomicU64,
    drafts: super::super::drafts::DraftRegistry,
    sessions: Arc<dyn HostSessions<P>>,
    slots: Mutex<Vec<Arc<SessionSlot<P>>>>,
    changes: watch::Sender<u64>,
    next_host_attachment: AtomicU64,
    binary_id: String,
    generation: u64,
    release_wait: Duration,
    /// How long `release_session` waits for an open or a release already
    /// under way ([`RELEASE_JOIN`]).
    release_join: Duration,
    /// Set once the host stops: nothing opens or is created after it.
    closing: Arc<std::sync::atomic::AtomicBool>,
}

/// Whether this nonblocking slot view names `session`. Busy views use a
/// cached peek label, not an authority for selecting a session to open.
fn serves<P: ProtocolExtension>(slot: &SessionSlot<P>, session: &str) -> bool {
    match slot.peek() {
        SlotView::Resident(served) => served.session_id() == session,
        SlotView::Dormant(id) | SlotView::Opening(id) | SlotView::Releasing(id) => id == session,
    }
}

/// The refusal of a request that would open or create a session on a
/// stopping host.
fn stopping() -> RequestRefusal {
    RequestRefusal::SessionUnavailable {
        detail: "the host is stopping".to_owned(),
    }
}

impl<P: ProtocolExtension> SessionHost<P>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// A host of `sessions`, from the binary `binary_id` in its process
    /// generation `generation`.
    #[must_use]
    pub fn new(
        sessions: impl HostSessions<P> + 'static,
        binary_id: impl Into<String>,
        generation: u64,
    ) -> Arc<Self> {
        Self::new_shared(Arc::new(sessions), binary_id, generation)
    }

    /// Constructs a host sharing its product directory with a startup worker.
    #[must_use]
    pub fn new_shared(
        sessions: Arc<dyn HostSessions<P>>,
        binary_id: impl Into<String>,
        generation: u64,
    ) -> Arc<Self> {
        let (changes, _) = watch::channel(0);
        let admission = admission::Admission::new(sessions.host_read_limits());
        Arc::new(Self {
            lifecycle: Mutex::default(),
            lifecycle_latest: Mutex::default(),
            upgrade_admission: Arc::default(),
            upgrade_controls: Mutex::default(),
            upgrade_committed: Arc::default(),
            pending_stops: Mutex::default(),
            admission,
            next_connection: AtomicU64::new(1),
            drafts: super::super::drafts::DraftRegistry::default(),
            sessions,
            slots: Mutex::new(Vec::new()),
            changes,
            next_host_attachment: AtomicU64::new(1),
            binary_id: binary_id.into(),
            generation,
            release_wait: RELEASE_WAIT,
            release_join: RELEASE_JOIN,
            closing: Arc::default(),
        })
    }

    /// Stops the host: from now on no session opens and none is created,
    /// so the sessions it holds are all it will hold.
    fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    fn closed(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// The whole visible directory.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    pub fn directory(&self) -> Result<Arc<Vec<SessionRow>>, String> {
        self.directory_with_archived(false)
    }

    /// Snapshot the current slots for the draft directory, without opening a session.
    #[must_use]
    pub fn directory_slots(&self) -> BTreeMap<String, DirectorySlot> {
        self.slots()
            .iter()
            .map(|slot| {
                use SessionResidency::{Dormant, Opening, Releasing, Resident};
                let (id, residency, resident) = match slot.peek() {
                    SlotView::Dormant(id) => (id, Dormant, None),
                    SlotView::Opening(id) => (id, Opening, None),
                    SlotView::Releasing(id) => (id, Releasing, None),
                    SlotView::Resident(service) => {
                        let (id, facts) = service.directory_resident();
                        (id, Resident, Some(facts))
                    }
                };
                (
                    id,
                    DirectorySlot {
                        residency,
                        resident,
                        open_failed: slot.open_failure().is_some() && !slot.startup_retrying(),
                    },
                )
            })
            .collect()
    }

    /// Notify a draft directory watch of changes to the host overlay.
    #[must_use]
    pub fn directory_slot_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn directory_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Arc<Vec<SessionRow>>, String> {
        self.rows(None, None, include_archived).map(Arc::new)
    }

    /// [`Self::new`] whose `release_session` waits `release_wait`.
    #[cfg(test)]
    pub(crate) fn with_release_wait(
        sessions: impl HostSessions<P> + 'static,
        release_wait: Duration,
    ) -> Arc<Self> {
        let mut host = Self::new(sessions, "b1", 7);
        if let Some(host) = Arc::get_mut(&mut host) {
            host.release_wait = release_wait;
        }
        host
    }

    /// [`Self::with_release_wait`] that also bounds an open or a release
    /// already under way by `release_join`.
    #[cfg(test)]
    pub(crate) fn with_release_bounds(
        sessions: impl HostSessions<P> + 'static,
        release_wait: Duration,
        release_join: Duration,
    ) -> Arc<Self> {
        let mut host = Self::with_release_wait(sessions, release_wait);
        if let Some(host) = Arc::get_mut(&mut host) {
            host.release_join = release_join;
        }
        host
    }

    /// The welcome every connection gets: it names no session.
    #[must_use]
    pub fn welcome_for(&self, principal: &HostPrincipal) -> Welcome<P> {
        let mut welcome = host_welcome(
            self.binary_id.clone(),
            self.generation,
            self.sessions
                .welcome_extension(principal, &self.admission.limits),
        );
        welcome.state_key = self.sessions.state_key();
        welcome
    }

    /// A new connection writing its frames to `out`, from a client its
    /// acceptor marked `face`; it ends once `stopped` turns true.
    /// The acceptor supplies the same verified principal used for welcome.
    /// ```
    /// use plakous_host_protocol::Core;
    /// use plakous_hosted::host_service::wire::{Face, HostPrincipal, SessionHost};
    /// use std::sync::Arc;
    /// fn verified(host: Arc<SessionHost<Core>>, verified_uid: u32) {
    ///     let principal = HostPrincipal::Socket(verified_uid);
    ///     let _welcome = host.welcome_for(&principal);
    ///     let _connection = host.connect(std::io::sink(), Face::Remote, principal, || false);
    /// }
    /// ```
    ///
    /// A connection cannot be constructed without that identity:
    /// ```compile_fail
    /// use plakous_host_protocol::Core;
    /// use plakous_hosted::host_service::wire::{Face, SessionHost};
    /// use std::sync::Arc;
    /// fn missing_identity(host: Arc<SessionHost<Core>>) {
    ///     host.connect(std::io::sink(), Face::Remote, || false);
    /// }
    /// ```
    pub fn connect<W: Write + Clone>(
        self: &Arc<Self>,
        out: W,
        face: Face,
        principal: HostPrincipal,
        stopped: impl Fn() -> bool + Send + Sync + 'static,
    ) -> HostConnection<P, W> {
        let connection_id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        // Register under the same lock order as publication, so a concurrent
        // drain is either seeded here or queued by the publisher, never missed.
        let latest = self
            .lifecycle_latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(connection_id, lifecycle::Delivery::new(latest.clone()));
        drop(latest);
        HostConnection {
            connection_id,
            principal_budget: self.admission.principal(&principal),
            login: match &principal {
                HostPrincipal::Web(login) => Some(login.clone()),
                HostPrincipal::Socket(_) => None,
            },
            principal,
            read_ready: Arc::default(),
            host_reads: BTreeMap::new(),
            attaching: None,
            attach_queue: VecDeque::new(),
            host: Arc::clone(self),
            host_changes: self.changes.subscribe(),
            directory_changes: self.sessions.directory_changes(),
            out,
            buffered_output: false,
            snapshot_output: None,
            blob_refusal_output: None,
            lifecycle_output: None,
            face,
            stopped: Arc::new(stopped),
            node: None,
            readonly: None,
            draining: None,
            watch: None,
        }
    }

    /// The directory, newest first: the stores' rows with what this host
    /// holds of each.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    pub fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionRow>, String> {
        let mut rows = self.sessions.rows(workspace, runner, include_archived)?;
        let held: BTreeMap<_, _> = self
            .lock()
            .iter()
            .map(|slot| {
                let view = slot.peek();
                let id = match &view {
                    SlotView::Resident(session) => session.session_id(),
                    SlotView::Dormant(id) | SlotView::Opening(id) | SlotView::Releasing(id) => {
                        id.clone()
                    }
                };
                (
                    id,
                    (
                        view,
                        slot.open_failure().is_some() && !slot.startup_retrying(),
                    ),
                )
            })
            .collect();
        for row in &mut rows {
            let entry = held.get(&row.session);
            if entry.is_some_and(|(_, needs_operator)| *needs_operator) {
                row.needs_operator = true;
            }
            match entry.map(|(view, _)| view) {
                None | Some(SlotView::Dormant(_)) => {}
                Some(SlotView::Opening(_)) => row.residency = SessionResidency::Opening,
                Some(SlotView::Releasing(_)) => row.residency = SessionResidency::Releasing,
                Some(SlotView::Resident(session)) => {
                    row.residency = SessionResidency::Resident;
                    // This process holds the tree.
                    row.hosted_elsewhere = None;
                    row.environment = SessionEnvironment::Host;
                    let kept = session.kept_activity();
                    row.live_work.processes = u32::try_from(kept.processes).unwrap_or(u32::MAX);
                    row.live_work.children = u32::try_from(kept.children).unwrap_or(u32::MAX);
                    row.live_work.held_children =
                        u32::try_from(kept.held_children).unwrap_or(u32::MAX);
                    row.needs_operator |= needs_operator(&kept, &row.runner_status);
                    if kept.busy {
                        row.activity = SessionActivity::Busy;
                    }
                }
            }
        }
        Ok(rows)
    }

    /// The slot of the session whose tree `node` is in: the resident one
    /// serving that tree, or the head session of that tree's root line.
    fn resolve(&self, node: &NodeAddress) -> Result<Arc<SessionSlot<P>>, RequestRefusal> {
        for slot in self.lock().iter() {
            if let SlotView::Resident(session) = slot.peek()
                && session.root().tree_id == node.tree_id
            {
                return Ok(Arc::clone(slot));
            }
        }
        let session = self
            .sessions
            .session_of(node)
            .map_err(|detail| RequestRefusal::SessionUnavailable { detail })?
            .ok_or(RequestRefusal::UnknownNode)?;
        Ok(self.slot(&session))
    }

    /// The slot of `session`, made dormant when the host has none yet.
    pub fn slot(&self, session: &str) -> Arc<SessionSlot<P>> {
        // A candidate may be opening. Wait only for that slot, never under
        // the host-wide lock used by other attaches and directory readers.
        loop {
            let snapshot = self.slots();
            if let Some(slot) = snapshot
                .iter()
                .find(|slot| serves(slot, session) && slot.serves_current(session))
            {
                return Arc::clone(slot);
            }
            let mut slots = self.lock();
            if slots.len() == snapshot.len()
                && slots
                    .iter()
                    .zip(snapshot)
                    .all(|(old, current)| Arc::ptr_eq(old, &current))
                && !slots.iter().any(|slot| serves(slot, session))
            {
                let slot = self.released_slot(session);
                slots.push(Arc::clone(&slot));
                return slot;
            }
        }
    }

    fn released_slot(&self, session: &str) -> Arc<SessionSlot<P>> {
        let (opener, closing) = (Arc::clone(&self.sessions), Arc::clone(&self.closing));
        Arc::new(
            SessionSlot::released(session, move |id: &str| {
                // A stopping host opens nothing its handoff would miss.
                if closing.load(Ordering::SeqCst) {
                    return Err(CliFailure::new("the host is stopping"));
                }
                opener.open(id)
            })
            .watch_directory(self.changes.clone()),
        )
    }

    /// Every slot the host holds, resident or not.
    pub fn slots(&self) -> Vec<Arc<SessionSlot<P>>> {
        self.lock().clone()
    }

    /// Cheap in-memory blockers, safe to check on the accept loop each second.
    #[must_use]
    pub fn idle_residents_blocked(&self) -> bool {
        self.sessions.operator_terminals_blocked(None)
            || self
                .slots()
                .iter()
                .any(|slot| !matches!(slot.peek(), SlotView::Dormant(_)))
    }

    /// Durable eligibility; callers must run this off their accept/frame loop.
    #[must_use]
    pub fn durable_idle_exit_blocked(&self) -> bool {
        self.sessions.durable_idle_exit_blocked()
    }

    /// Changes whenever the host opened, released or created a session.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Tells every host attachment that the directory may have changed.
    pub fn changed(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<SessionSlot<P>>>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection's node attachment: its connection on one session's shared
/// service, and its hold on that session's residency.
struct NodeLink<P: ProtocolExtension, W: Write> {
    session: Arc<WireSession<P>>,
    connection: WireConnection<P, W>,
    changes: watch::Receiver<u64>,
    /// The session's release requests when the link was made; a later one
    /// detaches it.
    releases: u64,
    _hold: ResidencyHold,
}

struct ReadOnlyLink<P: ProtocolExtension, W: Write> {
    session: String,
    refusal: RequestRefusal,
    connection: WireConnection<P, W>,
    _mailbox: plakous_host_protocol::IntentReceiver<P>,
}

/// A connection's host attachment: the rows it sent last.
struct Watch {
    include_archived: bool,
    attachment: AttachmentId,
    seq: u64,
    sent: Option<BTreeMap<String, SessionRow>>,
    dirty: bool,
}

/// One connection to a host of many sessions.
pub struct HostConnection<P: ProtocolExtension, W: Write> {
    buffered_output: bool,
    snapshot_output: Option<super::SnapshotOutput>,
    blob_refusal_output: Option<BlobRefusalOutput>,
    lifecycle_output: Option<LifecycleOutput<P>>,
    connection_id: u64,
    principal: HostPrincipal,
    principal_budget: Arc<admission::PrincipalBudget>,
    read_ready: Arc<reads::ReadyReads>,
    host_reads: BTreeMap<plakous_host_protocol::wire::RequestId, reads::PendingRead<P>>,
    attaching: Option<attach::PendingAttach<P>>,
    attach_queue: VecDeque<attach::AttachRequest>,
    host: Arc<SessionHost<P>>,
    host_changes: watch::Receiver<u64>,
    directory_changes: Option<watch::Receiver<u64>>,
    out: W,
    face: Face,
    stopped: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The login its transport admitted, which each session connection it
    /// makes is identified by.
    login: Option<String>,
    node: Option<NodeLink<P, W>>,
    readonly: Option<ReadOnlyLink<P, W>>,
    /// A node attachment that ended while its `stop_session` was in flight:
    /// its session connection, and its hold on the session's residency,
    /// stay until the stop is answered.
    draining: Option<NodeLink<P, W>>,
    watch: Option<Watch>,
}

impl<P: ProtocolExtension, W: Write> Drop for HostConnection<P, W> {
    fn drop(&mut self) {
        self.host
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.connection_id);
        self.host_reads.clear();
        self.host.sessions.host_disconnect(
            &self.principal,
            self.connection_id,
            self.host.generation,
        );
    }
}

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// Answers one client line; `false` once the client has left.
    ///
    /// Buffered transports complete an attach in a later flush.
    /// Request ids must be unique across all pending operations of this
    /// connection, including host reads and session operations.
    ///
    /// # Errors
    ///
    /// A lost output.
    pub fn frame(&mut self, line: Result<String, WireError>) -> CliResult<bool> {
        let gate = Arc::clone(&self.host.upgrade_admission);
        let paused = gate.read().unwrap_or_else(PoisonError::into_inner);
        let Ok(text) = line else {
            self.leave();
            return Ok(false);
        };
        let mut text = zeroize::Zeroizing::new(text);
        if let Some(reading) = self.preflight_host_read(&text)? {
            if reading {
                self.discard_blob_staging()?;
            }
            return Ok(reading);
        }
        let frame = match decode_line::<ClientFrame<P>>(&text) {
            // A second hello, a bye, or a line that is not a frame: the
            // client left.
            Ok(ClientFrame::Hello(_) | ClientFrame::Bye) | Err(_) => {
                self.leave();
                return Ok(false);
            }
            Ok(frame) => frame,
        };
        if *paused && !self.allowed_during_pause(&frame) {
            match frame {
                ClientFrame::Request { id, .. } => {
                    self.discard_blob_staging()?;
                    write_frame(
                        &mut self.out,
                        &HostFrame::<P>::Response {
                            id,
                            outcome: Outcome::Refused(RequestRefusal::Backpressure),
                        },
                    )?;
                }
                ClientFrame::Blob(chunk) => self.discard_blob_part(chunk.id)?,
                _ => {}
            }
            return Ok(true);
        }
        let ClientFrame::Request { id, op } = frame else {
            // A blob precedes the intent that names it, on the node
            // attachment; with none it is no one's.
            return if self.node.is_some() {
                self.forward(std::mem::take(&mut *text))
            } else {
                if let ClientFrame::Blob(chunk) = frame {
                    self.discard_blob_part(chunk.id)?;
                }
                Ok(true)
            };
        };
        if self.attach_in_flight(id) {
            self.discard_blob_staging()?;
            write_frame(
                &mut self.out,
                &HostFrame::<P>::Response {
                    id,
                    outcome: Outcome::Refused(RequestRefusal::Malformed {
                        detail: "request id is already in flight".into(),
                    }),
                },
            )?;
            return Ok(true);
        }
        if !matches!(op, Op::Intent { .. }) {
            self.discard_blob_staging()?;
        }
        if matches!(
            op,
            Op::Intent { .. }
                | Op::Detach { .. }
                | Op::ResumeSession { .. }
                | Op::Rebind { .. }
                | Op::RetryAttach { .. }
        ) {
            self.flush_attachments(false)?;
        }
        match self.request(id, op, &text, !*paused)? {
            Forwarded::Answered(outcome) => {
                self.discard_blob_staging()?;
                write_frame(&mut self.out, &HostFrame::<P>::Response { id, outcome })?;
                Ok(true)
            }
            Forwarded::Reading(reading) => Ok(reading),
        }
    }

    fn answer_new_session(&mut self, op: Op<P>) -> Forwarded<P> {
        let Op::NewSession {
            runner,
            profile,
            model,
            workspace,
            template,
            request_id,
            accounts,
            ..
        } = op
        else {
            unreachable!("NewSession is the only op forwarded here");
        };
        Forwarded::Answered(self.create_session(&Creation {
            runner: &runner,
            workspace: &workspace,
            profile: profile.as_deref(),
            template: template.as_deref(),
            model: model.as_ref(),
            accounts: &accounts,
            request_id: &request_id,
        }))
    }

    fn request(
        &mut self,
        request_id: plakous_host_protocol::wire::RequestId,
        op: Op<P>,
        text: &str,
        allow_open: bool,
    ) -> CliResult<Forwarded<P>> {
        let answered = |outcome| Ok(Forwarded::Answered(outcome));
        let refused = |refusal| Ok(Forwarded::Answered(Outcome::Refused(refusal)));
        if let Some(link) = &self.readonly
            && let Op::Intent { attachment, .. }
            | Op::ResumeSession { attachment, .. }
            | Op::Rebind { attachment, .. } = &op
        {
            if link.connection.attachment() != Some(*attachment)
                || !link.connection.service.attached(*attachment)
            {
                return refused(RequestRefusal::NotAttached);
            }
            return refused(link.refusal.clone());
        }
        match op {
            Op::Host { request } => Ok(self.start_host_read(request_id, request)),
            Op::Ping => {
                super::test_timing::pong("dispatch", request_id);
                answered(Outcome::Ok(Answer::Pong))
            }
            Op::ListSessions {
                runner,
                include_archived,
                workspace,
            } => match self.host.rows(
                workspace.as_deref(),
                runner
                    .as_deref()
                    .or_else(|| workspace.as_ref().map(|_| "local")),
                include_archived,
            ) {
                Ok(rows) => answered(Outcome::Ok(Answer::Sessions { rows })),
                Err(detail) => refused(RequestRefusal::SessionUnavailable { detail }),
            },
            Op::WatchSessions { include_archived } => {
                answered(self.watch_sessions(include_archived))
            }
            Op::UnwatchSessions { attachment } => {
                if self.watch.as_ref().map(|watch| watch.attachment) != Some(attachment) {
                    return refused(RequestRefusal::NotAttached);
                }
                self.watch = None;
                answered(Outcome::Ok(Answer::Detached))
            }
            Op::NewSession { .. } if self.host.closed() => refused(stopping()),
            op @ Op::NewSession { .. } => Ok(self.answer_new_session(op)),
            Op::ReleaseSession { session } => answered(self.release(&session)?),
            Op::SessionOptions { workspace, .. } => {
                answered(self.session_options(request_id, workspace.as_ref()))
            }
            Op::RenameSession { session, title } => {
                answered(self.rename_session(session, title.as_deref()))
            }
            Op::ArchiveSession { session, archived } => {
                answered(self.archive_session(session, archived))
            }
            Op::DeleteSession {
                session,
                abandon_children,
                abandon_jobs,
            } => answered(self.delete_session(session, abandon_children, abandon_jobs)),
            Op::StopSession { session } => self.stop(&session, text),
            Op::Attach { node } => self.start_attach(request_id, node, text, allow_open),
            Op::RetryAttach {
                attachment,
                request_id,
            } => answered(self.reopen(attachment, &request_id, None)),
            Op::Rebind {
                attachment,
                request_id,
                cause,
            } => answered(self.reopen(attachment, &request_id, Some(&cause))),
            Op::Detach { .. } | Op::Intent { .. } | Op::ResumeSession { .. } => {
                if self.node.is_none() && self.readonly.is_none() {
                    return refused(RequestRefusal::NotAttached);
                }
                if self
                    .node
                    .as_ref()
                    .is_some_and(|link| link.session.reopening())
                {
                    return refused(RequestRefusal::SessionUnavailable {
                        detail: "the session is reopening".to_owned(),
                    });
                }
                self.forward(text.to_owned()).map(Forwarded::Reading)
            }
        }
    }

    fn watch_sessions(&mut self, include_archived: bool) -> Outcome<P> {
        if self.watch.is_some() {
            return Outcome::Refused(RequestRefusal::OneInFlight);
        }
        let attachment = self
            .host
            .next_host_attachment
            .fetch_add(1, Ordering::Relaxed);
        self.watch = Some(Watch {
            include_archived,
            attachment,
            seq: 0,
            sent: None,
            dirty: true,
        });
        Outcome::Ok(Answer::Watching { attachment })
    }

    fn reopen(
        &mut self,
        attachment: AttachmentId,
        request: &str,
        cause: Option<&str>,
    ) -> Outcome<P> {
        if let Some(link) = &self.readonly {
            if link.connection.attachment() != Some(attachment)
                || !link.connection.service.attached(attachment)
            {
                return Outcome::Refused(RequestRefusal::NotAttached);
            }
            if cause.is_some() || matches!(link.refusal, RequestRefusal::SessionArchived { .. }) {
                return Outcome::Refused(link.refusal.clone());
            }
            if self.host.closed() {
                return Outcome::Refused(stopping());
            }
            let session = link.session.clone();
            let result = self.host.slot(&session).resident_at_handoff();
            self.host.changed();
            return match result {
                Ok(((served, _hold, _releases), _opened)) => {
                    let node = served.root();
                    link.connection
                        .service
                        .move_attachments(&node, Some((attachment, request)));
                    Outcome::Ok(Answer::Moved { node })
                }
                Err(failure) => Outcome::Refused(self.host.sessions.refusal(&session, &failure)),
            };
        }
        let Some(link) = &self.node else {
            return Outcome::Refused(RequestRefusal::NotAttached);
        };
        if link.connection.attachment() != Some(attachment)
            || !link.connection.service.attached(attachment)
        {
            return Outcome::Refused(RequestRefusal::NotAttached);
        }
        if link.connection.service.is_child(attachment) {
            return Outcome::Refused(RequestRefusal::RefusedForRole {
                row: if cause.is_some() {
                    "workflow"
                } else {
                    "readiness"
                }
                .to_owned(),
            });
        }
        if link.connection.stopping() || self.draining.is_some() {
            return Outcome::Refused(RequestRefusal::OneInFlight);
        }
        let session = link
            .connection
            .session
            .clone()
            .unwrap_or_else(|| link.session.session_id());
        let source_root = link.session.root();
        let slot = self.host.slot(&session);
        let sessions = Arc::clone(&self.host.sessions);
        let result = slot.reopen(
            &link.session,
            &session,
            attachment,
            request,
            |source| match cause {
                Some(cause) => sessions
                    .rebind(source, cause)
                    .map(|created| (created.session, created.node)),
                None => Ok((source.to_owned(), source_root)),
            },
        );
        self.host.changed();
        match result {
            Ok(node) => Outcome::Ok(Answer::Moved { node }),
            Err(failure) => Outcome::Refused(self.host.sessions.refusal(&session, &failure)),
        }
    }

    fn stop(&mut self, session: &str, text: &str) -> CliResult<Forwarded<P>> {
        let refused = |refusal| Ok(Forwarded::Answered(Outcome::Refused(refusal)));
        if self.readonly.as_ref().is_some_and(|link| {
            link.session == session
                && matches!(link.refusal, RequestRefusal::SessionArchived { .. })
        }) {
            return Ok(Forwarded::Answered(Outcome::Ok(Answer::Stopped {
                runner_pending: false,
            })));
        }
        if self.draining.is_some()
            || self
                .node
                .as_ref()
                .is_some_and(|link| link.connection.stopping())
        {
            return refused(RequestRefusal::OneInFlight);
        }
        if self
            .node
            .as_ref()
            .is_some_and(|link| link.session.session_id() == session)
        {
            let reading = self.forward(text.to_owned())?;
            if let Some(link) = &self.node {
                self.track_stop(&link.connection);
            }
            return Ok(Forwarded::Reading(reading));
        }
        if !self.knows(session) {
            return refused(RequestRefusal::UnknownSession);
        }
        tracing::info!(target: "plak::host", face = ?self.face, login = self.login.as_deref().unwrap_or("-"), session, "stop_session without attachment");
        let slot = self.host.slot(session);
        let sessions = Arc::clone(&self.host.sessions);
        match slot.stop_target(session, |id| sessions.stop_dormant(id)) {
            Ok(None) => {
                self.host.changed();
                Ok(Forwarded::Answered(Outcome::Ok(Answer::Stopped {
                    runner_pending: self.host.sessions.runner_pending(session),
                })))
            }
            Ok(Some((session, hold, releases))) => {
                let stopped = Arc::clone(&self.stopped);
                let mut connection = session
                    .connect(self.out.clone(), self.face, move || stopped())
                    .with_buffered_output(self.buffered_output);
                if let Some(login) = &self.login {
                    connection = connection.identified(login.clone());
                }
                let reading = connection.frame(Ok(text.to_owned()))?;
                self.track_stop(&connection);
                self.draining = Some(NodeLink {
                    changes: session.changes(),
                    releases,
                    session,
                    connection,
                    _hold: hold,
                });
                Ok(Forwarded::Reading(reading))
            }
            Err(failure) => refused(self.host.sessions.refusal(session, &failure)),
        }
    }

    fn track_stop(&self, connection: &WireConnection<P, W>) {
        if let Some((_, stop, _)) = &connection.stop {
            let mut pending = self
                .host
                .pending_stops
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pending.retain(|tracked| tracked.pending());
            pending.push(Arc::clone(stop));
        }
    }

    fn create_session(&self, creation: &Creation<'_>) -> Outcome<P> {
        if creation.profile.is_some() && (creation.template.is_some() || creation.model.is_some()) {
            return Outcome::Refused(RequestRefusal::Malformed {
                detail: "profile is exclusive with template and model".into(),
            });
        }
        match self.host.sessions.create(creation) {
            Ok(created) => {
                self.host.changed();
                Outcome::Ok(Answer::Created {
                    session: created.session,
                    node: created.node,
                    environment: created.environment,
                })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }

    fn session_options(
        &mut self,
        request_id: plakous_host_protocol::wire::RequestId,
        workspace: Option<&plakous_host_protocol::wire::WorkspaceProbe>,
    ) -> Outcome<P> {
        let out = &mut self.out;
        let mut progress = |progress: PickerProgress| {
            write_frame::<P>(
                out,
                &HostFrame::OptionsProgress {
                    request_id,
                    runner: progress.runner,
                    phase: progress.phase,
                    row: progress.row,
                },
            )
            .map_err(|failure| failure.message().to_owned())
        };
        match self.host.sessions.options(workspace, &mut progress) {
            Ok(options) => Outcome::Ok(Answer::Options {
                options: Box::new(options),
            }),
            Err(refusal) => Outcome::Refused(refusal),
        }
    }

    fn rename_session(&self, session: String, title: Option<&str>) -> Outcome<P> {
        match self.host.sessions.rename(&session, title) {
            Ok(title) => {
                self.host.changed();
                Outcome::Ok(Answer::Renamed { session, title })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }

    fn archive_session(&self, session: String, archived: bool) -> Outcome<P> {
        if let Err(refusal) = self.require_lifecycle_session(&session) {
            return Outcome::Refused(refusal);
        }
        let slot = self.host.slot(&session);
        let sessions = Arc::clone(&self.host.sessions);
        match slot.mutate_lifecycle(&session, archived, || sessions.archive(&session, archived)) {
            Ok(()) => {
                self.host.changed();
                Outcome::Ok(Answer::ArchiveUpdated { session, archived })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }

    fn delete_session(
        &self,
        session: String,
        abandon_children: bool,
        abandon_jobs: bool,
    ) -> Outcome<P> {
        if let Err(refusal) = self.require_lifecycle_session(&session) {
            return Outcome::Refused(refusal);
        }
        let slot = self.host.slot(&session);
        let sessions = Arc::clone(&self.host.sessions);
        match slot.mutate_lifecycle(&session, true, || {
            sessions.delete(&session, abandon_children, abandon_jobs)
        }) {
            Ok(()) => {
                self.host.lock().retain(|held| {
                    !Arc::ptr_eq(held, &slot)
                        || slot.known() != session
                        || !matches!(slot.peek(),SlotView::Dormant(ref id) if id==&session)
                });
                self.host.changed();
                Outcome::Ok(Answer::Deleted { session })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }
    fn require_lifecycle_session(&self, session: &str) -> Result<(), RequestRefusal> {
        match self.host.sessions.contains(session) {
            Ok(true) => Ok(()),
            Ok(false) => Err(RequestRefusal::SessionLifecycleRefused {
                session: session.to_owned(),
                reason: plakous_host_protocol::wire::LifecycleReason::NotFound,
            }),
            Err(detail) => Err(RequestRefusal::SessionUnavailable { detail }),
        }
    }
    fn attach_resident(
        &mut self,
        (session, hold, releases): super::slot::HeldSession<P>,
        text: &str,
    ) -> CliResult<Forwarded<P>> {
        if self
            .node
            .as_ref()
            .is_some_and(|link| Arc::ptr_eq(&session, &link.session) && link.releases == releases)
        {
            return self.forward(text.to_owned()).map(Forwarded::Reading);
        }
        let stopped = Arc::clone(&self.stopped);
        let sessions = Arc::clone(&self.host.sessions);
        let mut connection = session
            .connect(self.out.clone(), self.face, move || stopped())
            .with_buffered_output(self.buffered_output)
            .with_snapshot_output(self.snapshot_output.clone())
            .with_blob_refusal_output(self.blob_refusal_output)
            .with_drafts(self.host.drafts.clone())
            .with_runner_labels(Arc::new(move |id| sessions.runner_label(id)));
        if let Some(login) = &self.login {
            connection = connection.identified(login.clone());
        }
        let mut link = NodeLink {
            changes: session.changes(),
            releases,
            session,
            connection,
            _hold: hold,
        };
        // The connection's attachment moves only once the new one is made:
        // a refused attach leaves it as it was.
        if !link.connection.frame(Ok(text.to_owned()))? {
            self.leave();
            return Ok(Forwarded::Reading(false));
        }
        if link.connection.holding() {
            self.unlink();
            self.node = Some(link);
        } else {
            link.connection.leave();
        }
        Ok(Forwarded::Reading(true))
    }

    /// Whether `session` is one of this host's: a slot holds it, or the
    /// directory lists it.
    fn knows(&self, session: &str) -> bool {
        self.host.lock().iter().any(|slot| slot.known() == session)
            || self
                .host
                .directory_with_archived(true)
                .is_ok_and(|rows| rows.iter().any(|row| row.session == session))
    }

    /// `release_session`: every attachment of `session` detaches (this
    /// connection's own first, ended with `detached` like the others), then
    /// the session releases as soon as nothing else keeps it resident.
    /// Answered `released` once the release committed (its worker ended), or
    /// `resident` with what keeps it when none began within
    /// [`RELEASE_WAIT`], and the request is withdrawn; a dormant session is
    /// `released`. An open or a release already under way settles first.
    ///
    /// # Errors
    ///
    /// A lost output.
    fn release(&mut self, session: &str) -> CliResult<Outcome<P>> {
        if let Some(outcome) = operator::resident(&self.host, session) {
            return Ok(outcome);
        }
        let slot = self
            .host
            .lock()
            .iter()
            .find(|slot| serves(slot, session))
            .cloned();
        let Some(slot) = slot else {
            return Ok(if self.knows(session) {
                Outcome::Ok(Answer::Released)
            } else {
                Outcome::Refused(RequestRefusal::UnknownSession)
            });
        };
        let served = match self.settled(&slot)? {
            Settled::Dormant => return Ok(Outcome::Ok(Answer::Released)),
            Settled::Opening => {
                return Ok(Outcome::Ok(Answer::Resident {
                    reason: ResidentReason::Work,
                }));
            }
            Settled::Resident(served) => served,
        };
        if self
            .node
            .as_ref()
            .is_some_and(|link| Arc::ptr_eq(&link.session, &served))
        {
            self.detach_link()?;
        }
        // The other connections see the request when the host's change
        // wakes them, and detach.
        served.request_release();
        self.host.changed();
        let deadline = Instant::now() + self.host.release_wait;
        let mut lost = Ok(());
        while lost.is_ok()
            && matches!(slot.peek(), SlotView::Resident(_))
            && Instant::now() < deadline
        {
            lost = self.flush_draining();
            if lost.is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        // A lost output must withdraw too: otherwise the request keeps
        // asking and the session releases the moment it is next idle.
        served.cancel_release();
        lost?;
        // A release that began by the deadline cannot be withdrawn: it is
        // answered once it committed.
        if !matches!(self.settled(&slot)?, Settled::Resident(_)) {
            self.host.changed();
            return Ok(Outcome::Ok(Answer::Released));
        }
        let kept = served.kept_activity();
        let reason = if served.attached() > 0 {
            ResidentReason::Attachment
        } else if kept.processes > 0 {
            ResidentReason::Processes
        } else {
            ResidentReason::Work
        };
        Ok(Outcome::Ok(Answer::Resident { reason }))
    }

    /// Waits, bounded by [`RELEASE_JOIN`], until `slot` is neither opening
    /// nor releasing. A release still running past the bound began and is
    /// answered as committed (`Dormant`); an open still running is
    /// `Opening`.
    fn settled(&mut self, slot: &SessionSlot<P>) -> CliResult<Settled<P>> {
        let joined = Instant::now() + self.host.release_join;
        loop {
            match slot.peek() {
                SlotView::Dormant(_) => return Ok(Settled::Dormant),
                SlotView::Resident(served) => return Ok(Settled::Resident(served)),
                SlotView::Releasing(_) if Instant::now() >= joined => return Ok(Settled::Dormant),
                SlotView::Opening(_) if Instant::now() >= joined => return Ok(Settled::Opening),
                SlotView::Releasing(_) | SlotView::Opening(_) => {}
            }
            self.flush_draining()?;
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ends the node attachment with the host frame `detached`, and lets its
    /// session go.
    fn detach_link(&mut self) -> CliResult<()> {
        self.discard_blob_staging()?;
        let attachment = self
            .node
            .as_ref()
            .and_then(|link| link.connection.attachment());
        self.unlink();
        if let Some(attachment) = attachment {
            write_frame(&mut self.out, &HostFrame::<P>::Detached { attachment })?;
        }
        Ok(())
    }

    /// Hands `text` to the node attachment's connection; `false` once the
    /// client has left. A connection left holding nothing lets its session
    /// go.
    fn forward(&mut self, text: String) -> CliResult<bool> {
        if let Some(link) = &mut self.readonly {
            let reading = link.connection.frame(Ok(text))?;
            if !reading || !link.connection.holding() {
                self.readonly = None;
            }
            return Ok(reading);
        }
        let Some(link) = &mut self.node else {
            return Ok(true);
        };
        if !link.connection.frame(Ok(text))? {
            self.leave();
            return Ok(false);
        }
        if !link.connection.holding() {
            self.unlink();
        }
        Ok(true)
    }

    /// Writes what the node attachment and the host attachment owe: a
    /// node attachment whose session is no longer resident ends with
    /// `detached`, and the directory goes out when it is due.
    ///
    /// # Errors
    ///
    /// A lost output, a snapshot that cannot be paged, or a directory that
    /// cannot be read.
    pub fn flush(&mut self) -> CliResult<()> {
        self.flush_replies()?;
        self.flush_output(true)
    }

    /// Flushes attachment output after the driver has given ready client
    /// frames a chance to run. A backed-up writer may defer live observations
    /// and the directory, but not attachment endings.
    ///
    /// # Errors
    ///
    /// A lost output, a snapshot that cannot be paged, or a directory that
    /// cannot be read.
    pub fn flush_output(&mut self, bulk: bool) -> CliResult<()> {
        self.flush_attachments(bulk)?;
        if !bulk {
            return Ok(());
        }
        self.flush_directory()?;
        let context = self.host_context();
        for _ in 0..4 {
            let Some(frame) = self.host.sessions.host_stream(&context) else {
                break;
            };
            write_frame(&mut self.out, &frame)?;
        }
        Ok(())
    }

    /// Settles replies, stops and ended attachments without draining live
    /// observations or refreshing the directory behind a backed-up writer.
    ///
    /// # Errors
    ///
    /// The transport outbox cannot accept output.
    pub fn flush_control(&mut self) -> CliResult<()> {
        self.flush_replies()?;
        self.flush_output(false)
    }

    /// Settles lifecycle notices, host reads, attaches and stops, in that
    /// order, without draining the new attachment's baseline.
    ///
    /// # Errors
    ///
    /// The transport outbox cannot accept output.
    pub fn flush_replies(&mut self) -> CliResult<()> {
        self.flush_lifecycle()?;
        self.flush_host_reads()?;
        self.flush_attach()?;
        self.flush_draining()
    }

    fn flush_attachments(&mut self, bulk: bool) -> CliResult<()> {
        let _timing = super::test_timing::phase(if bulk {
            "drain attachment"
        } else {
            "settle ended attachment"
        })
        .entered();
        if let Some(link) = &mut self.readonly {
            if !matches!(link.refusal, RequestRefusal::SessionArchived { .. }) {
                let root = self.host.lock().iter().find_map(|slot| {
                    if serves(slot, &link.session)
                        && let SlotView::Resident(session) = slot.peek()
                    {
                        return Some(session.root());
                    }
                    None
                });
                if let Some(root) = root {
                    link.connection.service.move_attachments(&root, None);
                }
            }
            link.connection.flush_pending(bulk)?;
        }
        if let Some(link) = &mut self.node {
            // A session that ended, or one a `release_session` since the
            // link asked for, is no longer this connection's.
            let detach = !link.session.reopening()
                && (link.session.ended() || link.session.releases() > link.releases);
            let paced = link.connection.snapshot_output.is_some();
            if detach
                && paced
                && let Some(attachment) = link.connection.attachment()
            {
                link.connection.service.finish_attachment(attachment);
            }
            link.connection.flush_pending(bulk || (detach && !paced))?;
            if !link.connection.holding() {
                link.connection.discard_blob_staging()?;
                self.unlink();
            } else if detach
                && (!paced
                    || link
                        .connection
                        .attachment()
                        .is_none_or(|attachment| !link.connection.service.has_queued(attachment)))
            {
                self.detach_link()?;
                self.host.changed();
            }
        }
        Ok(())
    }

    /// Answers the ended attachment's stop once it settled, and then lets
    /// its session go.
    fn flush_draining(&mut self) -> CliResult<()> {
        if let Some(link) = &mut self.draining {
            link.connection.flush()?;
            if !link.connection.stopping() {
                self.draining = None;
            }
        }
        Ok(())
    }

    fn flush_directory(&mut self) -> CliResult<()> {
        let host_changed = consume_change(&mut self.host_changes);
        let product_changed = self.directory_changes.as_mut().is_some_and(consume_change);
        let Some(watch) = &mut self.watch else {
            return Ok(());
        };
        if !watch.dirty && !host_changed && !product_changed {
            return Ok(());
        }
        // A failed read is skipped; the next publication permits a retry.
        let Ok(rows) = self.host.directory_with_archived(watch.include_archived) else {
            return Ok(());
        };
        let rows: Vec<SessionRow> = rows
            .iter()
            .filter(|row| watch.include_archived || !row.archived)
            .cloned()
            .collect();
        let watch = self
            .watch
            .as_mut()
            .ok_or_else(|| CliFailure::new("unwatched"))?;
        watch.dirty = false;
        let now: BTreeMap<String, SessionRow> = rows
            .iter()
            .map(|row| (row.session.clone(), row.clone()))
            .collect();
        let (snapshot, out) = match &watch.sent {
            // A row gone from the directory is only said by a snapshot.
            Some(sent) if sent.keys().all(|session| now.contains_key(session)) => (
                false,
                rows.into_iter()
                    .filter(|row| sent.get(&row.session) != Some(row))
                    .collect::<Vec<_>>(),
            ),
            _ => (true, rows),
        };
        watch.sent = Some(now);
        if !snapshot && out.is_empty() {
            return Ok(());
        }
        watch.seq += 1;
        let frame = HostFrame::<P>::Directory {
            attachment: watch.attachment,
            seq: watch.seq,
            snapshot,
            rows: out,
        };
        write_frame(&mut self.out, &frame)
    }

    /// Waits until the connection may have something to write: the node
    /// attachment's session or the host directory published a change.
    pub async fn changed(&mut self) {
        let stream = self.host.sessions.host_stream_notify(&self.host_context());
        let directory = self
            .directory_changes
            .as_mut()
            .filter(|_| self.watch.is_some());
        let host = &mut self.host_changes;
        let session = self.node.as_mut().map(|link| &mut link.changes);
        let draining = self.draining.as_mut().map(|link| &mut link.changes);
        let deadline = self.host_reads.values().map(|read| read.deadline).min();
        let host_changed = tokio::select! {
            () = async { match stream { Some(notify) => notify.notified().await, None => std::future::pending::<()>().await } } => false,
            () = self.read_ready.notify.notified() => false,
            () = async { match deadline { Some(at) => tokio::time::sleep_until(at.into()).await, None => std::future::pending::<()>().await } } => false,
            _ = host.changed() => true,
            () = session_changed(directory) => true,
            () = session_changed(session) => false,
            () = session_changed(draining) => false,
        };
        // The wait saw the host's change: the directory is due.
        if host_changed && let Some(watch) = &mut self.watch {
            watch.dirty = true;
        }
    }

    /// The client left: its node attachment detaches, its host attachment
    /// closes, and its session hold ends.
    pub fn leave(&mut self) {
        self.attaching = None;
        self.attach_queue.clear();
        self.host_reads.clear();
        self.host.sessions.host_disconnect(
            &self.principal,
            self.connection_id,
            self.host.generation,
        );
        self.unlink();
        // No one is left to answer.
        self.draining = None;
        self.watch = None;
    }

    /// Ends the node attachment. What its end owes goes out now (a pending
    /// request's `not_attached`), and a stop in flight keeps its session
    /// connection draining until the stop is answered.
    fn unlink(&mut self) {
        if let Some(mut link) = self.readonly.take() {
            link.connection.leave();
        }
        if let Some(mut link) = self.node.take() {
            link.connection.leave();
            // A lost output is seen by the next write.
            let _ = link.connection.flush();
            if link.connection.stopping() {
                self.draining = Some(link);
            }
        }
    }
}

/// Resolves once `changes`, when there are any, changed; a session that
/// ended changes no more.
fn consume_change(changes: &mut watch::Receiver<u64>) -> bool {
    let pending = changes.has_changed().unwrap_or(false);
    if pending {
        changes.borrow_and_update();
    }
    pending
}

async fn session_changed(changes: Option<&mut watch::Receiver<u64>>) {
    match changes {
        Some(changes) => {
            if changes.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending().await,
    }
}

/// Where a slot's open or release in flight came to.
enum Settled<P: ProtocolExtension> {
    Dormant,
    Opening,
    Resident(Arc<WireSession<P>>),
}

/// What a request came to: an outcome this connection writes, or the node
/// attachment's connection answered it itself.
enum Forwarded<P: ProtocolExtension> {
    Answered(Outcome<P>),
    Reading(bool),
}

mod operator;
mod reads;
pub use admission::{AdmissionPause, HostReadLimits};
pub use reads::{HostPrincipal, HostReadTask, HostRequestContext};
mod admission;
mod step_drain;
pub use step_drain::{HostStepDrain, StepDrainMonitor};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;

mod archived;

#[cfg(test)]
mod host_reads_tests;

/// A resident row's attention: an operator decision, or a running root turn
/// whose own placement's link or reconnect barrier is down. Parked or live
/// children alone do not raise it; a refused placement is not unreachable.
fn needs_operator(
    kept: &crate::host_service::KeptActivity,
    status: &plakous_host_protocol::wire::RunnerStatus,
) -> bool {
    kept.awaiting_operator
        || (kept.turn_busy
            && matches!(
                status,
                plakous_host_protocol::wire::RunnerStatus::Blocked {
                    reason: plakous_host_protocol::wire::PlacementReason::RunnerUnreachable { .. },
                }
            ))
}
