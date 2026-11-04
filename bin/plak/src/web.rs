//! `plak web` (hidden): one session's wire v2 over a WebSocket at `/wire`,
//! and the web UI's static files, on a private listener (spec 02 §4.6,
//! spec 04 §2.10).
//!
//! The listener is a unix socket in a 0700 directory, which `tailscale
//! serve unix:<path>` proxies to, or, in dev mode only, 127.0.0.1. Every
//! request passes the same gate (`auth`): the tailnet identity Serve stamps
//! (`Tailscale-User-Login`), trusted only on the unix listener and checked
//! against `[web] allow`, the exact `Host`, and on `/wire` the exact
//! `Origin` and the session cookie the gate issued. Everything refused gets
//! the same `403`. The session is served through its shared host service
//! (`WireSession`): a connection that leaves detaches, and the session runs
//! until it ends or the process is stopped. A switch to another session
//! (`/new`, `/resume`, a rebind) moves every attachment to its root, and a
//! later connection is welcomed to it. Everything a request needs is one
//! `Served`, so a host serving many sessions mounts one per session.

mod auth;
mod forwarded;
mod listen;
mod socket;
mod ui;

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_host_protocol::wire::Welcome;
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};
use plakous_hosted::host_service::wire::{
    WireSession, session_root, session_root_of, session_welcome,
};
use plakous_hosted::runtime::spawn_host_worker;

use crate::command::WebCommand;
use crate::product_config::ProductConfig;
use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;

use auth::{WebAccess, WebOrigin};
use listen::Listen;

/// How long the process waits for the session to end after asking it to.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(30);

/// One served session and what every request to it shares.
pub(crate) struct Served {
    pub(crate) session: WireSession<PlakExtension>,
    /// Whether the process was asked to stop since it began serving.
    pub(crate) stop: StopProbe,
    binary_id: String,
    pub(crate) ui: Option<ui::Ui>,
    pub(crate) gate: auth::Gate,
}

impl Served {
    /// The welcome a connection gets: it names the session served now.
    pub(crate) fn welcome(&self) -> Welcome {
        session_welcome(
            self.binary_id.clone(),
            host_generation(),
            self.session.session_id(),
            self.session.root(),
        )
    }
}

/// Serves one headless session until it ends or the process is stopped.
///
/// # Errors
///
/// A listener that is not private or cannot be bound, `[web]` keys that do
/// not admit anyone, a UI directory without its CSP, a cookie key that
/// cannot be read or made, or a session that does not open.
pub(crate) fn run_web_command(
    services: &Services,
    command: WebCommand,
) -> CliResult<ProcessOutcome> {
    // Armed before the session opens, as for `plak agent`: a stop that
    // arrives first ends the process with its announcement.
    crate::signals::install(crate::signals::cli_announcement(), || {});
    crate::signals::count_interrupts();
    let listen = Listen::parse(&command.listen).map_err(CliFailure::new)?;
    let paths = services.paths()?;
    let config = ProductConfig::load(paths).map_err(CliFailure::new)?;
    let access = access(&listen, &config, command.dev_login.clone())?;
    let ui = command.ui.as_deref().map(ui::Ui::load).transpose()?;
    let gate = auth::Gate::new(access, auth::CookieKey::load_or_create(paths)?);
    // Bound before the session opens, so an endpoint that cannot take its
    // socket (another one serves there) opens nothing.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("plak web could not start its runtime"))?;
    let (listener, socket_file) = runtime.block_on(listen::bind(&listen))?;
    let host = match open_session(services, &command) {
        Ok(host) => host,
        Err(failure) => {
            listen::cleanup(socket_file);
            return Err(failure);
        }
    };
    let root = session_root(&host);
    let session_id = host.session.id.clone();
    let state_roots = host.state_roots.clone();
    let (actions, events, worker) = match spawn_host_worker(host) {
        Ok(worker) => worker,
        Err(failure) => {
            listen::cleanup(socket_file);
            return Err(failure);
        }
    };
    let served = Arc::new(Served {
        session: WireSession::start(root, session_id, actions, events, move |id| {
            session_root_of(&state_roots, id)
        }),
        stop: StopProbe::armed(),
        binary_id: crate::build_identity::known_binary_id().unwrap_or_else(|| "unknown".into()),
        ui,
        gate,
    });
    tracing::info!(target: "plak::web", listener = %listen, "plak web serving");
    eprintln!("plak web: serving on {listen}");
    runtime.block_on(listen::serve(listener, Arc::clone(&served)));
    served.session.quit();
    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err(CliFailure::new(
                "plak web host worker exceeded its shutdown deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    worker
        .join()
        .map_err(|_| CliFailure::new("plak web host worker failed"))??;
    runtime.shutdown_timeout(Duration::from_secs(1));
    listen::cleanup(socket_file);
    Ok(ProcessOutcome::Success)
}

/// A headless session on the root template's binding, as `plak agent` opens
/// one with no overrides.
fn open_session(
    services: &Services,
    command: &WebCommand,
) -> CliResult<plakous_hosted::runtime::HostConfig<PlakExtension>> {
    // The session's workspace is explicit: `--cwd`, or the launch directory.
    // The process working directory is never changed.
    let context = match &command.cwd {
        Some(cwd) => {
            let workspace = std::fs::canonicalize(cwd).map_err(|_| {
                CliFailure::new(format!("plak web: no workspace {}", cwd.display()))
            })?;
            Some(services.paths()?.for_workspace(&workspace))
        }
        None => None,
    };
    crate::commands::agent::run_agent_after_admission(
        services,
        context.as_ref(),
        command.session.clone(),
        None,
        None,
        None,
        None,
        None,
        false,
        None,
    )
}

/// Who the listener admits: the tailnet logins `[web] allow` names, at
/// `[web] origin`, on the unix listener; or, with `--dev-login` on
/// loopback, that one login at `http://127.0.0.1:<port>`.
fn access(
    listen: &Listen,
    config: &ProductConfig,
    dev_login: Option<String>,
) -> CliResult<WebAccess> {
    match (listen, dev_login) {
        (Listen::Unix(_), None) => {
            let origin = config.web_origin.as_deref().ok_or_else(|| {
                CliFailure::new(
                    "plak web: set [web] origin to the origin tailscale serve publishes",
                )
            })?;
            if config.web_allow.is_empty() {
                return Err(CliFailure::new(
                    "plak web: [web] allow names no login, so no one could connect",
                ));
            }
            Ok(WebAccess::Tailnet {
                allow: config.web_allow.clone(),
                origin: WebOrigin::parse(origin)?,
            })
        }
        (Listen::Loopback(address), Some(login)) => Ok(WebAccess::Dev {
            login,
            origin: WebOrigin::parse(&format!("http://{address}"))?,
        }),
        (Listen::Unix(_), Some(_)) => Err(CliFailure::new(
            "plak web: --dev-login is for a loopback listener only",
        )),
        (Listen::Loopback(_), None) => Err(CliFailure::new(
            "plak web: the tailnet identity is trusted only on a unix listener; a loopback listener is dev mode and needs --dev-login",
        )),
    }
}

/// Whether `SIGINT` or a terminating signal arrived since the probe was
/// armed: the process was asked to stop. The accept loop, every hello wait
/// and every connection read it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StopProbe {
    interrupts: usize,
    stops: usize,
}

impl StopProbe {
    fn armed() -> Self {
        Self {
            interrupts: crate::signals::interrupts(),
            stops: crate::signals::signalled_stops(),
        }
    }

    pub(crate) fn stopped(self) -> bool {
        crate::signals::interrupts() != self.interrupts
            || crate::signals::signalled_stops() != self.stops
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
