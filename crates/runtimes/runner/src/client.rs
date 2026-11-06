//! The host end: a [`RunnerLink`] that frames, keys and fences each call over
//! one runner connection, carries streams, watches the link's liveness, and
//! survives a reconnection.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use plakous_model::CancellationToken;
use plakous_runner_protocol::frame::FrameError;
use plakous_runner_protocol::{
    Bind, Hello, HostFrame, LineOwner, LineStatus, Operation, Refusal, Request, RequestClass,
    RunnerFrame, StreamEvent, StreamFrame, TransferOwner, VersionTriple,
};
use plakous_tools::ToolPreempt;
use plakous_tools::internal::runner::{
    RunnerCall, RunnerCallFuture, RunnerClass, RunnerFailure, RunnerLink, RunnerOpenFuture,
    RunnerStreams, RunnerVerb, RunnerVerbFamily, encode_preempt,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::connection::{FrameStream, fits, write_frames};
use crate::journal::{CallJournal, JournalEntry};
use crate::streams::{
    Credit, INITIAL_CREDIT, Outbound, StreamEnds, Window, receive_stream, send_stream,
};

/// How long the host waits on its runner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientWindows {
    /// How long a call that is not an effect call waits for its answer from
    /// when it was sent, across a lost link, before its outcome is unknown
    /// (spec 01 §8.4: 30 s).
    pub read: Duration,
    /// The same for an effect call (120 s).
    pub effect: Duration,
    /// How often the host pings the runner (5 s).
    pub ping: Duration,
    /// How long the link may stay silent before it is lost (15 s).
    pub dead: Duration,
}

impl Default for ClientWindows {
    fn default() -> Self {
        Self {
            read: Duration::from_secs(30),
            effect: Duration::from_secs(120),
            ping: Duration::from_secs(5),
            dead: Duration::from_secs(15),
        }
    }
}

/// Why a connection could not be bound.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConnectError {
    /// The stream ended before the runner's hello.
    Closed,
    /// The runner's first frame was not a hello.
    Malformed(String),
    /// The runner speaks another version triple.
    VersionMismatch {
        /// The runner's triple.
        runner: VersionTriple,
    },
    /// The keyed calls a previous host process left could not be read.
    Journal(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => {
                formatter.write_str("the runner closed the connection before its hello")
            }
            Self::Malformed(detail) => {
                write!(formatter, "the runner's hello is malformed: {detail}")
            }
            Self::VersionMismatch { runner } => write!(
                formatter,
                "the runner speaks protocol {} at {} ({})",
                runner.protocol, runner.source_revision, runner.flavor
            ),
            Self::Journal(detail) => {
                write!(
                    formatter,
                    "the host's runner call journal is unreadable: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for ConnectError {}

/// One host's link to one runner.
#[derive(Clone)]
pub struct RunnerClient {
    inner: Arc<Inner>,
}

struct Inner {
    bind: Bind,
    windows: ClientWindows,
    /// Bound to this home's home-local runner (spec 01 §8.4): any build of
    /// the same protocol version is bound, and only process-session calls
    /// are sent.
    home_local: bool,
    /// Where keyed calls are recorded before they are sent.
    journal: Option<Arc<dyn CallJournal>>,
    /// The calls a previous host process left in the journal, until
    /// [`RunnerClient::recover`] replays them.
    left: Mutex<Vec<JournalEntry>>,
    /// Unique to this client, so its request and control ids never collide
    /// with another host process's under the same host id.
    ids: String,
    next: AtomicU64,
    state: Mutex<State>,
    transferred: Mutex<Transferred>,
}

struct State {
    sender: Option<mpsc::UnboundedSender<HostFrame>>,
    connection: u64,
    /// Ends the current connection: a newer one, or a silent link.
    closer: CancellationToken,
    last_heard: Instant,
    instance: String,
    pending: HashMap<String, Pending>,
    transfers: HashMap<String, oneshot::Sender<RunnerFrame>>,
    questions: HashMap<String, oneshot::Sender<Vec<LineStatus>>>,
    /// Why the runner refused the connection itself, such as an unpaired
    /// host.
    refusal: Option<Refusal>,
    streams: HashMap<u64, StreamEnds>,
}

struct Pending {
    request: Request,
    answer: oneshot::Sender<Result<Value, RunnerFailure>>,
    /// Recorded in the journal, so its answer settles the record.
    journaled: bool,
}

/// A keyed call a previous host process left unsettled, and what replaying
/// it established.
#[derive(Debug)]
pub struct RecoveredCall {
    /// The call as first sent.
    pub request: Request,
    /// Its answer: what it returned, or why its outcome is known not to have
    /// happened ([`RunnerFailure::Unavailable`]: the runner never saw it, so
    /// it never will run) or is unknown ([`RunnerFailure::OutcomeUnknown`]).
    pub answer: Result<Value, RunnerFailure>,
}

/// The owner last transferred to the current runner instance, per line. Each
/// line's lock is held across its transfer so two calls of one new owner
/// transfer once, without making other lines wait. A new instance has
/// accepted no owner.
#[derive(Default)]
struct Transferred {
    instance: String,
    lines: HashMap<String, Arc<tokio::sync::Mutex<Option<LineOwner>>>>,
}

impl RunnerClient {
    /// Reads the runner's hello from `stream`, binds as `bind`, and starts
    /// answering calls.
    ///
    /// # Errors
    ///
    /// Refuses a stream without a hello, or a runner of another version.
    pub async fn connect<S>(
        stream: S,
        bind: Bind,
        windows: ClientWindows,
    ) -> Result<(Self, Hello), ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::connect_journaled(stream, bind, windows, None).await
    }

    /// [`Self::connect`], recording every keyed call in `journal` before it
    /// is sent. Calls a previous host process left there are replayed by
    /// [`Self::recover`].
    ///
    /// # Errors
    ///
    /// As [`Self::connect`], and a journal that cannot be read.
    pub async fn connect_journaled<S>(
        stream: S,
        bind: Bind,
        windows: ClientWindows,
        journal: Option<Arc<dyn CallJournal>>,
    ) -> Result<(Self, Hello), ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::connect_with(stream, bind, windows, journal, false).await
    }

    /// [`Self::connect_journaled`] to this home's home-local runner: a
    /// runner of the bind's protocol version is bound whatever its build's
    /// revision or flavor, and only process-session calls are sent; any
    /// other call is refused here, with no effect.
    ///
    /// # Errors
    ///
    /// As [`Self::connect_journaled`], with a runner of another protocol
    /// version.
    pub async fn connect_home_local<S>(
        stream: S,
        bind: Bind,
        windows: ClientWindows,
        journal: Option<Arc<dyn CallJournal>>,
    ) -> Result<(Self, Hello), ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self::connect_with(stream, bind, windows, journal, true).await
    }

    async fn connect_with<S>(
        stream: S,
        bind: Bind,
        windows: ClientWindows,
        journal: Option<Arc<dyn CallJournal>>,
        home_local: bool,
    ) -> Result<(Self, Hello), ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let left = match &journal {
            Some(journal) => journal
                .pending()
                .map_err(|error| ConnectError::Journal(error.to_string()))?,
            None => Vec::new(),
        };
        let client = Self {
            inner: Arc::new(Inner {
                bind,
                windows,
                home_local,
                journal,
                left: Mutex::new(left),
                ids: uuid::Uuid::new_v4().simple().to_string(),
                next: AtomicU64::new(0),
                state: Mutex::new(State {
                    sender: None,
                    connection: 0,
                    closer: CancellationToken::new(),
                    last_heard: Instant::now(),
                    instance: String::new(),
                    pending: HashMap::new(),
                    transfers: HashMap::new(),
                    questions: HashMap::new(),
                    refusal: None,
                    streams: HashMap::new(),
                }),
                transferred: Mutex::default(),
            }),
        };
        let hello = client.attach(stream, false).await?;
        Ok((client, hello))
    }

    /// Binds a new connection to the same runner. Calls still waiting are
    /// replayed by key and digest when the runner is the same instance; when
    /// it restarted, their outcome is unknown and every line is transferred
    /// again before its next call. Streams do not survive a lost link.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub async fn reconnect<S>(&self, stream: S) -> Result<Hello, ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        self.attach(stream, true).await
    }

    /// Why the runner refused this host's connection, when it did: the
    /// connection then ends.
    #[must_use]
    pub fn refusal(&self) -> Option<Refusal> {
        lock(&self.inner.state).refusal.clone()
    }

    /// The runner instance this client is bound to now.
    #[must_use]
    pub fn instance(&self) -> String {
        lock(&self.inner.state).instance.clone()
    }

    /// Replays every keyed call a previous host process left in the journal
    /// and answers what each established. A call dispatched to another
    /// runner instance is not replayed: its outcome is unknown. Each answered
    /// call is settled in the journal.
    ///
    /// The journal is the home's, shared by its processes, and the runner
    /// answers a line's keys only to the line's holder. So only the process
    /// that serves the lines recovers, after transferring each to its newer
    /// owner; a call refused because another instance holds its line stays
    /// journaled for that holder's recovery.
    pub async fn recover(&self) -> Vec<RecoveredCall> {
        let left = std::mem::take(&mut *lock(&self.inner.left));
        let instance = self.instance();
        let mut recovered = Vec::with_capacity(left.len());
        let mut kept = Vec::new();
        for entry in left {
            let answer = if entry.dispatched_to == instance {
                self.inner.replay(&entry).await
            } else {
                Err(RunnerFailure::OutcomeUnknown(
                    "the runner restarted since the call was sent".into(),
                ))
            };
            // A replay the link lost, or one another holder must make, stays
            // recorded for the next recovery.
            let held_elsewhere = matches!(answer, Err(RunnerFailure::NotHolder));
            let answered = !held_elsewhere
                && !matches!(&answer, Err(RunnerFailure::OutcomeUnknown(reason)) if reason == LINK_LOST);
            if held_elsewhere {
                kept.push(entry.clone());
            }
            if answered && let Some(journal) = &self.inner.journal {
                let _ = journal.settle(&entry.request.request_id);
            }
            recovered.push(RecoveredCall {
                request: entry.request,
                answer,
            });
        }
        lock(&self.inner.left).extend(kept);
        recovered
    }

    /// The runner's lines of this host: each line's owner, whether that
    /// owner holds it, and its jobs.
    ///
    /// # Errors
    ///
    /// The runner is unreachable or did not answer within the read window.
    pub async fn lines(&self) -> Result<Vec<LineStatus>, RunnerFailure> {
        let request_id = self.inner.next_id("q");
        let (waiter, answer) = oneshot::channel();
        lock(&self.inner.state)
            .questions
            .insert(request_id.clone(), waiter);
        if !self.inner.send(HostFrame::Lines {
            request_id: request_id.clone(),
        }) {
            lock(&self.inner.state).questions.remove(&request_id);
            return Err(RunnerFailure::Unavailable(
                "the runner is not connected".into(),
            ));
        }
        if let Ok(Ok(lines)) = tokio::time::timeout(self.inner.windows.read, answer).await {
            return Ok(lines);
        }
        lock(&self.inner.state).questions.remove(&request_id);
        Err(RunnerFailure::Unavailable(
            "the runner did not list its lines".into(),
        ))
    }

    /// Makes `owner` the line's owner on the runner before its first call
    /// (spec 01 §8.4): a no-op when it already is.
    ///
    /// # Errors
    ///
    /// The runner accepts a newer owner of the line, refused to record the
    /// change, or did not answer.
    pub async fn transfer_owner(
        &self,
        owner: &plakous_tools::LineOwner,
    ) -> Result<(), RunnerFailure> {
        self.inner.transfer(&wire_owner(owner)).await
    }

    async fn attach<S>(&self, stream: S, again: bool) -> Result<Hello, ConnectError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let mut reader = FrameStream::new(reader);
        let hello = match reader.next::<RunnerFrame>().await {
            None => return Err(ConnectError::Closed),
            Some(Ok(RunnerFrame::Hello(hello))) => hello,
            Some(Ok(_)) => return Err(ConnectError::Malformed("not a hello".into())),
            Some(Err(error)) => return Err(ConnectError::Malformed(error.to_string())),
        };
        let bound = if self.inner.home_local {
            hello.version.protocol == self.inner.bind.version.protocol
        } else {
            hello.version == self.inner.bind.version
        };
        if !bound {
            return Err(ConnectError::VersionMismatch {
                runner: hello.version,
            });
        }
        let (sender, frames) = mpsc::unbounded_channel();
        let closer = CancellationToken::new();
        tokio::spawn(write_frames(writer, frames, closer.clone()));
        let _ = sender.send(HostFrame::Bind(self.inner.bind.clone()));
        let connection = {
            let mut state = lock(&self.inner.state);
            let restarted = again && state.instance != hello.instance;
            state.refusal = None;
            state.closer.cancel();
            state.lose(|request| restarted || !replayable(request), restarted);
            state.instance.clone_from(&hello.instance);
            state.connection += 1;
            state.closer = closer.clone();
            state.last_heard = Instant::now();
            for pending in state.pending.values() {
                let mut request = pending.request.clone();
                request.replay = true;
                let _ = sender.send(HostFrame::Request(request));
            }
            state.sender = Some(sender);
            state.connection
        };
        // A runner forgets a line's holder once its owner lease runs out, so
        // after a new connection every line is transferred again before its
        // next call.
        if again {
            *lock(&self.inner.transferred) = Transferred::default();
        }
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.read(reader, connection, closer).await });
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.ping(connection).await });
        Ok(hello)
    }
}

impl State {
    /// Ends the current connection's transfers and streams, and fails each
    /// pending call `lost` selects as of unknown outcome.
    fn lose(&mut self, lost: impl Fn(&Request) -> bool, restarted: bool) {
        self.transfers.clear();
        self.questions.clear();
        for (_, mut stream) in self.streams.drain() {
            stream.close();
        }
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| lost(&pending.request))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(pending) = self.pending.remove(&id) {
                let _ = pending
                    .answer
                    .send(Err(RunnerFailure::OutcomeUnknown(if restarted {
                        "the runner restarted with the call outstanding".into()
                    } else {
                        "the link was lost with the call outstanding".into()
                    })));
            }
        }
    }
}

impl Inner {
    async fn read<R: AsyncRead + Unpin>(
        &self,
        mut reader: FrameStream<R>,
        connection: u64,
        closer: CancellationToken,
    ) {
        loop {
            let frame = tokio::select! {
                frame = reader.next::<RunnerFrame>() => frame,
                () = closer.cancelled() => break,
            };
            match frame {
                // An oversized prefix leaves the stream unframed: the link is
                // lost.
                None | Some(Err(FrameError::TooLarge)) => break,
                Some(Ok(frame)) => {
                    lock(&self.state).last_heard = Instant::now();
                    self.receive(frame);
                }
                Some(Err(_)) => {}
            }
        }
        closer.cancel();
        let mut state = lock(&self.state);
        if state.connection != connection {
            return;
        }
        state.sender = None;
        // Only keyed calls, observations and reads are replayed on a
        // reconnection: an unkeyed mutation or process may have run, and has
        // no key to ask by.
        state.lose(|request| !replayable(request), false);
    }

    /// Pings the runner while `connection` is current, and loses the link
    /// once it was silent for the dead window.
    async fn ping(&self, connection: u64) {
        let mut nonce = 0_u64;
        loop {
            tokio::time::sleep(self.windows.ping).await;
            let (silent, closer) = {
                let state = lock(&self.state);
                if state.connection != connection || state.sender.is_none() {
                    return;
                }
                (state.last_heard.elapsed(), state.closer.clone())
            };
            if silent >= self.windows.dead {
                closer.cancel();
                return;
            }
            nonce += 1;
            self.send(HostFrame::Ping { nonce });
        }
    }

    fn receive(&self, frame: RunnerFrame) {
        let control = match &frame {
            RunnerFrame::TransferAccepted { control_id }
            | RunnerFrame::TransferRefused { control_id, .. }
            | RunnerFrame::StaleOwner {
                control_id: Some(control_id),
                ..
            } => Some(control_id.clone()),
            _ => None,
        };
        if let Some(control) = control {
            if let Some(waiter) = lock(&self.state).transfers.remove(&control) {
                let _ = waiter.send(frame);
            }
            return;
        }
        let (request_id, answer) = match frame {
            RunnerFrame::Refused {
                request_id: None,
                refusal,
            } => {
                // These end the connection before any request of it ran; a
                // malformed frame on a bound connection ends nothing.
                if ends_connection(&refusal) {
                    self.refused(refusal);
                } else {
                    lock(&self.state).refusal = Some(refusal);
                }
                return;
            }
            RunnerFrame::Lines { request_id, lines } => {
                if let Some(waiter) = lock(&self.state).questions.remove(&request_id) {
                    let _ = waiter.send(lines);
                }
                return;
            }
            RunnerFrame::Stream(frame) => {
                // Taken out of the state while it runs: an abort sends a
                // close, and sending takes the state's lock.
                let stream = frame.stream;
                let Some(mut ends) = lock(&self.state).streams.remove(&stream) else {
                    return;
                };
                if ends.receive(frame) {
                    let mut state = lock(&self.state);
                    if state.sender.is_some() {
                        state.streams.insert(stream, ends);
                    } else {
                        ends.close();
                    }
                }
                return;
            }
            other => {
                let Some((request_id, answer, acknowledge)) = request_answer(other) else {
                    return;
                };
                // The answer is recorded below, so the runner may forget it.
                if acknowledge {
                    self.send(HostFrame::Ack {
                        request_id: request_id.clone(),
                    });
                }
                (request_id, answer)
            }
        };
        let Some(pending) = lock(&self.state).pending.remove(&request_id) else {
            return;
        };
        if pending.journaled
            && let Some(journal) = &self.journal
        {
            let _ = journal.settle(&pending.request.request_id);
        }
        let _ = pending.answer.send(answer);
    }

    /// The runner refused the connection itself (an unpaired host, an
    /// inspection asked for more): nothing this link sent ran, so every
    /// waiting call and transfer fails now with no effect, and the link
    /// takes no new call.
    fn refused(&self, refusal: Refusal) {
        let reason = format!("the runner refused this host: {refusal:?}");
        let mut state = lock(&self.state);
        state.refusal = Some(refusal);
        state.sender = None;
        state.closer.cancel();
        state.transfers.clear();
        state.questions.clear();
        for (_, mut stream) in state.streams.drain() {
            stream.close();
        }
        let pending: Vec<Pending> = state.pending.drain().map(|(_, pending)| pending).collect();
        drop(state);
        for pending in pending {
            if pending.journaled
                && let Some(journal) = &self.journal
            {
                let _ = journal.settle(&pending.request.request_id);
            }
            let _ = pending
                .answer
                .send(Err(RunnerFailure::Unavailable(reason.clone())));
        }
    }

    fn send(&self, frame: HostFrame) -> bool {
        lock(&self.state)
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(frame).is_ok())
    }

    fn next_id(&self, prefix: &str) -> String {
        format!(
            "{}.{prefix}{}",
            self.ids,
            self.next.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Sends `owner`'s transfer before its first call, unless it is the
    /// owner last transferred for its line. A transfer from an owner the
    /// runner no longer accepts is answered with the one it does; a newer
    /// `owner` (a fresher lease) transfers again from that one, once.
    async fn transfer(&self, owner: &LineOwner) -> Result<(), RunnerFailure> {
        let line = {
            let instance = lock(&self.state).instance.clone();
            let mut transferred = lock(&self.transferred);
            if transferred.instance != instance {
                *transferred = Transferred {
                    instance,
                    lines: HashMap::new(),
                };
            }
            Arc::clone(transferred.lines.entry(owner.line.clone()).or_default())
        };
        let mut transferred = line.lock().await;
        if transferred.as_ref() == Some(owner) {
            return Ok(());
        }
        let mut from = transferred.clone();
        for _ in 0..2 {
            match self.send_transfer(from.clone(), owner).await? {
                RunnerFrame::TransferAccepted { .. } => {
                    *transferred = Some(owner.clone());
                    return Ok(());
                }
                RunnerFrame::StaleOwner { current, .. }
                    if owner.is_newer_than(&current) && from.as_ref() != Some(&current) =>
                {
                    from = Some(current);
                }
                RunnerFrame::TransferRefused { detail, .. } => {
                    return Err(RunnerFailure::Unavailable(detail));
                }
                _ => return Err(RunnerFailure::StaleOwner),
            }
        }
        Err(RunnerFailure::StaleOwner)
    }

    /// Sends one transfer and waits for its answer.
    async fn send_transfer(
        &self,
        from: Option<LineOwner>,
        to: &LineOwner,
    ) -> Result<RunnerFrame, RunnerFailure> {
        let control_id = self.next_id("t");
        let (waiter, answer) = oneshot::channel();
        lock(&self.state)
            .transfers
            .insert(control_id.clone(), waiter);
        let sent = self.send(HostFrame::TransferOwner(TransferOwner {
            control_id: control_id.clone(),
            from,
            to: to.clone(),
        }));
        if !sent {
            lock(&self.state).transfers.remove(&control_id);
            return Err(RunnerFailure::Unavailable(
                "the runner is not connected".into(),
            ));
        }
        if let Ok(Ok(frame)) = tokio::time::timeout(self.windows.read, answer).await {
            return Ok(frame);
        }
        lock(&self.state).transfers.remove(&control_id);
        Err(RunnerFailure::Unavailable(
            "the runner did not answer the owner transfer".into(),
        ))
    }

    /// Replays one call a previous host process left, and waits for its
    /// answer within the call's window.
    async fn replay(&self, entry: &JournalEntry) -> Result<Value, RunnerFailure> {
        let mut request = entry.request.clone();
        request.request_id = self.next_id("r");
        request.replay = true;
        request.age_ms = Some(plakous_model::now_unix_millis().saturating_sub(entry.claimed_at_ms));
        let window = if matches!(
            request.class,
            RequestClass::EffectCall { .. } | RequestClass::TreeOperation { .. }
        ) {
            self.windows.effect
        } else {
            self.windows.read
        };
        let request_id = request.request_id.clone();
        let (answer, answered) = oneshot::channel();
        {
            let mut state = lock(&self.state);
            let Some(sender) = state.sender.clone() else {
                return Err(RunnerFailure::OutcomeUnknown(LINK_LOST.into()));
            };
            let frame = HostFrame::Request(request.clone());
            state.pending.insert(
                request_id.clone(),
                Pending {
                    request,
                    answer,
                    journaled: false,
                },
            );
            if sender.send(frame).is_err() {
                state.pending.remove(&request_id);
                return Err(RunnerFailure::OutcomeUnknown(LINK_LOST.into()));
            }
        }
        if let Ok(Ok(answer)) = tokio::time::timeout(window, answered).await {
            return answer;
        }
        lock(&self.state).pending.remove(&request_id);
        Err(RunnerFailure::OutcomeUnknown(LINK_LOST.into()))
    }

    fn request(&self, call: &RunnerCall, owner: Option<LineOwner>) -> Request {
        let operation = Operation {
            verb: call.verb().as_str().to_owned(),
            payload: call.payload().clone(),
        };
        Request {
            request_id: self.next_id("r"),
            class: match call.class() {
                RunnerClass::Effect {
                    call_id,
                    attempt_id,
                } => RequestClass::EffectCall {
                    call_id: call_id.to_string(),
                    attempt_id: attempt_id.to_string(),
                },
                RunnerClass::Observation { job, cursor } => RequestClass::ProcessObservation {
                    job: *job,
                    cursor: *cursor,
                },
                RunnerClass::Control => RequestClass::OwnerControl {
                    control_id: self.next_id("c"),
                },
                RunnerClass::RecordedControl(control_id) => RequestClass::OwnerControl {
                    control_id: control_id.clone(),
                },
                RunnerClass::Tree {
                    tree_id,
                    operation_id,
                } => RequestClass::TreeOperation {
                    tree_id: tree_id.clone(),
                    operation_id: operation_id.clone(),
                },
                RunnerClass::Unkeyed => RequestClass::BuildRead,
            },
            owner,
            deadline_ms: call.budget().map(|deadline| {
                deadline
                    .seconds()
                    .saturating_mul(1000)
                    .saturating_add(u64::from(deadline.nanoseconds() / 1_000_000))
                    .saturating_sub(plakous_model::now_unix_millis())
            }),
            digest: digest(&operation),
            replay: false,
            age_ms: None,
            operation,
        }
    }

    async fn call(&self, call: RunnerCall) -> Result<Value, RunnerFailure> {
        if self.home_local && call.verb().family() != RunnerVerbFamily::Sessions {
            return Err(RunnerFailure::Unavailable(
                "the home-local runner carries only process sessions".into(),
            ));
        }
        let owner = call.owner().map(wire_owner);
        if let Some(owner) = &owner {
            self.transfer(owner).await?;
        }
        let request = self.request(&call, owner);
        let window = if matches!(
            request.class,
            RequestClass::EffectCall { .. } | RequestClass::TreeOperation { .. }
        ) {
            self.windows.effect
        } else {
            self.windows.read
        };
        let frame = HostFrame::Request(request.clone());
        if !fits(&frame) {
            return Err(RunnerFailure::Refused(
                "the call exceeds the runner frame limit".into(),
            ));
        }
        let request_id = request.request_id.clone();
        let (answer, mut answered) = oneshot::channel();
        self.dispatch(request, frame, answer)?;
        let expiry = tokio::time::sleep(window);
        tokio::pin!(expiry);
        let cancellation = call.cancellation().clone();
        let mut cancel_sent = false;
        let mut preempt = call.preempt().clone();
        let mut yields = true;
        let initial = *preempt.borrow_and_update();
        if initial != ToolPreempt::None {
            self.send_preempt(&request_id, initial);
        }
        loop {
            tokio::select! {
                answer = &mut answered => {
                    return answer.unwrap_or_else(|_| Err(RunnerFailure::OutcomeUnknown(
                        "the call was abandoned".into(),
                    )));
                }
                () = cancellation.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    if !self.send(HostFrame::Cancel { request_id: request_id.clone() }) {
                        lock(&self.state).pending.remove(&request_id);
                        return Err(RunnerFailure::OutcomeUnknown(
                            "the call was cancelled while the runner was unreachable".into(),
                        ));
                    }
                }
                changed = preempt.changed(), if yields => {
                    yields = changed.is_ok();
                    if yields {
                        let value = *preempt.borrow_and_update();
                        self.send_preempt(&request_id, value);
                    }
                }
                () = &mut expiry => {
                    lock(&self.state).pending.remove(&request_id);
                    return Err(RunnerFailure::OutcomeUnknown(
                        "the runner did not answer within the call's window".into(),
                    ));
                }
            }
        }
    }

    /// Sends one request, recorded as pending its answer. A keyed call is
    /// journaled first, with the runner instance it goes to, so a restarted
    /// host replays it rather than guessing its outcome.
    fn dispatch(
        &self,
        request: Request,
        frame: HostFrame,
        answer: oneshot::Sender<Result<Value, RunnerFailure>>,
    ) -> Result<(), RunnerFailure> {
        let request_id = request.request_id.clone();
        let (sender, instance, refusal) = {
            let state = lock(&self.state);
            (
                state.sender.clone(),
                state.instance.clone(),
                state.refusal.clone().filter(ends_connection),
            )
        };
        let not_connected = || {
            RunnerFailure::Unavailable(refusal.as_ref().map_or_else(
                || "the runner is not connected".to_owned(),
                |refusal| format!("the runner refused this host: {refusal:?}"),
            ))
        };
        let sender = sender.ok_or_else(not_connected)?;
        let journaled = match (&self.journal, keyed(&request)) {
            (Some(journal), true) => {
                journal
                    .claim(&JournalEntry {
                        request: request.clone(),
                        claimed_at_ms: plakous_model::now_unix_millis(),
                        dispatched_to: instance,
                    })
                    .map_err(|error| {
                        RunnerFailure::Unavailable(format!(
                            "the call could not be recorded before it was sent: {error}"
                        ))
                    })?;
                true
            }
            _ => false,
        };
        let mut state = lock(&self.state);
        state.pending.insert(
            request_id.clone(),
            Pending {
                request,
                answer,
                journaled,
            },
        );
        if sender.send(frame).is_err() {
            state.pending.remove(&request_id);
            drop(state);
            self.unclaim(journaled, &request_id);
            return Err(not_connected());
        }
        Ok(())
    }

    /// Forgets a claimed call that was never sent.
    fn unclaim(&self, journaled: bool, request_id: &str) {
        if journaled && let Some(journal) = &self.journal {
            let _ = journal.settle(request_id);
        }
    }

    fn send_preempt(&self, request_id: &str, value: ToolPreempt) {
        self.send(HostFrame::Preempt {
            request_id: request_id.to_owned(),
            value: encode_preempt(value),
        });
    }

    /// Makes the call, then joins the stream its result names: the host
    /// grants the runner its first credit only once it holds the stream.
    async fn open(
        self: Arc<Self>,
        call: RunnerCall,
    ) -> Result<(Value, RunnerStreams), RunnerFailure> {
        let value = self.call(call).await?;
        let Some(stream) = value.get("stream").and_then(Value::as_u64) else {
            return Err(RunnerFailure::Refused(
                "the runner's answer names no stream".into(),
            ));
        };
        let (input, input_pump) = tokio::io::duplex(64 * 1024);
        let (output_pump, output) = tokio::io::duplex(64 * 1024);
        let (chunks, received) = mpsc::unbounded_channel();
        let credit = Credit::new(INITIAL_CREDIT);
        let window = Window::new(INITIAL_CREDIT);
        let weak = Arc::downgrade(&self);
        let out = Outbound::new(stream, move |frame: StreamFrame| {
            weak.upgrade()
                .is_some_and(|inner| inner.send(HostFrame::Stream(frame)))
        });
        {
            let mut state = lock(&self.state);
            if state.sender.is_none() {
                return Err(RunnerFailure::OutcomeUnknown(
                    "the link was lost as the stream opened".into(),
                ));
            }
            state.streams.insert(
                stream,
                StreamEnds::new(
                    chunks,
                    Arc::clone(&credit),
                    Arc::clone(&window),
                    Arc::clone(&out),
                ),
            );
        }
        tokio::spawn(send_stream(input_pump, credit, Arc::clone(&out)));
        out.send(StreamEvent::Credit {
            bytes: INITIAL_CREDIT,
        });
        tokio::spawn(receive_stream(output_pump, received, window, out));
        Ok((
            value,
            RunnerStreams {
                input: Box::pin(input),
                output: Box::pin(output),
            },
        ))
    }
}

impl RunnerLink for RunnerClient {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.call(call).await })
    }

    fn open(&self, call: RunnerCall) -> RunnerOpenFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(inner.open(call))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The request a runner frame answers, its answer, and whether the runner
/// keeps that answer until it is acknowledged. `InProgress` (the answer
/// still comes), `Pong` and frames without a request id answer nothing.
fn request_answer(frame: RunnerFrame) -> Option<(String, Result<Value, RunnerFailure>, bool)> {
    Some(match frame {
        RunnerFrame::Result {
            request_id,
            payload,
        } => (request_id, Ok(payload), true),
        RunnerFrame::Error { request_id, error } => {
            (request_id, Err(RunnerFailure::Backend(error.error)), true)
        }
        RunnerFrame::NotSeen { request_id } => (
            request_id,
            Err(RunnerFailure::Unavailable(
                "the runner never saw the call; nothing ran".into(),
            )),
            false,
        ),
        RunnerFrame::Spent { request_id } => (request_id, Err(RunnerFailure::Spent), false),
        RunnerFrame::Expired { request_id } => (
            request_id,
            Err(RunnerFailure::OutcomeUnknown(
                "the call is older than the runner keeps answers, so whether it ran is unknown"
                    .into(),
            )),
            false,
        ),
        RunnerFrame::StaleRequest { request_id } => (
            request_id,
            Err(RunnerFailure::Refused(
                "the runner holds another request under this call's key".into(),
            )),
            false,
        ),
        RunnerFrame::StaleOwner {
            request_id: Some(request_id),
            ..
        } => (request_id, Err(RunnerFailure::StaleOwner), false),
        RunnerFrame::Refused {
            request_id: Some(request_id),
            refusal,
        } => (
            request_id,
            Err(match refusal {
                Refusal::Unsupported { .. } => RunnerFailure::Unsupported,
                Refusal::OwnerNotTransferred => RunnerFailure::NotHolder,
                other => RunnerFailure::Refused(format!("{other:?}")),
            }),
            false,
        ),
        _ => return None,
    })
}

/// Whether the runner refuses the connection itself with `refusal`, before
/// any request of it ran.
const fn ends_connection(refusal: &Refusal) -> bool {
    matches!(
        refusal,
        Refusal::Unbound
            | Refusal::VersionMismatch { .. }
            | Refusal::NotPaired { .. }
            | Refusal::InspectOnly
    )
}

/// How a replay or recovery says the link was lost before an answer came.
const LINK_LOST: &str = "the link was lost before the replay was answered";

/// Whether a request runs at most once per key: an effect call or an owner
/// control. These are the calls the journal records.
const fn keyed(request: &Request) -> bool {
    matches!(
        request.class,
        RequestClass::EffectCall { .. }
            | RequestClass::OwnerControl { .. }
            | RequestClass::TreeOperation { .. }
    )
}

/// Whether a request outstanding at a lost link may be sent again: a keyed
/// call or control, an observation, or a read.
fn replayable(request: &Request) -> bool {
    matches!(
        request.class,
        RequestClass::EffectCall { .. }
            | RequestClass::OwnerControl { .. }
            | RequestClass::TreeOperation { .. }
            | RequestClass::ProcessObservation { .. }
    ) || RunnerVerb::parse(&request.operation.verb).is_some_and(RunnerVerb::is_read)
}

fn wire_owner(owner: &plakous_tools::LineOwner) -> LineOwner {
    LineOwner {
        line: owner.line().to_string(),
        generation: owner.generation(),
        branch: owner.branch().to_string(),
        fence: owner.fence(),
    }
}

/// Lowercase hex SHA-256 of the operation's canonical (RFC 8785) JSON.
pub(crate) fn digest(operation: &Operation) -> String {
    use std::fmt::Write as _;
    let bytes = serde_json_canonicalizer::to_vec(operation).unwrap_or_default();
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(&bytes) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}
