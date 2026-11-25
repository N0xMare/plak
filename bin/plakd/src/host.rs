//! `plakd host` (feature `web`, enabled by default): the per-user host process (spec 02
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
mod child_shutdown;
pub(crate) mod connection;
mod console;
mod gateway_startup;
mod idle;
mod legacy_upgrade;
pub(crate) mod lifecycle;
mod lock;
pub(crate) mod reexec;
pub(crate) use reexec::receive as receive_reexec;
mod requested_reload;
pub(crate) mod send;
mod sessions;
mod socket;
mod starter;
mod store_revisions;
mod upgrade;

use crate::terminal_registry as terminals;

#[cfg(test)]
mod scan_tests;

pub(crate) const AVAILABLE: bool = true;

pub(crate) fn preflight_upgrade_service(
    services: &Services,
    command: &HostCommand,
    probe: &std::path::Path,
) -> CliResult<()> {
    let paths = services.paths()?;
    legacy_upgrade::refuse_unresolved(paths)?;
    crate::product_config::ProductConfig::load_user(paths).map_err(CliFailure::new)?;
    if let Some(web) = &command.web {
        crate::web::preflight(services, web)?;
    }
    upgrade::health::stores(paths)?;
    store_revisions::rehearse_for_service(paths, probe)
}

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
pub(crate) const HANDOFF_DEADLINE: Duration = Duration::from_secs(50);

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
    drain: crate::command::HostDrain,
    backup_store: bool,
    inspection: Option<crate::deploy::DeployRunnerInspection>,
) -> CliResult<ProcessOutcome> {
    upgrade::request(services, expect_binary, drain, backup_store, inspection)
}

fn startup_runtime() -> CliResult<(
    tokio::runtime::Runtime,
    tokio::signal::unix::Signal,
    crate::web::StopProbe,
)> {
    let workers = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers.clamp(4, 16))
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("plakd host could not start its runtime"))?;
    let entered = runtime.enter();
    let reload = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .map_err(|error| CliFailure::new(format!("cannot listen for UI reload: {error}")))?;
    drop(entered);
    // A stop that arrives during the start still ends the process within the deadline.
    crate::signals::install_bounded(crate::signals::cli_announcement(), HANDOFF_DEADLINE, || {});
    crate::signals::count_interrupts();
    Ok((runtime, reload, crate::web::StopProbe::armed()))
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
    inherited: Option<reexec::Inherited>,
) -> CliResult<ProcessOutcome> {
    match command.start {
        crate::command::HostStart::Helper => return starter::helper(services.paths()?),
        crate::command::HostStart::Supervisor => return starter::supervise(services.paths()?),
        crate::command::HostStart::Foreground | crate::command::HostStart::Detached => {}
    }
    let paths = services.paths()?;
    let checkpoint = match &inherited {
        Some(transport) if !transport.fallback => upgrade::checkpoint::Checkpoint::inherited(
            paths,
            crate::build_identity::binary_id(),
            transport.source_instance.as_deref(),
        )?,
        _ => None,
    };
    if let Some(checkpoint) = &checkpoint {
        checkpoint.ticket.record_pending(paths)?;
    }
    let mut gate = UpgradeGate {
        checkpoint,
        evidence: None,
        stage: upgrade::checkpoint::Failure::Startup,
    };
    let outcome = serve_host(services, command, inherited, &mut gate);
    if let (Err(error), Some(checkpoint)) = (&outcome, gate.checkpoint) {
        eprintln!("host upgrade: {}", error.message());
        checkpoint.ticket.record_failure(paths, gate.stage);
        return Err(gate.stage.fix_forward());
    }
    outcome
}

struct UpgradeGate {
    checkpoint: Option<upgrade::checkpoint::Checkpoint>,
    evidence: Option<upgrade::checkpoint::EvidenceLevel>,
    stage: upgrade::checkpoint::Failure,
}

impl UpgradeGate {
    fn verify(&mut self, paths: &crate::home::paths::PlakPaths) -> CliResult<()> {
        let Some(checkpoint) = &mut self.checkpoint else {
            return Ok(());
        };
        self.stage = upgrade::checkpoint::Failure::Health;
        let _writers = crate::carry::upgrade::admission_until(
            paths,
            true,
            std::time::Instant::now() + upgrade::health::ADMISSION_TIME,
        )?;
        upgrade::health::stores(paths)?;
        self.stage = upgrade::checkpoint::Failure::Preservation;
        let (evidence, baseline) = checkpoint.verify_destination(paths)?;
        self.stage = upgrade::checkpoint::Failure::Health;
        upgrade::bindings::health(paths, &baseline)?;
        self.evidence = Some(evidence);
        crate::carry::upgrade::crash_cut("after-destination-verification");
        self.stage = upgrade::checkpoint::Failure::Startup;
        Ok(())
    }

    fn ready(&mut self, paths: &crate::home::paths::PlakPaths) -> CliResult<()> {
        if let Some(checkpoint) = &self.checkpoint {
            let evidence = self
                .evidence
                .ok_or_else(|| CliFailure::new("missing preservation verdict"))?;
            checkpoint.ticket.record_ready(paths, evidence)?;
            self.checkpoint = None;
        }
        Ok(())
    }
}

pub(crate) fn abort_child_retirement(
    paths: &crate::product_paths::PlakPaths,
    tree_id: &str,
    child: &str,
    stdout: &mut dyn std::io::Write,
) -> CliResult<()> {
    use plakous_hosted::agent_tree::TreeStore as _;

    if !paths.subagents_database().is_file() {
        return Err(CliFailure::new("no persisted agent tree store"));
    }
    paths.ensure_private_state_home().map_err(CliFailure::new)?;
    let _host = lock::HostLock::take_home(paths, crate::build_identity::binary_id())?;
    let _writers = crate::carry::upgrade::admission(paths, true)?;
    crate::store::revisions::refuse_incompatible(paths).map_err(CliFailure::new)?;
    let store = crate::store::tree::open_tree_store(paths).map_err(CliFailure::new)?;
    let mut tree = store
        .load(tree_id)
        .map_err(|error| CliFailure::new(error.detail))?
        .ok_or_else(|| CliFailure::new("unknown agent tree"))?
        .rows()
        .clone();
    let locator = tree
        .child_retirement_abort_locator(child)
        .ok_or_else(|| CliFailure::new("no abortable child retirement"))?;
    let runtime = locator
        .parse()
        .map_err(|_| CliFailure::new("invalid retirement locator"))?;
    if !plakous::sessions::AgentSessions::child_retirement_can_abort_sqlite(
        paths.runtime_database(),
        runtime,
    )
    .map_err(|error| CliFailure::new(error.to_string()))?
    {
        return Err(CliFailure::new(
            "child runtime is archived, deleted, or missing; retirement cannot be aborted",
        ));
    }
    tree.abort_child_retirement(child, &locator)
        .map_err(|error| CliFailure::new(error.message))?;
    let revision = store
        .save_in_custody(&tree)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    tree.acknowledge_revision(revision);
    writeln!(stdout, "aborted retirement of {child} in tree {tree_id}")
        .map_err(|_| CliFailure::new("stdout failed"))
}

fn serve_host(
    services: &Services,
    command: &HostCommand,
    inherited: Option<reexec::Inherited>,
    gate: &mut UpgradeGate,
) -> CliResult<ProcessOutcome> {
    let host_arguments = Arc::new(upgrade::host_argv(command)?);
    // Arm reload before any startup I/O, even for a control-only host.
    let (runtime, reload, stop) = startup_runtime()?;
    let paths = services.paths()?;
    // Custody is resolved once, here, and every consumer takes this value
    // through `services`; `[gateway]` changes need a host restart.
    let custody = services.custody()?;
    let _ = services.child_retirement_policy()?;
    runtime.block_on(refuse_external_gateway(services, &custody))?;
    paths.ensure_private_state_home().map_err(CliFailure::new)?;
    crate::store::revisions::refuse_incompatible(paths).map_err(CliFailure::new)?;
    paths.ensure_layout().map_err(CliFailure::new)?;
    let binary = crate::build_identity::known_binary_id().unwrap_or_else(|| "unknown".into());
    let (ownership, inherited) = HostOwnership::take(paths, &binary, inherited)?;
    if inherited.is_none() {
        upgrade::checkpoint::refuse_cold_start(paths)?;
    }
    legacy_upgrade::refuse_unresolved(paths)?;
    let paired_start = inherited.is_some();
    let stores_pending = store_revisions::pending(paths)?;
    let web_policy = command
        .web
        .as_ref()
        .map(|args| crate::web::WebPolicy::load(services, args))
        .transpose()?;
    let (listeners, environment) = HostListeners::bind(
        paths,
        web_policy.as_ref(),
        &runtime,
        &ownership.state,
        inherited,
    )?;
    store_revisions::upgrade_stopped(paths, stores_pending, &ownership.state)?;
    gate.verify(paths)?;
    let environment = environment.unwrap_or_else(environment::HostEnvironment::capture);
    let opened = open_host(
        services,
        command.idle_release,
        &runtime,
        HostSignals { reload, stop },
        web_policy.zip(listeners.web.as_ref()),
        &environment,
        &binary,
    )?;
    let inventory = if paired_start {
        StartupInventory::Discover(Arc::clone(&opened.product))
    } else {
        StartupInventory::Known(opened.product.owed()?)
    };
    announce_host(&listeners.control, &environment, paths, &custody);
    let _store_monitor = runtime.spawn(crate::runtime_headroom::watch(
        paths.clone(),
        services.runtime_store_bounds()?,
    ));
    let owner = RunningHost {
        listeners,
        ownership,
        opened,
        inventory,
        runtime,
        environment,
        stop,
        binary,
    };
    owner.run(command.start, host_arguments, gate)
}

struct HostOwnership {
    state: lock::HostLock,
    home: lock::HostLock,
}

struct InheritedListeners {
    control: std::os::fd::OwnedFd,
    web: Option<std::os::fd::OwnedFd>,
    environment: environment::HostEnvironment,
}

impl HostOwnership {
    fn take(
        paths: &crate::home::paths::PlakPaths,
        binary: &str,
        inherited: Option<reexec::Inherited>,
    ) -> CliResult<(Self, Option<InheritedListeners>)> {
        let Some(inherited) = inherited else {
            let (home, state) = lock::HostLock::take_home(paths, binary)?;
            return Ok((Self { state, home }, None));
        };
        let home = lock::HostLock::adopt(
            inherited.home_lock,
            &crate::wire_transport::hosts::home_lock(paths),
            binary,
        )?;
        let state = lock::HostLock::adopt(inherited.state_lock, &paths.host_lock(), binary)?;
        Ok((
            Self { state, home },
            Some(InheritedListeners {
                control: inherited.control,
                web: inherited.web,
                environment: inherited.environment,
            }),
        ))
    }
}

struct HostListeners {
    control: socket::Bound,
    web: Option<Arc<crate::web::Listener>>,
}

impl HostListeners {
    fn bind(
        paths: &crate::home::paths::PlakPaths,
        web_policy: Option<&crate::web::WebPolicy>,
        runtime: &tokio::runtime::Runtime,
        state: &lock::HostLock,
        inherited: Option<InheritedListeners>,
    ) -> CliResult<(Self, Option<environment::HostEnvironment>)> {
        let (control, web, environment) = match inherited {
            Some(inherited) => (
                Some(inherited.control),
                inherited.web,
                Some(inherited.environment),
            ),
            None => (None, None, None),
        };
        let entered = runtime.enter();
        let control = match control {
            Some(fd) => socket::Bound::adopt(fd, &paths.host_socket(), state)?,
            None => socket::Bound::bind(&paths.host_socket(), state)?,
        };
        drop(entered);
        let web = match (web_policy, web) {
            (Some(policy), fd) => Some(Arc::new(runtime.block_on(policy.bind_listener(fd))?)),
            (None, None) => None,
            (None, Some(_)) => return Err(CliFailure::new("unexpected inherited web listener")),
        };
        Ok((Self { control, web }, environment))
    }

    fn remove(&self) {
        self.control.remove();
        if let Some(web) = &self.web {
            web.remove();
        }
    }
}

struct RunningHost {
    listeners: HostListeners,
    ownership: HostOwnership,
    opened: OpenedHost,
    inventory: StartupInventory,
    runtime: tokio::runtime::Runtime,
    environment: environment::HostEnvironment,
    stop: crate::web::StopProbe,
    binary: String,
}

impl RunningHost {
    fn run(
        self,
        start: crate::command::HostStart,
        host_arguments: Arc<Vec<String>>,
        gate: &mut UpgradeGate,
    ) -> CliResult<ProcessOutcome> {
        let Self {
            mut listeners,
            ownership,
            opened,
            inventory,
            runtime,
            environment,
            stop,
            binary,
        } = self;
        let OpenedHost {
            upgrade_listener,
            services,
            terminals,
            host,
            product: _,
            web,
            idle_reload,
        } = opened;
        let paths = services.paths()?;
        let scanning = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut startup = startup_scan(&services, &host, &runtime, stop, inventory, &scanning)?;
        let reload_requests = runtime.spawn(requested_reload::serve(
            Arc::clone(&host),
            paths.clone(),
            binary,
        ));
        let idle = (start == crate::command::HostStart::Detached).then(idle::IdleExit::default);
        ownership.state.running()?;
        ownership.home.running()?;
        gate.ready(paths)?;
        // The web endpoint serves beside the control socket and stops with it.
        let web = web.map(|web| runtime.spawn(web.serve()));
        let upgrades = upgrade::Coordinator::start(
            &runtime,
            upgrade_listener,
            &host,
            &services,
            Arc::clone(&host_arguments),
            Arc::clone(&scanning),
            startup.ready.take(),
        );
        let pending = upgrades.pending();
        let marker_pending = Arc::clone(&pending);
        let startup_failed = Arc::clone(&startup.failed);
        let stopping = runtime.block_on(listeners.control.serve(
            &host,
            move || {
                stop.stopped()
                    || startup_failed.load(std::sync::atomic::Ordering::Acquire)
                    || upgrade::has_ready(&pending)
            },
            idle,
            scanning,
            || {
                let restarting = !stop.stopped() && upgrade::has_ready(&marker_pending);
                let _ = ownership.state.stopping(restarting);
                let _ = ownership.home.stopping(restarting);
            },
        ));
        let ready = upgrades.stop(&runtime, paths);
        let paired = !stop.stopped() && ready.is_some();
        lifecycle::stopping(&host);
        runtime.block_on(host.wait_lifecycle_delivery(Duration::from_secs(1)));
        stop_listeners(&runtime, reload_requests, web, idle_reload, ready.is_some());
        // The socket goes first, so no client reaches a host that is handing
        // its sessions off, and before the lock: a host that takes the lock
        // next finds no socket of this one.
        if !paired {
            listeners.remove();
        }
        runtime.block_on(terminals.stop());
        // Joined release closes the host to new sessions.
        hand_off(&host, &runtime, stopping, ready.as_ref().filter(|_| paired));
        let scan_result = startup.join();
        eprintln!(
            "plakd host: stopped in {} ms",
            stopping.elapsed().as_millis()
        );
        shutdown_runtime(runtime);
        scan_result?;
        if paired {
            let ready = ready.ok_or_else(|| CliFailure::new("missing paired host upgrade"))?;
            if stop.stopped() {
                listeners.remove();
                return Ok(ProcessOutcome::Success);
            }
            return reexec::replace(
                ready,
                paths.home(),
                &host_arguments,
                &environment,
                &ownership,
                listeners,
            );
        }
        Ok(ProcessOutcome::Success)
    }
}

struct HostSignals {
    reload: tokio::signal::unix::Signal,
    stop: crate::web::StopProbe,
}

struct OpenedHost {
    upgrade_listener: tokio::net::UnixListener,
    services: Services,
    terminals: Arc<terminals::TerminalRegistry>,
    host: Arc<SessionHost<crate::surface_protocol::PlakExtension>>,
    product: Arc<sessions::ProductSessions>,
    web: Option<crate::web::WebEndpoint>,
    idle_reload: Option<tokio::task::JoinHandle<()>>,
}

fn open_host(
    services: &Services,
    idle_release: Option<Duration>,
    runtime: &tokio::runtime::Runtime,
    signals: HostSignals,
    web: Option<(crate::web::WebPolicy, &Arc<crate::web::Listener>)>,
    environment: &environment::HostEnvironment,
    binary: &str,
) -> CliResult<OpenedHost> {
    let paths = services.paths()?;
    crate::store::tree::bind_custody_host(paths).map_err(CliFailure::new)?;
    let (services, terminals) = terminals::TerminalRegistry::compose(services, host_generation())?;
    let (services, local) = runner::host::prepare(&services, None)?;
    let session_runtime = Some(runtime.handle().clone());
    let mut sessions = match &web {
        Some((policy, _)) => sessions::ProductSessions::with_switches(
            &services,
            idle_release,
            session_runtime,
            policy.switches,
        ),
        None => sessions::ProductSessions::new(&services, idle_release, session_runtime),
    }?;
    sessions.serve_directory(runtime.handle())?;
    let product = Arc::new(sessions);
    let host = SessionHost::new_shared(product.clone(), binary.to_owned(), terminals.generation());
    terminals.serve(&host, runtime.handle())?;
    let (web, idle_reload) = compose_web(runtime, &services, web, &host, signals)?;
    let entered = runtime.enter();
    let upgrade_listener = upgrade::bind(paths)?;
    drop(entered);
    runner::host::start(&services, local, environment, runtime);
    Ok(OpenedHost {
        upgrade_listener,
        services,
        terminals,
        host,
        product,
        web,
        idle_reload,
    })
}

/// A gateway host carries no provider key variables, and a unix gateway
/// must hold this host's own state store at the configured socket.
async fn refuse_external_gateway(
    services: &Services,
    custody: &crate::gateway::HostCustody,
) -> CliResult<()> {
    let set = crate::environment::host::provider_key_env_set();
    if !set.is_empty() {
        return Err(CliFailure::new(format!(
            "a host with [gateway] address set refuses provider key environment variables: {}",
            set.join(", ")
        )));
    }
    gateway_startup::verify(custody, services.home().state()?).await
}

fn startup_scan(
    services: &Services,
    host: &Arc<SessionHost<crate::surface_protocol::PlakExtension>>,
    runtime: &tokio::runtime::Runtime,
    stop: crate::web::StopProbe,
    inventory: StartupInventory,
    scanning: &Arc<std::sync::atomic::AtomicUsize>,
) -> CliResult<StartupScan> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let failed = Arc::new(AtomicBool::new(false));
    let product = match inventory {
        StartupInventory::Known(owed) => {
            scan(
                host,
                owed,
                move || stop.stopped(),
                services,
                runtime.handle(),
                Arc::clone(scanning),
            );
            return Ok(StartupScan {
                worker: None,
                failed,
                ready: None,
            });
        }
        StartupInventory::Discover(product) => product,
    };
    // Capture owed work before admitting any request that could change it.
    // Welcome and directory observations don't need this recovery inventory.
    let pause = host.pause_admissions()?;
    let (host, services, runtime, scanning) = (
        Arc::clone(host),
        services.clone(),
        runtime.handle().clone(),
        Arc::clone(scanning),
    );
    let worker_failed = Arc::clone(&failed);
    let (settled, ready) = tokio::sync::oneshot::channel();
    let worker = std::thread::Builder::new()
        .name("plakd-startup-scan".into())
        .spawn(move || {
            let finished = owed_at_start(&product, stop).map(|owed| {
                scan(
                    &host,
                    owed,
                    move || stop.stopped(),
                    &services,
                    &runtime,
                    scanning,
                );
            });
            let hold = if finished.is_err() {
                worker_failed.store(true, Ordering::Release);
                // Keep ingress paused until the host has closed.
                Some(pause)
            } else {
                drop(pause);
                let _ = settled.send(());
                None
            };
            (finished, hold)
        })
        .map_err(|error| CliFailure::new(error.to_string()))?;
    Ok(StartupScan {
        worker: Some(worker),
        failed,
        ready: Some(ready),
    })
}

enum StartupInventory {
    Known(Vec<String>),
    Discover(Arc<sessions::ProductSessions>),
}

fn owed_at_start(
    product: &sessions::ProductSessions,
    stop: crate::web::StopProbe,
) -> CliResult<Vec<String>> {
    while crate::test_hooks::host_lifecycle::continuation_scan_blocked() {
        if stop.stopped() {
            return Ok(Vec::new());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    product.owed()
}

type InventoryResult = (
    CliResult<()>,
    Option<plakous_hosted::host_service::wire::AdmissionPause>,
);

struct StartupScan {
    worker: Option<std::thread::JoinHandle<InventoryResult>>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    ready: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl StartupScan {
    fn join(self) -> CliResult<()> {
        let Some(worker) = self.worker else {
            return Ok(());
        };
        let (result, _pause) = worker
            .join()
            .map_err(|_| CliFailure::new("host continuation scan panicked"))?;
        result
    }
}

fn announce_host(
    bound: &socket::Bound,
    environment: &environment::HostEnvironment,
    paths: &crate::home::paths::PlakPaths,
    custody: &crate::gateway::HostCustody,
) {
    crate::shell::catalog_refresh::hook(paths.clone(), custody.clone(), None)(None);
    let identity = plakous_recursive_subagents::tree::custody_host();
    let gateway = custody.label();
    eprintln!(
        "plakd host: serving on {} (host {}, instance {}); gateway: {gateway}; tools get {}",
        bound.path().display(),
        identity.host_id,
        identity.instance,
        environment.describe(),
    );
}

fn shutdown_runtime(runtime: tokio::runtime::Runtime) {
    runtime.shutdown_timeout(Duration::from_secs(1));
    child_shutdown::close();
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

fn compose_web(
    runtime: &tokio::runtime::Runtime,
    services: &Services,
    web: Option<(crate::web::WebPolicy, &Arc<crate::web::Listener>)>,
    host: &Arc<SessionHost<crate::surface_protocol::PlakExtension>>,
    signals: HostSignals,
) -> CliResult<(
    Option<crate::web::WebEndpoint>,
    Option<tokio::task::JoinHandle<()>>,
)> {
    if let Some((policy, listener)) = web {
        let endpoint = crate::web::WebEndpoint::new(
            services,
            policy,
            Arc::clone(host),
            signals.stop,
            signals.reload,
            Arc::clone(listener),
        )?;
        eprintln!("plakd host: web on {}", endpoint.listener());
        Ok((Some(endpoint), None))
    } else {
        let idle_reload = runtime.spawn(async move {
            let mut reload = signals.reload;
            while reload.recv().await.is_some() {
                eprintln!("plakd host: no UI build to reload");
            }
        });
        Ok((None, Some(idle_reload)))
    }
}

fn hand_off(
    host: &SessionHost<crate::surface_protocol::PlakExtension>,
    runtime: &tokio::runtime::Runtime,
    stopping: Instant,
    paired: Option<&upgrade::Ready>,
) {
    if let Some(ready) = paired {
        runtime.block_on(ready.record_child_handoffs(handoff_remaining(stopping.elapsed())));
    }
    let elapsed = stopping.elapsed();
    let deadline = handoff_remaining(elapsed);
    crate::test_hooks::host_lifecycle::record_handoff_budget(elapsed, deadline);
    for (session, failure) in host.hand_off_all(deadline) {
        eprintln!(
            "plakd host: session {session} was not handed off: {}",
            failure.message()
        );
    }
}

fn handoff_remaining(elapsed: Duration) -> Duration {
    HANDOFF_DEADLINE
        .saturating_sub(elapsed)
        .saturating_sub(STOP_MARGIN)
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
    scanning: Arc<std::sync::atomic::AtomicUsize>,
) {
    if owed.is_empty() {
        eprintln!("plakd host: continuation scan complete");
        return;
    }
    scanning.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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
                            "plakd host: cannot check continuation {session}: {}",
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
                eprintln!("plakd host: continuing session {session}");
                let result = host.slot(session).resident();
                host.changed();
                match result {
                    Ok(_) => false,
                    Err(failure) => {
                        eprintln!(
                            "plakd host: session {session} did not continue: {}",
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
        scanning.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        eprintln!("plakd host: continuation scan complete");
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
