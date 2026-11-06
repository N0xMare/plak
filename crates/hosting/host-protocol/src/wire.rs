//! Wire v2: the framed protocol every frontend speaks to a host
//! (spec 02 §4.4). It replaces the v1 JSONL [`crate::envelope`] at D2-D3;
//! until then both exist.
//!
//! # Framing
//!
//! One JSON object per line (NDJSON) in both directions, at most
//! [`MAX_LINE_BYTES`] including the newline. A line over the cap is refused
//! before it is held in full. Every object carries `frame`, its kind.
//! Anything too large for one line travels in parts: image bytes as
//! [`BlobChunk`]s of at most [`BLOB_CHUNK_BYTES`], referenced by id from
//! the intent or observation that carries them ([`take_images`],
//! [`BlobAssembler`]), and presentation snapshots as [`SnapshotPage`]s of
//! one revision ([`page_snapshot`], [`SnapshotAssembler`]).
//!
//! # Handshake
//!
//! The client's first frame is [`Hello`]. The host answers [`Welcome`] or
//! [`HostFrame::Refused`] and closes; the protocol version must match
//! exactly ([`admit_hello`]). Nothing else is read before the handshake.
//!
//! # Requests and events
//!
//! A [`ClientFrame::Request`] gets exactly one [`HostFrame::Response`] with
//! its id: an answer, or a typed [`RequestRefusal`]. An intent's answer is
//! [`Answer::Accepted`], admission into the node's mailbox; what the intent
//! does comes back as observations. Observations arrive as
//! [`HostFrame::Event`]s on an attachment with the attachment's sequence
//! number and their [`ObservationClass`]. A reconnect is a fresh attach:
//! there is no replay, and the attach snapshot carries every state.

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::intent::{Draft, ProtocolExtension, SessionIntent};
use crate::observation::{ObservationClass, SessionObservation};
use crate::views::PresentationSnapshot;
use crate::views::transcript::{TranscriptItem, TranscriptScope};

mod directory;
pub use directory::{
    HostedElsewhere, LiveWork, ResidentReason, SessionActivity, SessionEnvironment,
    SessionResidency, SessionRow,
};

/// The wire protocol version; a peer with another is refused. 3 dropped
/// `hello`'s locality; 4 lets a welcome name no session and adds the
/// session directory's requests and frames.
pub const WIRE_VERSION: u32 = 4;

/// The most bytes one line holds, newline included.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// The most raw bytes one blob chunk carries.
pub const BLOB_CHUNK_BYTES: usize = 1024 * 1024;

/// The most raw bytes one blob holds once assembled.
pub const MAX_BLOB_BYTES: usize = 8 * 1024 * 1024;

/// The most blobs a connection holds before the frame that names them: a
/// draft's eight images and a little slack.
pub const MAX_PENDING_BLOBS: usize = 10;

/// The most raw blob bytes a connection holds before the frame that names
/// them: eight images at the 3.75 MiB image bound, and a little slack.
pub const MAX_PENDING_BLOB_BYTES: usize = 40 * 1024 * 1024;

/// The most pages one snapshot takes.
pub const MAX_SNAPSHOT_PAGES: u32 = 64;

/// The most encoded row bytes one snapshot takes: twice the presentation
/// bound, for the encoding.
pub const MAX_SNAPSHOT_BYTES: usize = 2 * crate::views::presentation::MAX_PRESENTATION_BYTES;

/// The encoded bytes one snapshot page aims to stay under, leaving room
/// for its frame within [`MAX_LINE_BYTES`].
const PAGE_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// A request's id, chosen by the client and unique on its connection.
pub type RequestId = u64;

/// An attachment's id, chosen by the host.
pub type AttachmentId = u64;

/// The node an attachment targets: the root or a child of a tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeAddress {
    /// The tree.
    pub tree_id: String,
    /// The node within it; the root's own id addresses the root.
    pub agent_id: String,
}

/// The client's first frame.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// The client's [`WIRE_VERSION`].
    pub proto: u32,
    /// The client binary's build id.
    pub binary_id: String,
    /// Optional functions the client supports.
    pub caps: Vec<String>,
}

/// The host's answer to an admitted [`Hello`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Welcome {
    /// The host's [`WIRE_VERSION`].
    pub proto: u32,
    /// The host binary's build id.
    pub binary_id: String,
    /// This host process's generation; it changes on every host start.
    pub host_generation: u64,
    /// Optional functions the host supports.
    pub caps: Vec<String>,
    /// The session this connection serves, when it serves one: a host of
    /// many sessions names none, and its client finds them in the
    /// session directory ([`Op::ListSessions`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<WelcomeSession>,
}

/// The one session a connection serves, as its welcome names it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WelcomeSession {
    /// The durable session id.
    pub session_id: String,
    /// The session's agent tree; not always the session id (a rebound
    /// session's tree is its lineage's).
    pub tree_id: String,
    /// The session's root node, the address an attach to the root names.
    pub root: NodeAddress,
}

/// Why the host refused a handshake.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HandshakeRefusal {
    /// The versions differ; neither side adapts.
    VersionMismatch {
        /// The host's version.
        host: u32,
        /// The client's.
        client: u32,
    },
    /// The first frame was not a hello.
    Malformed {
        /// What was wrong.
        detail: String,
    },
    /// The host takes no more connections now.
    Busy,
    /// The session the connection would serve could not be opened.
    Unavailable {
        /// Why.
        detail: String,
    },
}

/// The host's answer to `hello`: [`Welcome`], or the refusal.
///
/// # Errors
///
/// [`HandshakeRefusal::VersionMismatch`] unless `hello.proto` is exactly
/// [`WIRE_VERSION`].
pub fn admit_hello(hello: &Hello, welcome: Welcome) -> Result<Welcome, HandshakeRefusal> {
    if hello.proto != WIRE_VERSION {
        return Err(HandshakeRefusal::VersionMismatch {
            host: WIRE_VERSION,
            client: hello.proto,
        });
    }
    Ok(welcome)
}

/// One request a client sends.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "op",
    rename_all = "snake_case",
    deny_unknown_fields,
    bound(
        serialize = "P::Intent: Serialize",
        deserialize = "P::Intent: DeserializeOwned"
    )
)]
#[schemars(rename = "Op", bound = "P::Intent: schemars::JsonSchema")]
pub enum Op<P: ProtocolExtension> {
    /// Attach to a node.
    Attach {
        /// The node.
        node: NodeAddress,
    },
    /// Detach; the node keeps running.
    Detach {
        /// The attachment.
        attachment: AttachmentId,
    },
    /// Send an intent to the attached node.
    Intent {
        /// The attachment.
        attachment: AttachmentId,
        /// The intent, its images' bytes taken out into `blobs`.
        intent: SessionIntent<P>,
        /// The blobs holding its images' bytes, in image order.
        blobs: Vec<String>,
    },
    /// Liveness.
    Ping,
    /// Re-resolve the session's binding and open it again.
    RetryAttach {
        /// The requester: the connection's root attachment.
        attachment: AttachmentId,
        /// The client's id for the request, unique among its own, echoed
        /// on every `moved` the request causes.
        request_id: String,
    },
    /// Switch the session to the user's session `session`.
    ResumeSession {
        /// The requester: the connection's root attachment.
        attachment: AttachmentId,
        /// As [`Op::RetryAttach`]'s.
        request_id: String,
        /// The durable session id.
        session: String,
    },
    /// Carry the session's history into a new session on the current
    /// root binding.
    Rebind {
        /// The requester: the connection's root attachment.
        attachment: AttachmentId,
        /// As [`Op::RetryAttach`]'s.
        request_id: String,
        /// Why exact reopen cannot continue, or the operator's own reason.
        cause: String,
    },
    /// Stop the session `session`: its running turn is cancelled with the
    /// root's `interrupt{stopped}` recorded first, its background processes
    /// are stopped, its running and held sub-agents are interrupted with
    /// cause `stopped`, and a held or pending continuation is ended, so
    /// nothing continues by itself. A host-level request, naming the
    /// session rather than an attachment, so a session nobody attaches can
    /// be stopped; a connection has one in flight. Answered `stopped` once
    /// the stop is durable, `unknown_session` for a session the host does
    /// not serve, or `session_unavailable` when the stop could not be
    /// recorded or did not finish within the host's bound. Repeating it is
    /// harmless: a stopped session's stop records nothing more.
    StopSession {
        /// The durable session id.
        session: String,
    },
    /// The host's sessions, newest first: a one-shot read of the session
    /// directory, needing no attachment.
    ListSessions {
        /// Only the sessions of this workspace, when given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Opens the connection's one host attachment: the session directory,
    /// as a snapshot of its rows and then each row that changes.
    WatchSessions,
    /// Closes the host attachment.
    UnwatchSessions {
        /// The host attachment.
        attachment: AttachmentId,
    },
    /// Creates a session in `workspace`, dormant until attached.
    NewSession {
        /// An existing directory, the session's workspace.
        workspace: String,
        /// The root template; the workspace's `[root] agent` when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<String>,
        /// A uuid the client draws for each new-session action. It decides
        /// the new session's id, so a retry of the same action answers the
        /// session it created and never makes a second one.
        request_id: String,
    },
    /// Detaches every attachment of `session` and releases it now, unless
    /// something other than an attachment keeps it resident.
    ReleaseSession {
        /// The durable session id.
        session: String,
    },
}

impl<P: ProtocolExtension> Op<P> {
    /// The op's wire name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Attach { .. } => "attach",
            Self::Detach { .. } => "detach",
            Self::Intent { .. } => "intent",
            Self::Ping => "ping",
            Self::RetryAttach { .. } => "retry_attach",
            Self::ResumeSession { .. } => "resume_session",
            Self::Rebind { .. } => "rebind",
            Self::StopSession { .. } => "stop_session",
            Self::ListSessions { .. } => "list_sessions",
            Self::WatchSessions => "watch_sessions",
            Self::UnwatchSessions { .. } => "unwatch_sessions",
            Self::NewSession { .. } => "new_session",
            Self::ReleaseSession { .. } => "release_session",
        }
    }
}

/// The answer to an admitted request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "answer", rename_all = "snake_case", deny_unknown_fields)]
pub enum Answer {
    /// Attached; the attach snapshot follows as events and pages.
    Attached {
        /// The new attachment.
        attachment: AttachmentId,
    },
    /// Detached.
    Detached,
    /// The intent entered the node's mailbox.
    Accepted,
    /// Liveness.
    Pong,
    /// The session's work stopped, durably (`stop_session`).
    Stopped,
    /// The session request switched the session to the one rooted at
    /// `node`; the requester's `moved` follows.
    Moved {
        /// The new session's root.
        node: NodeAddress,
    },
    /// The session directory's rows (`list_sessions`).
    Sessions {
        /// Newest first.
        rows: Vec<SessionRow>,
    },
    /// The host attachment is open (`watch_sessions`); its `directory`
    /// frames follow.
    Watching {
        /// The host attachment.
        attachment: AttachmentId,
    },
    /// The session `new_session` created, or had created for the same
    /// request id.
    Created {
        /// The durable session id.
        session: String,
        /// Its root node, which an attach names.
        node: NodeAddress,
        /// The environment its tools run with.
        environment: SessionEnvironment,
    },
    /// The session released (`release_session`): it is dormant.
    Released,
    /// The session stays resident (`release_session`): its attachments
    /// were detached, and this keeps it.
    Resident {
        /// What keeps it.
        reason: ResidentReason,
    },
}

/// Why a request was refused. Nothing it asked for happened.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestRefusal {
    /// The node's mailbox is full; try again.
    Backpressure,
    /// The same one-in-flight operation is already running.
    OneInFlight,
    /// The node is not running.
    Disconnected,
    /// The attached node's role does not take this intent.
    RefusedForRole {
        /// The intent's row in the role table.
        row: String,
    },
    /// No such node.
    UnknownNode,
    /// No such session on this host: `stop_session` named one it does not
    /// serve. A session another host serves is stopped by that host.
    UnknownSession,
    /// Another host process holds the node's tree; nothing was opened.
    TreeHostedElsewhere {
        /// The machine the holder runs on.
        host_name: String,
        /// The holder's binary id, when it recorded one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary: Option<String>,
        /// The holder's process id.
        pid: u32,
    },
    /// The node was the root of the session the connection's welcome named,
    /// which has since switched to the one rooted at `node`: attach that.
    Moved {
        /// The served session's root now.
        node: NodeAddress,
    },
    /// The node is closed: it never runs again.
    NodeClosed,
    /// The node is retiring: its host is releasing it.
    NodeRetiring,
    /// The node is outside the operator's reach: the operator reaches the
    /// nodes the tree's root may write to.
    OutOfReach,
    /// The tree refused the operator's mail; nothing was written.
    MailRefused {
        /// The tree's reason.
        detail: String,
    },
    /// No such attachment on this connection.
    NotAttached,
    /// The session request could not open its session; the session the
    /// connection serves is unchanged.
    SessionUnavailable {
        /// Why.
        detail: String,
    },
    /// The request could not be read.
    Malformed {
        /// What was wrong.
        detail: String,
    },
    /// This host does not do what the request asks yet.
    Unsupported {
        /// What it does not do.
        detail: String,
    },
}

/// A request's outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Done.
    Ok(Answer),
    /// Refused.
    Refused(RequestRefusal),
}

/// One part of a blob.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlobChunk {
    /// The blob, named by the frame that references it.
    pub id: String,
    /// Where this part starts.
    pub offset: u64,
    /// The part, standard base64.
    pub data: String,
    /// Whether this is the blob's last part.
    pub last: bool,
}

/// One page of an attachment's presentation snapshot. The pages of one
/// revision arrive in order; the last one completes the snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotPage {
    /// The attachment.
    pub attachment: AttachmentId,
    /// The snapshot's schema.
    pub schema: u32,
    /// The transcript it replaces.
    pub scope: TranscriptScope,
    /// Its revision.
    pub revision: u64,
    /// This page's position, from zero.
    pub page: u32,
    /// Its rows, in order.
    pub items: Vec<TranscriptItem>,
    /// Whether this is the last page.
    pub last: bool,
}

/// Frames a client sends.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "frame",
    rename_all = "snake_case",
    deny_unknown_fields,
    bound(
        serialize = "P::Intent: Serialize",
        deserialize = "P::Intent: DeserializeOwned"
    )
)]
#[schemars(rename = "ClientFrame", bound = "P::Intent: schemars::JsonSchema")]
pub enum ClientFrame<P: ProtocolExtension> {
    /// The handshake.
    Hello(Hello),
    /// A request.
    Request {
        /// Its id.
        id: RequestId,
        /// What it asks.
        op: Op<P>,
    },
    /// A blob part, sent before the request that references it.
    Blob(BlobChunk),
    /// The client is leaving.
    Bye,
}

/// Frames a host sends.
#[allow(
    clippy::large_enum_variant,
    reason = "an event carries one observation by value"
)]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "frame",
    rename_all = "snake_case",
    deny_unknown_fields,
    bound(
        serialize = "P::Intent: Serialize, P::Observation: Serialize",
        deserialize = "P::Intent: DeserializeOwned, P::Observation: DeserializeOwned"
    )
)]
#[schemars(
    rename = "HostFrame",
    bound = "P::Intent: schemars::JsonSchema, P::Observation: schemars::JsonSchema"
)]
pub enum HostFrame<P: ProtocolExtension> {
    /// The handshake's answer.
    Welcome(Welcome),
    /// The handshake was refused; the host closes.
    Refused(HandshakeRefusal),
    /// A request's outcome.
    Response {
        /// The request's id.
        id: RequestId,
        /// Its outcome.
        outcome: Outcome,
    },
    /// One observation on an attachment.
    Event {
        /// The attachment.
        attachment: AttachmentId,
        /// Its sequence number on the attachment, from one, for gap
        /// detection; never replayed.
        seq: u64,
        /// Who receives it.
        class: ObservationClass,
        /// The observation, its images' bytes taken out into `blobs`.
        observation: SessionObservation<P>,
        /// The blobs holding its images' bytes, in image order.
        blobs: Vec<String>,
    },
    /// A page of an attachment's presentation snapshot.
    Snapshot(SnapshotPage),
    /// A blob part, sent before the event that references it.
    Blob(BlobChunk),
    /// The attachment fell behind on a required event and is detached;
    /// attach again for a fresh snapshot.
    ResyncRequired {
        /// The attachment.
        attachment: AttachmentId,
    },
    /// The session the attachment showed was replaced by another (`/new`,
    /// a resume, a rebind): the attachment has ended, and the client attaches
    /// `node`, the new session's root, fresh.
    Moved {
        /// The attachment that ended.
        attachment: AttachmentId,
        /// The node to attach next.
        node: NodeAddress,
        /// The session request that caused the switch, by its client's
        /// id; absent for a switch no session request caused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// The session directory, on the host attachment: a snapshot replaces
    /// every row the client holds, and a change replaces the rows it names.
    Directory {
        /// The host attachment.
        attachment: AttachmentId,
        /// Its sequence number on the host attachment, from one.
        seq: u64,
        /// Whether `rows` is the whole directory.
        snapshot: bool,
        /// The rows, newest first.
        rows: Vec<SessionRow>,
    },
    /// The attachment ended because its session is no longer resident on
    /// this host (`release_session`, or the session ended); an attach
    /// opens it again.
    Detached {
        /// The attachment that ended.
        attachment: AttachmentId,
    },
    /// The host is closing the connection.
    Bye,
}

/// A frame that could not be written or read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    /// The line is over [`MAX_LINE_BYTES`].
    TooLarge,
    /// The blobs held would be over their bounds ([`MAX_PENDING_BLOBS`],
    /// [`MAX_PENDING_BLOB_BYTES`], [`MAX_BLOB_BYTES`]).
    BlobLimit,
    /// The line is not a frame.
    Malformed(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(formatter, "the frame is over {MAX_LINE_BYTES} bytes"),
            Self::BlobLimit => write!(
                formatter,
                "the blobs held are over their bounds: {MAX_PENDING_BLOBS} blobs, \
                 {MAX_PENDING_BLOB_BYTES} bytes in all, {MAX_BLOB_BYTES} bytes each"
            ),
            Self::Malformed(detail) => write!(formatter, "malformed frame: {detail}"),
        }
    }
}

impl std::error::Error for WireError {}

/// `frame` as one line, newline included.
///
/// # Errors
///
/// [`WireError::TooLarge`] when the line would be over the cap.
pub fn encode_line<T: Serialize>(frame: &T) -> Result<String, WireError> {
    let mut line =
        serde_json::to_string(frame).map_err(|error| WireError::Malformed(error.to_string()))?;
    line.push('\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(WireError::TooLarge);
    }
    Ok(line)
}

/// The frame one line holds, its newline optional.
///
/// # Errors
///
/// [`WireError::TooLarge`] over the cap; [`WireError::Malformed`] when it
/// is not a frame.
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, WireError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(WireError::TooLarge);
    }
    serde_json::from_str(line.strip_suffix('\n').unwrap_or(line))
        .map_err(|error| WireError::Malformed(error.to_string()))
}

/// The host frame one line holds, its newline optional. An event whose
/// class is not its observation's own is refused: the receiver never trusts
/// the class as sent.
///
/// # Errors
///
/// As [`decode_line`], and [`WireError::Malformed`] for a mismatched class.
pub fn decode_host_line<P>(line: &str) -> Result<HostFrame<P>, WireError>
where
    P: ProtocolExtension,
    P::Intent: DeserializeOwned,
    P::Observation: DeserializeOwned,
{
    let frame: HostFrame<P> = decode_line(line)?;
    if let HostFrame::Event {
        class, observation, ..
    } = &frame
        && *class != observation.class()
    {
        return Err(WireError::Malformed(
            "an event's class is not its observation's".into(),
        ));
    }
    Ok(frame)
}

/// Splits a byte stream into lines, holding at most [`MAX_LINE_BYTES`]: a
/// longer line is reported once as [`WireError::TooLarge`] and skipped to
/// its newline.
#[derive(Debug, Default)]
pub struct LineReader {
    held: Vec<u8>,
    skipping: bool,
}

impl LineReader {
    /// Takes `bytes` and returns every line they complete, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<String, WireError>> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                if self.skipping {
                    self.skipping = false;
                } else {
                    let held = std::mem::take(&mut self.held);
                    lines.push(
                        String::from_utf8(held)
                            .map_err(|_| WireError::Malformed("a line is not UTF-8".into())),
                    );
                }
                continue;
            }
            if self.skipping {
                continue;
            }
            // The newline counts toward the cap.
            if self.held.len() + 1 >= MAX_LINE_BYTES {
                self.held = Vec::new();
                self.skipping = true;
                lines.push(Err(WireError::TooLarge));
                continue;
            }
            self.held.push(byte);
        }
        lines
    }
}

/// The drafts an intent carries.
fn intent_drafts<P: ProtocolExtension>(intent: &mut SessionIntent<P>) -> Vec<&mut Draft> {
    match intent {
        SessionIntent::Prompt(draft)
        | SessionIntent::FollowUp(draft)
        | SessionIntent::Steer(draft) => {
            vec![draft]
        }
        _ => Vec::new(),
    }
}

/// The drafts an observation carries.
fn observation_drafts<P: ProtocolExtension>(
    observation: &mut SessionObservation<P>,
) -> Vec<&mut Draft> {
    match observation {
        SessionObservation::DraftAccepted(intent) | SessionObservation::DraftRejected(intent) => {
            intent_drafts(intent)
        }
        SessionObservation::DraftRestored(draft) => vec![draft],
        _ => Vec::new(),
    }
}

/// Where a frame's image bytes go: the blobs, and their ids in image order.
#[derive(Debug, Default)]
pub struct TakenImages {
    /// The ids, in image order, for the frame's `blobs`.
    pub ids: Vec<String>,
    /// The chunks to send before the frame.
    pub chunks: Vec<BlobChunk>,
}

fn take_from(drafts: Vec<&mut Draft>, prefix: &str) -> TakenImages {
    use base64::Engine as _;
    let mut taken = TakenImages::default();
    for image in drafts.into_iter().flat_map(|draft| draft.images.iter_mut()) {
        let id = format!("{prefix}.{}", taken.ids.len());
        let bytes = std::mem::take(&mut image.bytes);
        let mut parts = bytes.chunks(BLOB_CHUNK_BYTES).peekable();
        let mut offset = 0_u64;
        if parts.peek().is_none() {
            taken.chunks.push(BlobChunk {
                id: id.clone(),
                offset: 0,
                data: String::new(),
                last: true,
            });
        }
        while let Some(part) = parts.next() {
            taken.chunks.push(BlobChunk {
                id: id.clone(),
                offset,
                data: base64::engine::general_purpose::STANDARD.encode(part),
                last: parts.peek().is_none(),
            });
            offset += u64::try_from(part.len()).unwrap_or(u64::MAX);
        }
        taken.ids.push(id);
    }
    taken
}

/// Takes the image bytes out of `intent` into blobs named `<prefix>.<n>`.
pub fn take_images<P: ProtocolExtension>(
    intent: &mut SessionIntent<P>,
    prefix: &str,
) -> TakenImages {
    take_from(intent_drafts(intent), prefix)
}

/// [`take_images`] for an observation.
pub fn take_observation_images<P: ProtocolExtension>(
    observation: &mut SessionObservation<P>,
    prefix: &str,
) -> TakenImages {
    take_from(observation_drafts(observation), prefix)
}

/// Collects blob chunks until the frame that references them arrives,
/// holding at most [`MAX_PENDING_BLOBS`] blobs of [`MAX_PENDING_BLOB_BYTES`]
/// in all. Whatever the next request or event does not name is dropped.
#[derive(Debug, Default)]
pub struct BlobAssembler {
    partial: BTreeMap<String, Vec<u8>>,
    complete: BTreeMap<String, Vec<u8>>,
}

impl BlobAssembler {
    /// Takes one chunk.
    ///
    /// # Errors
    ///
    /// Refuses a chunk out of order, a part that is not base64, a blob
    /// over [`MAX_BLOB_BYTES`], or a part for a blob already complete.
    pub fn push(&mut self, chunk: BlobChunk) -> Result<(), WireError> {
        use base64::Engine as _;
        if self.complete.contains_key(&chunk.id) {
            return Err(WireError::Malformed("a blob part after its last".into()));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&chunk.data)
            .map_err(|_| WireError::Malformed("a blob part is not base64".into()))?;
        let pending: usize = self
            .partial
            .values()
            .chain(self.complete.values())
            .map(Vec::len)
            .sum();
        let new_blob = !self.partial.contains_key(&chunk.id);
        if (new_blob && self.partial.len() + self.complete.len() >= MAX_PENDING_BLOBS)
            || pending + bytes.len() > MAX_PENDING_BLOB_BYTES
        {
            self.discard();
            return Err(WireError::BlobLimit);
        }
        let held = self.partial.entry(chunk.id.clone()).or_default();
        if u64::try_from(held.len()).ok() != Some(chunk.offset) {
            self.partial.remove(&chunk.id);
            return Err(WireError::Malformed("a blob part out of order".into()));
        }
        if held.len() + bytes.len() > MAX_BLOB_BYTES {
            self.partial.remove(&chunk.id);
            return Err(WireError::BlobLimit);
        }
        held.extend_from_slice(&bytes);
        if chunk.last
            && let Some(bytes) = self.partial.remove(&chunk.id)
        {
            self.complete.insert(chunk.id, bytes);
        }
        Ok(())
    }

    /// Drops every blob held: the frame they preceded named none of them.
    pub fn discard(&mut self) {
        self.partial.clear();
        self.complete.clear();
    }

    fn restore(&mut self, drafts: Vec<&mut Draft>, ids: &[String]) -> Result<(), WireError> {
        let restored = self.take(drafts, ids);
        // What this frame did not name is no one's.
        self.discard();
        restored
    }

    fn take(&mut self, drafts: Vec<&mut Draft>, ids: &[String]) -> Result<(), WireError> {
        let images: Vec<_> = drafts
            .into_iter()
            .flat_map(|draft| draft.images.iter_mut())
            .collect();
        if images.len() != ids.len() {
            return Err(WireError::Malformed(
                "the frame names another number of blobs than it has images".into(),
            ));
        }
        for (image, id) in images.into_iter().zip(ids) {
            image.bytes = self
                .complete
                .remove(id)
                .ok_or_else(|| WireError::Malformed(format!("blob {id} never completed")))?;
        }
        Ok(())
    }

    /// Puts the blobs `ids` names back into `intent`'s images.
    ///
    /// # Errors
    ///
    /// Refuses a blob that never completed, or a count that does not match
    /// the images.
    pub fn restore_images<P: ProtocolExtension>(
        &mut self,
        intent: &mut SessionIntent<P>,
        ids: &[String],
    ) -> Result<(), WireError> {
        self.restore(intent_drafts(intent), ids)
    }

    /// [`Self::restore_images`] for an observation.
    ///
    /// # Errors
    ///
    /// As [`Self::restore_images`].
    pub fn restore_observation_images<P: ProtocolExtension>(
        &mut self,
        observation: &mut SessionObservation<P>,
        ids: &[String],
    ) -> Result<(), WireError> {
        self.restore(observation_drafts(observation), ids)
    }
}

/// `snapshot` as pages for `attachment`, each within the line cap.
///
/// # Errors
///
/// [`WireError::TooLarge`] when one row alone is over the page budget.
pub fn page_snapshot(
    snapshot: PresentationSnapshot,
    attachment: AttachmentId,
) -> Result<Vec<SnapshotPage>, WireError> {
    let PresentationSnapshot {
        schema,
        scope,
        revision,
        items,
    } = snapshot;
    let mut pages: Vec<Vec<TranscriptItem>> = vec![Vec::new()];
    let mut used = 0;
    for item in items {
        let size = serde_json::to_vec(&item)
            .map_err(|error| WireError::Malformed(error.to_string()))?
            .len();
        if size > PAGE_BUDGET_BYTES {
            return Err(WireError::TooLarge);
        }
        if used + size > PAGE_BUDGET_BYTES {
            pages.push(Vec::new());
            used = 0;
        }
        used += size;
        if let Some(page) = pages.last_mut() {
            page.push(item);
        }
    }
    let count = pages.len();
    Ok(pages
        .into_iter()
        .enumerate()
        .map(|(index, items)| SnapshotPage {
            attachment,
            schema,
            scope: scope.clone(),
            revision,
            page: u32::try_from(index).unwrap_or(u32::MAX),
            items,
            last: index + 1 == count,
        })
        .collect())
}

/// Collects one attachment's snapshot pages into the snapshot.
#[derive(Debug, Default)]
pub struct SnapshotAssembler {
    pending: Option<SnapshotPage>,
    held_bytes: usize,
}

impl SnapshotAssembler {
    /// Takes one page; the last one returns the validated snapshot.
    ///
    /// # Errors
    ///
    /// Refuses a page out of order, one of another revision or scope, a
    /// snapshot over [`MAX_SNAPSHOT_PAGES`] or [`MAX_SNAPSHOT_BYTES`], or one
    /// that does not validate; the pages so far are dropped.
    pub fn push(&mut self, page: SnapshotPage) -> Result<Option<PresentationSnapshot>, WireError> {
        let page_bytes = page
            .items
            .iter()
            .map(|item| serde_json::to_vec(item).map_or(usize::MAX, |bytes| bytes.len()))
            .fold(0_usize, usize::saturating_add);
        if page.page >= MAX_SNAPSHOT_PAGES {
            self.pending = None;
            self.held_bytes = 0;
            return Err(WireError::TooLarge);
        }
        let held_bytes = if page.page == 0 {
            page_bytes
        } else {
            self.held_bytes.saturating_add(page_bytes)
        };
        if held_bytes > MAX_SNAPSHOT_BYTES {
            self.pending = None;
            self.held_bytes = 0;
            return Err(WireError::TooLarge);
        }
        self.held_bytes = held_bytes;
        let held = match self.pending.take() {
            None if page.page == 0 => page,
            None => return Err(WireError::Malformed("a snapshot page out of order".into())),
            Some(mut held) => {
                let next = held.page.checked_add(1);
                if Some(page.page) != next
                    || page.revision != held.revision
                    || page.scope != held.scope
                    || page.schema != held.schema
                    || page.attachment != held.attachment
                {
                    return Err(WireError::Malformed("a snapshot page out of order".into()));
                }
                held.items.extend(page.items);
                held.page = page.page;
                held.last = page.last;
                held
            }
        };
        if !held.last {
            self.pending = Some(held);
            return Ok(None);
        }
        let snapshot = PresentationSnapshot {
            schema: held.schema,
            scope: held.scope,
            revision: held.revision,
            items: held.items,
        };
        snapshot
            .validate()
            .map_err(|error| WireError::Malformed(format!("invalid snapshot: {error:?}")))?;
        Ok(Some(snapshot))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
