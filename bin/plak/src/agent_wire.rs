//! `plak agent --wire v2` (hidden): wire v2 on stdio (spec 02 §4.4) over
//! the session this process opens, through its host service
//! ([`plakous_hosted::host_service::HostService`]): one connection, one attachment at a time, on the
//! session's root node or a child of its tree. Every observation reaches the attachment as the
//! service routes it, with its class; a presentation snapshot goes as pages
//! and image bytes as blobs.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use plakous_host_protocol::SessionIntent;
use plakous_host_protocol::wire::{HandshakeRefusal, LineReader, WireError};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::host_service::wire::{
    Face, WireConnection, handshake, refuse, session_root, session_root_of, session_welcome,
};
use plakous_hosted::runtime::{HostConfig, spawn_host_worker};

use crate::surface_protocol::PlakExtension;

const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs wire v2 on `input` and `stdout` over the session `config` opens.
///
/// The session opens before the handshake is answered, so the welcome names
/// it and a session that does not open is refused in a v2 frame. A
/// terminating signal ends the hello wait or the connection as a leaving
/// client does: the session is quit and joined.
///
/// # Errors
///
/// A session that does not open, or a lost stdout.
pub(crate) fn run_agent_wire(
    config: HostConfig<PlakExtension>,
    input: impl Read + Send + 'static,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let lines = read_lines(input);
    let session_id = config.session.id.clone();
    let root = session_root(&config);
    let state_roots = config.state_roots.clone();
    let (actions, events, worker) = match spawn_host_worker(config) {
        Ok(worker) => worker,
        Err(failure) => {
            refuse_unavailable(stdout, &failure)?;
            return Err(failure);
        }
    };
    let welcome = session_welcome(
        crate::build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
        host_generation(),
        session_id.clone(),
        root.clone(),
    );
    let served = match handshake::<PlakExtension>(&lines, welcome, stdout, &signalled) {
        // stdio's peer is the parent process that started this one.
        Ok(true) => Adapter::new(root, stdout, actions.clone())
            .face(Face::Local)
            .with_stop(signalled)
            .serving(session_id, move |id| session_root_of(&state_roots, id))
            .serve(&lines, &events),
        Ok(false) => Ok(()),
        Err(failure) => Err(failure),
    };
    // A connection that failed still ends its session.
    let _ = actions.send(SessionIntent::Quit);
    let deadline = Instant::now() + JOIN_TIMEOUT;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err(CliFailure::new(
                "plak agent host worker exceeded its shutdown deadline",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| CliFailure::new("plak agent host worker failed"))??;
    served.map(|()| {
        if signalled() {
            ProcessOutcome::Interrupted
        } else {
            ProcessOutcome::Success
        }
    })
}

/// Whether a terminating signal asked this process to stop.
fn signalled() -> bool {
    crate::signals::signalled_stops() > 0
}

/// Refuses the connection, in a v2 frame, for a session that did not open:
/// a `plak agent --wire v2` failure before dispatch is never a JSONL record.
///
/// # Errors
///
/// A lost stdout.
pub(crate) fn refuse_unavailable(stdout: &mut dyn Write, failure: &CliFailure) -> CliResult<()> {
    refuse::<PlakExtension>(
        stdout,
        HandshakeRefusal::Unavailable {
            detail: failure.message().to_owned(),
        },
    )
}

/// This process's generation: it differs on every start.
fn host_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// Lines from `input`, read on a thread of their own; the channel closes at
/// the end of input.
pub(crate) fn read_lines(
    mut input: impl Read + Send + 'static,
) -> mpsc::Receiver<Result<String, WireError>> {
    let (sender, lines) = mpsc::sync_channel(64);
    thread::spawn(move || {
        let mut reader = LineReader::default();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = match input.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            for line in reader.push(&buffer[..read]) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        }
    });
    lines
}

/// The stdio connection: the one client's state on the session's host
/// service.
pub(crate) type Adapter<W> = WireConnection<PlakExtension, W>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
