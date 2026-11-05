//! `plak host` (hidden, feature `web`): the per-user host process (spec 02
//! §4.6), as a skeleton that hosts no session yet.
//!
//! It takes this home's `host.lock` for its state key before anything else,
//! so one host serves one home's state; binds the control socket
//! `<home>/host/run/<state key>.sock` for this user only (spec 04 §4.7);
//! names its custody identity (the home's host id and this process's
//! instance); and captures the login shell's environment once for the tools
//! its sessions will run. A `SIGTERM` or `SIGINT` ends it within
//! [`HANDOFF_DEADLINE`]. Every connection that passes the peer check is
//! refused `unavailable` after its hello. No existing face uses any of this.

mod environment;
mod lock;
mod socket;

use std::time::Duration;

use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

use crate::shell::services::Services;

/// How long a stopping host may take to hand its sessions off before the
/// process exits regardless; under the unit's `TimeoutStopSec` (spec 02
/// §4.6).
pub(crate) const HANDOFF_DEADLINE: Duration = Duration::from_secs(60);

/// Runs the host until it is asked to stop.
///
/// # Errors
///
/// Another host holds this home's state, the host id cannot be read or
/// made, or the control socket cannot be prepared.
pub(crate) fn run_host_command(services: &Services) -> CliResult<ProcessOutcome> {
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
    let runtime = tokio::runtime::Builder::new_current_thread()
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
    runtime.block_on(bound.serve(move || stop.stopped()));
    // The socket goes before the lock: a host that takes the lock next finds
    // no socket of this one.
    bound.remove();
    drop(lock);
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(ProcessOutcome::Success)
}
