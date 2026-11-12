//! The face a TUI round speaks to its session through (spec 02 §4.4): wire
//! v2 frames ([`plakous_host_protocol::client`]) over a socket pair inside
//! this process, to a connection of the session's own host service that a
//! thread of the launcher serves. Every byte between the TUI and the
//! session is a frame.
//!
//! The launcher keeps two process-local signals beside the frames
//! ([`RoundMailbox`]). Abandon-and-release, and its planned-handoff form,
//! are flags on the session's mailbox: they outrank queued work, are set
//! after the terminal is restored, and must be set before the TUI's face
//! leaves, since its leaving quits the session. The launcher's cells (the
//! `/new` publication and the exact-resume fence) are written by the host
//! for the launcher's next round, not for the TUI.

use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use plakous_host_protocol::client::{self, Transport};
use plakous_host_protocol::views::transcript::TranscriptScope;
use plakous_host_protocol::wire::{Hello, NodeAddress, WIRE_VERSION};
use plakous_hosted::host_service::FacePublisher;
use plakous_hosted::host_service::wire::{Face, WireConnection, handshake, session_welcome};
use plakous_hosted::runtime::{HostWorker, HostWorkerHandle};

use crate::surface_protocol::tui_channel::{TuiActionSender, TuiEventReceiver};
use crate::surface_protocol::{PlakExtension, UserActionSender};
use crate::{CliFailure, CliResult, agent_wire, build_identity};

/// How long a release waits for the session's verdict on what the TUI sent
/// before it: a send returns before its verdict, and the release must not
/// outrank an intent still on its way.
const SENT_SETTLE: Duration = Duration::from_secs(1);

/// One round's face on its session, and what the launcher keeps of it.
pub(crate) struct RoundFace {
    /// What the TUI sends with.
    pub(crate) actions: TuiActionSender,
    /// What the TUI receives.
    pub(crate) events: TuiEventReceiver,
    /// The launcher's hold on the session.
    pub(crate) mailbox: RoundMailbox,
    /// Publishes the launch's pre-bind progress to the face's service.
    pub(crate) publisher: FacePublisher<PlakExtension>,
    /// The session's host worker.
    pub(crate) join: HostWorkerHandle,
}

/// What the launcher keeps of a round's session beside its TUI: the
/// session's mailbox for the release signals, and a hold on the TUI's face,
/// so the face leaves (and quits the session) only when this is dropped.
pub(crate) struct RoundMailbox {
    release: UserActionSender,
    face: TuiActionSender,
}

impl RoundMailbox {
    /// As [`plakous_host_protocol::IntentSender::request_abandon_and_release`],
    /// once what the TUI sent has its verdict.
    pub(crate) fn request_abandon_and_release(&self) {
        let _ = self.face.settle(SENT_SETTLE);
        self.release.request_abandon_and_release();
    }

    /// As [`plakous_host_protocol::IntentSender::request_handoff_and_release`],
    /// once what the TUI sent has its verdict.
    pub(crate) fn request_handoff_and_release(&self) {
        let _ = self.face.settle(SENT_SETTLE);
        self.release.request_handoff_and_release();
    }

    /// The mailbox of a session `actions` sends to, held for a face no
    /// service serves.
    #[cfg(test)]
    pub(crate) fn unattached(actions: UserActionSender) -> Self {
        Self {
            release: actions.clone(),
            face: TuiActionSender::InProcess(
                crate::surface_protocol::FaceActionSender::unattached(actions),
            ),
        }
    }
}

/// The face of a round over `worker`'s session, `session_id` as the
/// launcher knows it: a wire client of a connection the launcher serves on
/// a thread. The connection is a local face of the session (it may check
/// the installed build and hand the host off) and names no tree: it holds
/// the session through a switch inside the host without being moved.
///
/// # Errors
///
/// The in-process socket pair or the wire handshake failing; the session is
/// then asked to abandon and release, and joined within the product's
/// error bound.
pub(crate) fn round_face(
    (actions, events, join): HostWorker<PlakExtension>,
    session_id: &str,
) -> CliResult<RoundFace> {
    let release = actions.clone();
    match over_frames(actions, events, session_id) {
        Ok((actions, events, publisher)) => Ok(RoundFace {
            mailbox: RoundMailbox {
                release,
                face: actions.clone(),
            },
            actions,
            events,
            publisher,
            join,
        }),
        Err(failure) => {
            release.request_abandon_and_release();
            drop(release);
            let _ = crate::commands::support::join_tui_host_worker(join, "TUI host");
            Err(failure)
        }
    }
}

fn over_frames(
    actions: UserActionSender,
    events: plakous_host_protocol::ObservationReceiver<PlakExtension>,
    session_id: &str,
) -> CliResult<(
    TuiActionSender,
    TuiEventReceiver,
    FacePublisher<PlakExtension>,
)> {
    let failed = |error: std::io::Error| {
        CliFailure::new(format!(
            "the TUI's in-process wire transport failed: {error}"
        ))
    };
    let (host, tui) = UnixStream::pair().map_err(failed)?;
    let lines = agent_wire::read_lines(host.try_clone().map_err(failed)?);
    let mut answer = host.try_clone().map_err(failed)?;
    let root = NodeAddress {
        tree_id: String::new(),
        agent_id: String::new(),
    };
    let welcome = session_welcome(
        build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
        agent_wire::host_generation(),
        session_id.to_owned(),
        root.clone(),
    );
    // Nothing is taken from the session before the client's first attach,
    // so the root baseline the session publishes when it opens reaches that
    // attach: it need not ask for another, unless the session's mailbox
    // already lost root content.
    let baseline_follows = !events.transcript_resnapshot_required_for(&TranscriptScope::Root);
    let mut connection = WireConnection::new(root, host, actions)
        .face(Face::Local)
        .session_baseline_follows(baseline_follows);
    let publisher = connection.publisher();
    thread::Builder::new()
        .name("plak-tui-wire".into())
        .spawn(move || {
            if !matches!(
                handshake::<PlakExtension>(&lines, welcome, &mut answer, &|| false),
                Ok(true)
            ) {
                return;
            }
            // The client's first request is its root attach. It is answered
            // before the session's observations are taken, so the TUI sees
            // what the session said from its start.
            let attached = lines.recv().map(|line| {
                connection
                    .frame(line)
                    .and_then(|reading| connection.flush().map(|()| reading))
            });
            // A connection that fails ends the TUI's too: its receiver then
            // reports the session gone.
            if matches!(attached, Some(Ok(true))) {
                let _ = connection.serve(&lines, &events);
            }
        })
        .map_err(failed)?;
    let hello = Hello {
        proto: WIRE_VERSION,
        binary_id: build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
        caps: Vec::new(),
    };
    let shutdown = tui.try_clone().map_err(failed)?;
    let transport = Transport::new(tui.try_clone().map_err(failed)?, tui)
        .with_write_shutdown(move || shutdown.shutdown(std::net::Shutdown::Write));
    let (sender, receiver, _) = client::connect::<PlakExtension>(transport, hello)
        .map_err(|error| CliFailure::new(format!("the TUI's wire handshake failed: {error}")))?;
    Ok((
        TuiActionSender::Wire(sender),
        TuiEventReceiver::Wire(receiver),
        publisher,
    ))
}

/// Session-owned process-group stops are supplied at this boundary.
fn stop_background_processes_for_interactive_exit(_host_mailbox: &RoundMailbox) {}

/// Voluntary TUI quit after the terminal is cooked and the resume hint is
/// flushed. The host is asked to interrupt and shut down, then this process
/// exits when that work finishes or when
/// [`crate::commands::support::VOLUNTARY_QUIT_GRACE`] ends,
/// whichever comes first. It does not print the quit notice and it does not
/// wait out session release.
///
/// The mailbox is forgotten rather than dropped when the grace wins: dropping
/// it lets the TUI's face leave, which the host takes as Quit.
/// A panicked join invokes the product's content-free report callback.
pub(crate) fn exit_restored_tui(
    worker: HostWorkerHandle,
    host_mailbox: RoundMailbox,
    report_worker_failure: fn(),
) -> ! {
    stop_background_processes_for_interactive_exit(&host_mailbox);
    host_mailbox.request_abandon_and_release();
    let deadline = std::time::Instant::now() + crate::tui_timing::remaining_exit_budget();
    while !worker.is_finished() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let outcome = worker.quit_outcome();
    if worker.is_finished() {
        if worker.join().is_err() {
            report_worker_failure();
        }
    } else {
        std::mem::forget(worker);
    }
    std::mem::forget(host_mailbox);
    crate::tui_timing::exiting();
    // A quit the signal watcher requested reports the signalled status, the
    // same one a CLI turn stopped by `SIGTERM` or `SIGHUP` reports.
    let code = if outcome == plakous_hosted::runtime::HostQuitOutcome::Escalated {
        130
    } else if crate::signals::stop_reason().is_some() {
        crate::signals::SIGNALLED_EXIT_CODE
    } else {
        0
    };
    std::process::exit(code);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
