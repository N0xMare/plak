//! `plak host` (hidden, feature `web`): the per-user host process (spec 02
//! §4.6), serving every session of the home.
//!
//! It takes this home's `host.lock` for its state key before anything else,
//! so one host serves one home's state; binds the control socket
//! `<home>/host/run/<state key>.sock` for this user only (spec 04 §4.7);
//! names its custody identity (the home's host id and this process's
//! instance); and captures the login shell's environment once for the tools
//! its sessions will run. Each connection, on the control socket or on the
//! web endpoint `--web` adds, is a client of the host's session directory
//! (spec 02 §4.4). A `SIGTERM` or `SIGINT` hands every resident
//! session off and ends the process within [`HANDOFF_DEADLINE`].

mod environment;
mod lock;
mod runner;
mod sessions;
mod socket;

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::host_service::wire::SessionHost;

use crate::command::HostCommand;
use crate::shell::services::Services;

/// How long a stopping host may take to hand its sessions off before the
/// process exits regardless; under the unit's `TimeoutStopSec` (spec 02
/// §4.6).
pub(crate) const HANDOFF_DEADLINE: Duration = Duration::from_secs(60);

/// What the sessions' handoff leaves of the deadline for the rest of the
/// stop: the socket, the lock and the runtime.
const STOP_MARGIN: Duration = Duration::from_secs(5);

/// Runs the host until it is asked to stop.
///
/// # Errors
///
/// Another host holds this home's state, the host id cannot be read or
/// made, the tree store does not open, or the control socket cannot be
/// prepared.
pub(crate) fn run_host_command(
    services: &Services,
    command: &HostCommand,
) -> CliResult<ProcessOutcome> {
    // Armed first: a stop that arrives during the start still ends the
    // process within the deadline.
    crate::signals::install_bounded(crate::signals::cli_announcement(), HANDOFF_DEADLINE, || {});
    crate::signals::count_interrupts();
    let stop = crate::web::StopProbe::armed();
    let paths = services.paths()?;
    paths.ensure_layout().map_err(CliFailure::new)?;
    let binary = crate::build_identity::known_binary_id().unwrap_or_else(|| "unknown".into());
    let lock = lock::HostLock::take(&paths.host_lock(), &binary)?;
    crate::store::tree::bind_custody_host(paths).map_err(CliFailure::new)?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    let environment = environment::HostEnvironment::capture();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("plak host could not start its runtime"))?;
    let entered = runtime.enter();
    let bound = socket::Bound::bind(&paths.host_socket(), &lock)?;
    drop(entered);
    let (services, runner_client) = runner::prepare(services, &environment, &runtime)?;
    let sessions = sessions::ProductSessions::new(&services, command.idle_release)?;
    // Read before the host serves, so nothing this host does is mistaken
    // for what the last one left.
    let owed = sessions.owed()?;
    let host = SessionHost::new(sessions, binary, host_generation());
    let web = match &command.web {
        Some(args) => {
            let bound_web = runtime.block_on(crate::web::WebEndpoint::bind(
                &services,
                args,
                Arc::clone(&host),
                stop,
            ));
            match bound_web {
                Ok(endpoint) => {
                    eprintln!("plak host: web on {}", endpoint.listener());
                    Some(endpoint)
                }
                Err(failure) => {
                    bound.remove();
                    return Err(failure);
                }
            }
        }
        None => None,
    };
    eprintln!(
        "plak host: serving on {} (host {}, instance {}); tools get {}",
        bound.path().display(),
        identity.host_id,
        identity.instance,
        environment.describe(),
    );
    scan(&host, owed, stop);
    // The web endpoint serves beside the control socket and stops with it.
    let web = web.map(|web| runtime.spawn(web.serve()));
    let served = runtime.block_on(async {
        tokio::select! {
            () = bound.serve(&host, move || stop.stopped()) => Ok(()),
            result = runner::supervise(paths, &runner_client) => result,
        }
    });
    if let Some(web) = web {
        if served.is_err() {
            web.abort();
        }
        let _ = runtime.block_on(web);
    }
    let stopping = Instant::now();
    // The socket goes first, so no client reaches a host that is handing
    // its sessions off, and before the lock: a host that takes the lock
    // next finds no socket of this one.
    bound.remove();
    // The handoff first closes the host: no session opens or is created
    // from then on.
    let deadline = HANDOFF_DEADLINE.saturating_sub(STOP_MARGIN);
    for (session, failure) in host.hand_off_all(deadline) {
        eprintln!(
            "plak host: session {session} was not handed off: {}",
            failure.message()
        );
    }
    eprintln!(
        "plak host: stopped in {} ms",
        stopping.elapsed().as_millis()
    );
    drop(lock);
    runtime.shutdown_timeout(Duration::from_secs(1));
    served.map(|()| ProcessOutcome::Success)
}

/// This process's generation: it differs on every start.
fn host_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// The host-start scan (spec 02 §4.6): opens, one at a time on a thread of
/// its own, each session the last host left a continuation it runs by
/// itself, so it continues with no client attached; each then stays
/// resident as long as anything keeps it. A stop ends the scan.
fn scan(
    host: &Arc<SessionHost<crate::surface_protocol::PlakExtension>>,
    owed: Vec<String>,
    stop: crate::web::StopProbe,
) {
    if owed.is_empty() {
        return;
    }
    let host = Arc::clone(host);
    std::thread::spawn(move || {
        continue_each(
            owed,
            || stop.stopped(),
            |session| {
                eprintln!("plak host: continuing session {session}");
                if let Err(failure) = host.slot(session).resident() {
                    eprintln!(
                        "plak host: session {session} did not continue: {}",
                        failure.message()
                    );
                }
                host.changed();
            },
        );
    });
}

/// Opens each of `owed` in turn with `open`, until `stopped`.
fn continue_each(owed: Vec<String>, stopped: impl Fn() -> bool, mut open: impl FnMut(&str)) {
    for session in owed {
        if stopped() {
            return;
        }
        open(&session);
    }
}

#[cfg(test)]
mod tests {
    use super::continue_each;

    /// A stop ends the start scan before its next session.
    #[test]
    fn a_stop_ends_the_start_scan() {
        let mut opened = Vec::new();
        let stopped = std::cell::Cell::new(false);
        continue_each(
            vec!["a".into(), "b".into()],
            || stopped.get(),
            |session| {
                opened.push(session.to_owned());
                stopped.set(true);
            },
        );
        assert_eq!(opened, ["a"]);
    }
}
