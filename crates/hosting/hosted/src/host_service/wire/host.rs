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
//! that changed, read again whenever the host's sessions change and at least
//! every [`DIRECTORY_POLL`] while it is open.

use std::collections::BTreeMap;
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
use super::{Face, WireConnection, WireSession, host_welcome, write_frame};
use crate::error::{CliFailure, CliResult};

/// How often an open host attachment reads the directory again when
/// nothing on the host changed: a session's title and its durable facts
/// change in its own stores.
pub const DIRECTORY_POLL: Duration = Duration::from_secs(1);

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

/// Persisted attach facts for an archived session, without runtime ownership.
pub struct ArchivedSessionView<P: ProtocolExtension> {
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
    /// Stable create-action UUID.
    pub request_id: &'a str,
}

/// What a host of many sessions needs of its product's stores.
pub trait HostSessions<P: ProtocolExtension>: Send + Sync {
    /// Decorate a saved runner identity without probing or changing its authority.
    fn runner_label(&self, id: &str) -> plakous_host_protocol::wire::RunnerLabel {
        plakous_host_protocol::wire::RunnerLabel {
            name: id.to_owned(),
            runner_id: id.to_owned(),
        }
    }

    /// The directory's rows as the stores show them, newest first: each one
    /// dormant, with the activity its durable facts give, and
    /// `hosted_elsewhere` only for a holder of its tree that is not proven
    /// gone.
    /// Apply workspace, pinned runner and archive visibility filters before
    /// the product's directory row limit. A nonlocal key is never host-canonicalized.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    fn rows(
        &self,
        workspace: Option<&str>,
        runner: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<SessionRow>, String>;

    /// The newest session of the tree `node` is in, or `None`.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    fn session_of(&self, node: &NodeAddress) -> Result<Option<String>, String>;

    /// Opens `session` on a host worker that releases it once idle.
    ///
    /// # Errors
    ///
    /// The session does not open.
    fn open(&self, session: &str) -> CliResult<OpenedSession<P>>;

    /// Carries the released session under its current root selection.
    ///
    /// # Errors
    /// The carry's validation or durable transition refused.
    fn rebind(&self, _session: &str, _cause: &str) -> CliResult<Created> {
        Err(CliFailure::new(
            "this launcher does not support binding repair",
        ))
    }

    /// Stops durable work without opening a dormant session.
    ///
    /// # Errors
    /// Authority, storage or unsettled runner effects refused the stop.
    fn stop_dormant(&self, _session: &str) -> CliResult<()> {
        Err(CliFailure::new("this launcher cannot stop dormant work"))
    }

    /// The refusal of an attach whose session did not open with `failure`:
    /// `tree_hosted_elsewhere` while another live host process holds its
    /// tree, `session_unavailable` with why otherwise.
    fn refusal(&self, session: &str, failure: &CliFailure) -> RequestRefusal;

    /// Creates the session a `new_session` asks for, or finds the one the
    /// same `request_id` created.
    ///
    /// # Errors
    ///
    /// The request's refusal: nothing was created.
    fn create(&self, request: &Creation<'_>) -> Result<Created, RequestRefusal>;

    /// Read archived attach facts without opening a runtime or taking a lease.
    ///
    /// # Errors
    /// Product metadata or transcript facts could not be read.
    fn archived_view(
        &self,
        _session: &str,
    ) -> Result<Option<ArchivedSessionView<P>>, RequestRefusal> {
        Ok(None)
    }

    /// Read the bounded placement picker without opening a session.
    ///
    /// # Errors
    /// Typed validation, transport or product refusal.
    fn options(
        &self,
        _workspace: Option<&plakous_host_protocol::wire::WorkspaceProbe>,
        _progress: &mut dyn FnMut(PickerProgress) -> Result<(), String>,
    ) -> Result<plakous_host_protocol::wire::SessionOptions, RequestRefusal> {
        Err(product_service_missing("session_options"))
    }

    /// Commit or clear the title override without opening a runtime.
    ///
    /// # Errors
    /// Typed product validation or write refusal.
    fn rename(
        &self,
        _session: &str,
        _title: Option<&str>,
    ) -> Result<Option<String>, RequestRefusal> {
        Err(product_service_missing("rename_session"))
    }

    /// Commit archive state under the product admission barrier.
    ///
    /// # Errors
    /// Typed lifecycle or write refusal.
    fn archive(&self, _session: &str, _archived: bool) -> Result<(), RequestRefusal> {
        Err(product_service_missing("archive_session"))
    }

    /// Run the guarded product deletion workflow after host release.
    ///
    /// # Errors
    /// Typed deletion fences or infrastructure refusal.
    fn delete(&self, _session: &str, _abandon_children: bool) -> Result<(), RequestRefusal> {
        Err(product_service_missing("delete_session"))
    }
}

fn product_service_missing(operation: &str) -> RequestRefusal {
    RequestRefusal::Unsupported {
        detail: format!("{operation} product service is not installed"),
    }
}

/// The sessions one host serves.
pub struct SessionHost<P: ProtocolExtension> {
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
    /// The directory last read, shared by every host attachment: when, at
    /// which host revision, and its rows.
    directory: Mutex<[Option<DirectoryRead>; 2]>,
}

/// One shared read of the directory: when, at which host revision, and its
/// rows.
type DirectoryRead = (Instant, u64, Arc<Vec<SessionRow>>);

/// Whether `slot` is `session`. A resident slot is the session it serves
/// now, which a switch can change while `known` still names the one it
/// opened; every other state is the id it names.
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
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            drafts: super::super::drafts::DraftRegistry::default(),
            sessions: Arc::new(sessions),
            slots: Mutex::new(Vec::new()),
            changes,
            next_host_attachment: AtomicU64::new(1),
            binary_id: binary_id.into(),
            generation,
            release_wait: RELEASE_WAIT,
            release_join: RELEASE_JOIN,
            closing: Arc::default(),
            directory: Mutex::new([None, None]),
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

    /// The whole directory, read at most once per host revision and per
    /// `DIRECTORY_POLL` (1 s), whichever comes first, and shared by every host
    /// attachment: a read in flight is waited for, not repeated.
    ///
    /// # Errors
    ///
    /// The stores cannot be read.
    pub fn directory(&self) -> Result<Arc<Vec<SessionRow>>, String> {
        self.directory_with_archived(false)
    }

    fn directory_with_archived(
        &self,
        include_archived: bool,
    ) -> Result<Arc<Vec<SessionRow>>, String> {
        let revision = *self.changes.borrow();
        let mut entries = self
            .directory
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let cached = &mut entries[usize::from(include_archived)];
        if let Some((read, at, rows)) = &*cached
            && *at == revision
            && read.elapsed() < DIRECTORY_POLL
        {
            return Ok(Arc::clone(rows));
        }
        let rows = Arc::new(self.rows(None, None, include_archived)?);
        *cached = Some((Instant::now(), revision, Arc::clone(&rows)));
        Ok(rows)
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
    pub fn welcome(&self) -> Welcome {
        host_welcome(self.binary_id.clone(), self.generation)
    }

    /// A new connection writing its frames to `out`, from a client its
    /// acceptor marked `face`; it ends once `stopped` turns true.
    pub fn connect<W: Write + Clone>(
        self: &Arc<Self>,
        out: W,
        face: Face,
        stopped: impl Fn() -> bool + Send + Sync + 'static,
    ) -> HostConnection<P, W> {
        HostConnection {
            host: Arc::clone(self),
            host_changes: self.changes.subscribe(),
            out,
            face,
            stopped: Arc::new(stopped),
            login: None,
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
        let held: BTreeMap<String, SlotView<P>> = self
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
                (id, view)
            })
            .collect();
        for row in &mut rows {
            match held.get(&row.session) {
                None | Some(SlotView::Dormant(_)) => {}
                Some(SlotView::Opening(_)) => row.residency = SessionResidency::Opening,
                Some(SlotView::Releasing(_)) => row.residency = SessionResidency::Releasing,
                Some(SlotView::Resident(session)) => {
                    row.residency = SessionResidency::Resident;
                    // This process holds the tree.
                    row.hosted_elsewhere = None;
                    let kept = session.kept_activity();
                    row.live_work.processes = u32::try_from(kept.processes).unwrap_or(u32::MAX);
                    row.live_work.children = u32::try_from(kept.children).unwrap_or(u32::MAX);
                    row.live_work.held_children =
                        u32::try_from(kept.held_children).unwrap_or(u32::MAX);
                    row.needs_operator = kept.awaiting_operator;
                    if kept.busy {
                        row.activity = SessionActivity::Busy;
                    }
                }
            }
        }
        Ok(rows)
    }

    /// The slot of the session whose tree `node` is in: the resident one
    /// serving that tree, or the newest session of that tree.
    fn resolve(&self, node: &NodeAddress) -> Result<Arc<SessionSlot<P>>, RequestRefusal> {
        let mut slots = self.lock();
        for slot in slots.iter() {
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
        Ok(Self::slot_in(
            &mut slots,
            &self.sessions,
            &self.closing,
            &session,
        ))
    }

    /// The slot of `session`, made dormant when the host has none yet.
    pub fn slot(&self, session: &str) -> Arc<SessionSlot<P>> {
        Self::slot_in(&mut self.lock(), &self.sessions, &self.closing, session)
    }

    fn slot_in(
        slots: &mut Vec<Arc<SessionSlot<P>>>,
        sessions: &Arc<dyn HostSessions<P>>,
        closing: &Arc<std::sync::atomic::AtomicBool>,
        session: &str,
    ) -> Arc<SessionSlot<P>> {
        if let Some(slot) = slots.iter().find(|slot| serves(slot, session)) {
            return Arc::clone(slot);
        }
        let (opener, closing) = (Arc::clone(sessions), Arc::clone(closing));
        let slot = Arc::new(SessionSlot::released(session, move |id: &str| {
            // A stopping host opens nothing its handoff would miss.
            if closing.load(Ordering::SeqCst) {
                return Err(CliFailure::new("the host is stopping"));
            }
            opener.open(id)
        }));
        slots.push(Arc::clone(&slot));
        slot
    }

    /// Every slot the host holds, resident or not.
    pub fn slots(&self) -> Vec<Arc<SessionSlot<P>>> {
        self.lock().clone()
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

    /// Stops the host, so no session opens or is created from here on, then
    /// releases every resident session as a planned handoff, all at once,
    /// each joined within `deadline`; the sessions that did not release in
    /// time, or failed to, with why.
    pub fn hand_off_all(&self, deadline: Duration) -> Vec<(String, CliFailure)> {
        // Closed first, so the slots below are every session this host holds.
        self.close();
        let slots = self.slots();
        std::thread::scope(|scope| {
            let handles: Vec<_> = slots
                .iter()
                .map(|slot| scope.spawn(move || (slot.known(), slot.hand_off(deadline))))
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| match handle.join() {
                    Ok((_, Ok(()))) => None,
                    Ok((session, Err(failure))) => Some((session, failure)),
                    Err(_) => Some((
                        String::new(),
                        CliFailure::new("a session's handoff panicked"),
                    )),
                })
                .collect()
        })
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
    connection: WireConnection<P, W>,
    _mailbox: plakous_host_protocol::IntentReceiver<P>,
}

/// A connection's host attachment: the rows it sent last.
struct Watch {
    include_archived: bool,
    attachment: AttachmentId,
    seq: u64,
    sent: Option<BTreeMap<String, SessionRow>>,
    read_at: Option<Instant>,
    dirty: bool,
}

/// One connection to a host of many sessions.
pub struct HostConnection<P: ProtocolExtension, W: Write> {
    host: Arc<SessionHost<P>>,
    host_changes: watch::Receiver<u64>,
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

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    P: 'static,
    P::Intent: Send,
    P::Observation: Send,
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    /// This connection, its transport having admitted `login`.
    #[must_use]
    pub fn identified(mut self, login: impl Into<String>) -> Self {
        self.login = Some(login.into());
        self
    }

    /// Answers one client line; `false` once the client has left.
    ///
    /// An attach of a dormant session opens it first, so this can block
    /// for as long as that open takes.
    ///
    /// # Errors
    ///
    /// A lost output.
    pub fn frame(&mut self, line: Result<String, WireError>) -> CliResult<bool> {
        let Ok(text) = line else {
            self.leave();
            return Ok(false);
        };
        let frame = match decode_line::<ClientFrame<P>>(&text) {
            // A second hello, a bye, or a line that is not a frame: the
            // client left.
            Ok(ClientFrame::Hello(_) | ClientFrame::Bye) | Err(_) => {
                self.leave();
                return Ok(false);
            }
            Ok(frame) => frame,
        };
        let ClientFrame::Request { id, op } = frame else {
            // A blob precedes the intent that names it, on the node
            // attachment; with none it is no one's.
            return if self.node.is_some() {
                self.forward(text)
            } else {
                Ok(true)
            };
        };
        match self.request(id, op, &text)? {
            Forwarded::Answered(outcome) => {
                write_frame(&mut self.out, &HostFrame::<P>::Response { id, outcome })?;
                Ok(true)
            }
            Forwarded::Reading(reading) => Ok(reading),
        }
    }

    fn request(
        &mut self,
        request_id: plakous_host_protocol::wire::RequestId,
        op: Op<P>,
        text: &str,
    ) -> CliResult<Forwarded> {
        let answered = |outcome| Ok(Forwarded::Answered(outcome));
        let refused = |refusal| Ok(Forwarded::Answered(Outcome::Refused(refusal)));
        if let Some(link) = &self.readonly
            && matches!(
                &op,
                Op::Intent { .. }
                    | Op::RetryAttach { .. }
                    | Op::ResumeSession { .. }
                    | Op::Rebind { .. }
            )
        {
            return refused(RequestRefusal::SessionArchived {
                session: link.session.clone(),
            });
        }
        match op {
            Op::Ping => answered(Outcome::Ok(Answer::Pong)),
            // A one-shot list is a fresh read, so it names a holder the
            // moment it holds; only a watch shares the cached read.
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
                if self.watch.is_some() {
                    return refused(RequestRefusal::OneInFlight);
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
                    read_at: None,
                    dirty: true,
                });
                answered(Outcome::Ok(Answer::Watching { attachment }))
            }
            Op::UnwatchSessions { attachment } => {
                if self.watch.as_ref().map(|watch| watch.attachment) != Some(attachment) {
                    return refused(RequestRefusal::NotAttached);
                }
                self.watch = None;
                answered(Outcome::Ok(Answer::Detached))
            }
            Op::NewSession { .. } if self.host.closed() => refused(stopping()),
            Op::NewSession {
                runner,
                profile,
                model,
                workspace,
                template,
                request_id,
            } => answered(self.create_session(&Creation {
                runner: &runner,
                workspace: &workspace,
                profile: profile.as_deref(),
                template: template.as_deref(),
                model: model.as_ref(),
                request_id: &request_id,
            })),
            Op::ReleaseSession { session } => answered(self.release(&session)?),
            Op::SessionOptions { workspace } => {
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
            } => answered(self.delete_session(session, abandon_children)),
            Op::StopSession { session } => self.stop(&session, text),
            Op::Attach { node } => self.attach(&node, text),
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

    fn reopen(&mut self, attachment: AttachmentId, request: &str, cause: Option<&str>) -> Outcome {
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

    fn stop(&mut self, session: &str, text: &str) -> CliResult<Forwarded> {
        let refused = |refusal| Ok(Forwarded::Answered(Outcome::Refused(refusal)));
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
            return self.forward(text.to_owned()).map(Forwarded::Reading);
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
                Ok(Forwarded::Answered(Outcome::Ok(Answer::Stopped)))
            }
            Ok(Some((session, hold, releases))) => {
                let stopped = Arc::clone(&self.stopped);
                let mut connection =
                    session.connect(self.out.clone(), self.face, move || stopped());
                if let Some(login) = &self.login {
                    connection = connection.identified(login.clone());
                }
                let reading = connection.frame(Ok(text.to_owned()))?;
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

    fn create_session(&self, creation: &Creation<'_>) -> Outcome {
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
    ) -> Outcome {
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

    fn rename_session(&self, session: String, title: Option<&str>) -> Outcome {
        match self.host.sessions.rename(&session, title) {
            Ok(title) => {
                self.host.changed();
                Outcome::Ok(Answer::Renamed { session, title })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }

    fn archive_session(&self, session: String, archived: bool) -> Outcome {
        match self.host.sessions.archive(&session, archived) {
            Ok(()) => {
                self.host.changed();
                Outcome::Ok(Answer::ArchiveUpdated { session, archived })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }

    fn delete_session(&self, session: String, abandon_children: bool) -> Outcome {
        match self.host.sessions.delete(&session, abandon_children) {
            Ok(()) => {
                self.host.changed();
                Outcome::Ok(Answer::Deleted { session })
            }
            Err(refusal) => Outcome::Refused(refusal),
        }
    }
    /// Attaches `node`: on the session the connection already serves when
    /// the node is in its tree, else on its session, opened first when it
    /// is dormant. The old node attachment ends once the new one is made,
    /// as a new attach on one session's connection ends the one before it;
    /// a refused attach leaves it.
    fn attach(&mut self, node: &NodeAddress, text: &str) -> CliResult<Forwarded> {
        if let Some(readonly) = self.archived_attachment(node, text)? {
            return Ok(readonly);
        }
        if self.node.as_ref().is_some_and(|link| {
            link.session.root().tree_id == node.tree_id && !link.session.ended()
        }) {
            return self.forward(text.to_owned()).map(Forwarded::Reading);
        }
        let slot = match self.host.resolve(node) {
            Ok(slot) => slot,
            Err(refusal) => return Ok(Forwarded::Answered(Outcome::Refused(refusal))),
        };
        self.host.changed();
        let resident = slot.resident_at_handoff();
        self.host.changed();
        let (session, hold, releases) = match resident {
            Ok(resident) => resident,
            Err(failure) => {
                let refusal = self.host.sessions.refusal(&slot.known(), &failure);
                return Ok(Forwarded::Answered(Outcome::Refused(refusal)));
            }
        };
        let stopped = Arc::clone(&self.stopped);
        let sessions = Arc::clone(&self.host.sessions);
        let mut connection = session
            .connect(self.out.clone(), self.face, move || stopped())
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
    fn release(&mut self, session: &str) -> CliResult<Outcome> {
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
        self.flush_draining()?;
        if let Some(link) = &mut self.readonly {
            link.connection.flush()?;
        }
        if let Some(link) = &mut self.node {
            // A session that ended, or one a `release_session` since the
            // link asked for, is no longer this connection's.
            link.connection.flush()?;
            if !link.connection.holding() {
                self.unlink();
            } else if !link.session.reopening()
                && (link.session.ended() || link.session.releases() > link.releases)
            {
                self.detach_link()?;
                self.host.changed();
            }
        }
        self.flush_directory()
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
        let changed = self.host_changes.has_changed().unwrap_or(false);
        if changed {
            self.host_changes.borrow_and_update();
        }
        let Some(watch) = &mut self.watch else {
            return Ok(());
        };
        let due = watch.dirty
            || changed
            || watch
                .read_at
                .is_none_or(|read| read.elapsed() >= DIRECTORY_POLL);
        if !due {
            return Ok(());
        }
        // A read that failed is a skipped tick: the next one reads again.
        let Ok(rows) = self.host.directory_with_archived(watch.include_archived) else {
            if let Some(watch) = &mut self.watch {
                watch.read_at = Some(Instant::now());
            }
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
        watch.read_at = Some(Instant::now());
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
    /// attachment's session published, the host's sessions changed, or the
    /// directory is due to be read again.
    pub async fn changed(&mut self) {
        let watching = self.watch.is_some();
        let host = &mut self.host_changes;
        let session = self.node.as_mut().map(|link| &mut link.changes);
        let draining = self.draining.as_mut().map(|link| &mut link.changes);
        let host_changed = tokio::select! {
            _ = host.changed() => true,
            () = session_changed(session) => false,
            () = session_changed(draining) => false,
            () = tokio::time::sleep(DIRECTORY_POLL), if watching => false,
        };
        // The wait saw the host's change: the directory is due.
        if host_changed && let Some(watch) = &mut self.watch {
            watch.dirty = true;
        }
    }

    /// The client left: its node attachment detaches, its host attachment
    /// closes, and its session hold ends.
    pub fn leave(&mut self) {
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
enum Forwarded {
    Answered(Outcome),
    Reading(bool),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;

mod archived;
