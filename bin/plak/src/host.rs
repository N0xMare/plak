//! `plak host` (feature `web`, enabled by default): the per-user host process (spec 02
//! §4.6), serving every session of the home.
//!
//! It takes the home-wide guard and its state key's diagnostic lock before
//! serving, so no second state-key host starts beside it; binds the control socket
//! `<home>/host/run/<state key>.sock` for this user only (spec 04 §4.7);
//! names its custody identity (the home's host id and this process's
//! instance); and captures the login shell's environment once for the tools
//! its sessions will run. Each connection, on the control socket or on the
//! web endpoint `--web` adds, is a client of the host's session directory
//! (spec 02 §4.4). A `SIGTERM` or `SIGINT` hands every resident
//! session off and ends the process within [`HANDOFF_DEADLINE`].

mod bridge;
mod console;
mod idle;
mod lock;
mod requested_reload;
pub(crate) mod send;
mod sessions;
mod socket;
mod starter;
mod upgrade;
use crate::terminal_registry as terminals;

#[cfg(test)]
mod scan_tests;

pub(crate) const AVAILABLE: bool = true;

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::host_service::wire::{SessionHost, SessionSlot};

use crate::command::HostCommand;
use crate::commands::runner::{environment, services as runner};
use crate::shell::services::Services;

/// How long a stopping host may take to hand its sessions off before the
/// process exits regardless; under the unit's `TimeoutStopSec` (spec 02
/// §4.6).
pub(crate) const HANDOFF_DEADLINE: Duration = Duration::from_secs(60);

/// What the sessions' handoff leaves of the deadline for the rest of the
/// stop: the socket, the lock and the runtime.
const STOP_MARGIN: Duration = Duration::from_secs(5);

fn host_read_limits() -> plakous_hosted::host_service::wire::HostReadLimits {
    console::admission_limits()
}

pub(crate) fn connect_stdio(services: &Services, auto_start: bool) -> CliResult<ProcessOutcome> {
    bridge::connect(services.paths()?, auto_start)
}

pub(crate) fn upgrade(
    services: &Services,
    expect_binary: &str,
    wait: Duration,
) -> CliResult<ProcessOutcome> {
    upgrade::request(services, expect_binary, wait)
}

fn startup_runtime() -> CliResult<(tokio::runtime::Runtime, tokio::signal::unix::Signal)> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("plak host could not start its runtime"))?;
    let entered = runtime.enter();
    let reload = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .map_err(|error| CliFailure::new(format!("cannot listen for UI reload: {error}")))?;
    drop(entered);
    Ok((runtime, reload))
}

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
    match command.start {
        crate::command::HostStart::Helper => return starter::helper(services.paths()?),
        crate::command::HostStart::Supervisor => return starter::supervise(services.paths()?),
        crate::command::HostStart::Foreground | crate::command::HostStart::Detached => {}
    }
    crate::environment::host::serving_keys(command.start == crate::command::HostStart::Detached);
    serve_host(services, command)
}

fn serve_host(services: &Services, command: &HostCommand) -> CliResult<ProcessOutcome> {
    let host_arguments = Arc::new(upgrade::host_argv(command)?);
    // Arm reload before any startup I/O, even for a control-only host.
    let (runtime, reload) = startup_runtime()?;
    // A stop that arrives during the start still ends the
    // process within the deadline.
    crate::signals::install_bounded(crate::signals::cli_announcement(), HANDOFF_DEADLINE, || {});
    crate::signals::count_interrupts();
    let stop = crate::web::StopProbe::armed();
    let paths = services.paths()?;
    paths.ensure_layout().map_err(CliFailure::new)?;
    let binary = crate::build_identity::known_binary_id().unwrap_or_else(|| "unknown".into());
    let (home_lock, lock) = lock::HostLock::take_home(paths, &binary)?;
    if let Some(replacement) = upgrade::startup_replacement(paths)? {
        drop(lock);
        drop(home_lock);
        runtime.shutdown_timeout(Duration::from_secs(1));
        return replacement.exec(paths);
    }
    crate::store::tree::bind_custody_host(paths).map_err(CliFailure::new)?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    let environment = environment::HostEnvironment::capture();
    let entered = runtime.enter();
    let mut bound = socket::Bound::bind(&paths.host_socket(), &lock)?;
    let upgrade_listener = upgrade::bind(paths)?;
    drop(entered);
    let restart_services = services.clone();
    let (services, terminals) = terminals::TerminalRegistry::compose(services, host_generation())?;
    let (services, local) = upgrade::prepare_host(&services)?;
    let sessions = sessions::ProductSessions::new(
        &services,
        command.idle_release,
        Some(runtime.handle().clone()),
    )?;
    // Read before the host serves, so nothing this host does is mistaken
    // for what the last one left.
    let owed = sessions.owed()?;
    let console_switches = sessions.console_switches();
    let host = SessionHost::new(sessions, binary.clone(), terminals.generation());
    terminals.serve(&host, runtime.handle())?;
    let (web, idle_reload) = match bind_web(
        &runtime,
        &services,
        command,
        &host,
        stop,
        reload,
        console_switches,
    ) {
        Ok(endpoint) => endpoint,
        Err(failure) => {
            bound.remove();
            return Err(failure);
        }
    };
    runner::host::start(&services, local, &environment, &runtime);
    upgrade::acknowledge(&services, &runtime)?;
    crate::shell::catalog_refresh::hook(services.paths()?.clone(), None)(None);
    eprintln!(
        "plak host: serving on {} (host {}, instance {}); tools get {}",
        bound.path().display(),
        identity.host_id,
        identity.instance,
        environment.describe(),
    );
    let scanning = Arc::new(std::sync::atomic::AtomicBool::new(!owed.is_empty()));
    scan(
        &host,
        owed,
        move || stop.stopped(),
        &services,
        runtime.handle(),
        Arc::clone(&scanning),
    );
    let reload_requests = runtime.spawn(requested_reload::serve(
        Arc::clone(&host),
        services.paths()?.clone(),
        binary,
    ));
    // The web endpoint serves beside the control socket and stops with it.
    let web = web.map(|web| runtime.spawn(web.serve()));
    let idle = (command.start == crate::command::HostStart::Detached).then(idle::IdleExit::default);
    let upgrades = upgrade::Coordinator::start(
        &runtime,
        upgrade_listener,
        &host,
        &services,
        Arc::clone(&host_arguments),
    );
    let pending = upgrades.pending();
    runtime.block_on(bound.serve(
        &host,
        move || stop.stopped() || upgrade::has_ready(&pending),
        idle,
        scanning,
    ));
    let ready = upgrades.stop(&runtime, services.paths()?);
    stop_listeners(&runtime, reload_requests, web, idle_reload, ready.is_some());
    let stopping = Instant::now();
    // The socket goes first, so no client reaches a host that is handing
    // its sessions off, and before the lock: a host that takes the lock
    // next finds no socket of this one.
    bound.remove();
    runtime.block_on(terminals.stop());
    // The handoff first closes the host: no session opens or is created
    // from then on.
    hand_off(&host);
    eprintln!(
        "plak host: stopped in {} ms",
        stopping.elapsed().as_millis()
    );
    drop(lock);
    runtime.shutdown_timeout(Duration::from_secs(1));
    drop(home_lock);
    upgrade::replace(ready, &restart_services, command, stop, &host_arguments)
}

fn stop_listeners(
    runtime: &tokio::runtime::Runtime,
    reload_requests: tokio::task::JoinHandle<()>,
    web: Option<tokio::task::JoinHandle<()>>,
    idle_reload: Option<tokio::task::JoinHandle<()>>,
    upgrading: bool,
) {
    reload_requests.abort();
    let _ = runtime.block_on(reload_requests);
    if let Some(web) = web {
        if upgrading {
            web.abort();
        }
        let _ = runtime.block_on(web);
    }
    if let Some(reload) = idle_reload {
        reload.abort();
    }
}

fn bind_web(
    runtime: &tokio::runtime::Runtime,
    services: &Services,
    command: &HostCommand,
    host: &Arc<SessionHost<crate::surface_protocol::PlakExtension>>,
    stop: crate::web::StopProbe,
    reload: tokio::signal::unix::Signal,
    switches: plakd_wire_types::console::ConfiguredSwitches,
) -> CliResult<(
    Option<crate::web::WebEndpoint>,
    Option<tokio::task::JoinHandle<()>>,
)> {
    if let Some(args) = &command.web {
        let endpoint = runtime.block_on(crate::web::WebEndpoint::bind(
            services,
            args,
            Arc::clone(host),
            stop,
            reload,
            switches,
        ))?;
        eprintln!("plak host: web on {}", endpoint.listener());
        Ok((Some(endpoint), None))
    } else {
        let idle_reload = runtime.spawn(async move {
            let mut reload = reload;
            while reload.recv().await.is_some() {
                eprintln!("plak host: no UI build to reload");
            }
        });
        Ok((None, Some(idle_reload)))
    }
}

fn hand_off(host: &SessionHost<crate::surface_protocol::PlakExtension>) {
    let deadline = HANDOFF_DEADLINE.saturating_sub(STOP_MARGIN);
    for (session, failure) in host.hand_off_all(deadline) {
        eprintln!(
            "plak host: session {session} was not handed off: {}",
            failure.message()
        );
    }
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
    stopped: impl Fn() -> bool + Send + 'static,
    services: &Services,
    runtime: &tokio::runtime::Handle,
    scanning: Arc<std::sync::atomic::AtomicBool>,
) {
    if owed.is_empty() {
        return;
    }
    let host = Arc::clone(host);
    let services = services.clone();
    let runtime = runtime.clone();
    let mut pending: Vec<_> = owed
        .into_iter()
        .map(|session| {
            let slot = host.slot(&session);
            slot.set_startup_retrying(true);
            PendingOpen::new(session, slot)
        })
        .collect();
    std::thread::spawn(move || {
        let mut changed = services.runners().changes();
        let mut runner_changed = false;
        while !pending.is_empty() && !stopped() {
            pending.retain_mut(|pending| {
                if stopped() || (!runner_changed && Instant::now() < pending.next) {
                    return true;
                }
                let session = &pending.session;
                // A Stop or changed loss policy wins over an earlier retry.
                match sessions::continuation_owed(&services, session) {
                    Ok(false) => return false,
                    Err(failure) => {
                        eprintln!(
                            "plak host: cannot check continuation {session}: {}",
                            failure.message()
                        );
                        return false;
                    }
                    Ok(true) => {}
                }
                if !placement_ready(&services, session) {
                    pending.defer();
                    return true;
                }
                eprintln!("plak host: continuing session {session}");
                let result = host.slot(session).resident();
                host.changed();
                match result {
                    Ok(_) => false,
                    Err(failure) => {
                        eprintln!(
                            "plak host: session {session} did not continue: {}",
                            failure.message()
                        );
                        if !failure.can_retry_open() {
                            return false;
                        }
                        pending.defer();
                        true
                    }
                }
            });
            if !pending.is_empty() && !stopped() {
                runner_changed = runtime.block_on(async {
                    tokio::select! {
                        result = changed.changed() => result.is_ok(),
                        () = tokio::time::sleep(Duration::from_secs(1)) => false,
                    }
                });
            }
        }
        drop(pending);
        host.changed();
        scanning.store(false, std::sync::atomic::Ordering::Release);
    });
}

struct PendingOpen {
    session: String,
    slot: Arc<SessionSlot<crate::surface_protocol::PlakExtension>>,
    next: Instant,
    delay: Duration,
}

impl PendingOpen {
    fn new(
        session: String,
        slot: Arc<SessionSlot<crate::surface_protocol::PlakExtension>>,
    ) -> Self {
        Self {
            session,
            slot,
            next: Instant::now(),
            delay: Duration::from_secs(1),
        }
    }

    fn defer(&mut self) {
        self.next = Instant::now() + self.delay;
        self.delay = (self.delay * 2).min(Duration::from_secs(30));
    }
}

impl Drop for PendingOpen {
    fn drop(&mut self) {
        self.slot.set_startup_retrying(false);
    }
}

fn placement_ready(services: &Services, session: &str) -> bool {
    let Ok(paths) = services.paths() else {
        return false;
    };
    let Ok(mut product) = crate::store::ProductStore::open(paths) else {
        return false;
    };
    let Ok(record) = product.load(session) else {
        return false;
    };
    let runner = record
        .runner
        .as_ref()
        .map_or("local", |runner| runner.runner_id.as_str());
    let ready = services.runners().rows().iter().any(|row| {
        row.runner.runner_id == runner && row.status == plakous::runner::RunnerStatus::Ready
    });
    if !ready {
        services.runners().require_connection(runner, true);
    }
    ready
}
