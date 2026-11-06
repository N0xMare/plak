//! `plak host` (hidden, feature `web`): the per-user host process (spec 02
//! §4.6), serving every session of the home.
//!
//! It takes this home's `host.lock` for its state key before anything else,
//! so one host serves one home's state; binds the control socket
//! `<home>/host/run/<state key>.sock` for this user only (spec 04 §4.7);
//! names its custody identity (the home's host id and this process's
//! instance); and captures the login shell's environment once for the tools
//! its sessions will run. Each connection is a client of the host's session
//! directory (spec 02 §4.4). A `SIGTERM` or `SIGINT` hands every resident
//! session off and ends the process within [`HANDOFF_DEADLINE`].

mod environment;
mod lock;
mod sessions;
mod socket;

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
    let host = SessionHost::new(
        sessions::ProductSessions::new(services, command.idle_release)?,
        binary,
        host_generation(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("plak host could not start its runtime"))?;
    let entered = runtime.enter();
    let bound = socket::Bound::bind(&paths.host_socket(), &lock)?;
    drop(entered);
    eprintln!(
        "plak host: serving on {} (host {}, instance {}); tools get {}",
        bound.path().display(),
        identity.host_id,
        identity.instance,
        environment.describe(),
    );
    runtime.block_on(bound.serve(&host, move || stop.stopped()));
    let stopping = Instant::now();
    // The socket goes first, so no client reaches a host that is handing
    // its sessions off, and before the lock: a host that takes the lock
    // next finds no socket of this one.
    bound.remove();
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
    Ok(ProcessOutcome::Success)
}

/// This process's generation: it differs on every start.
fn host_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        })
}
