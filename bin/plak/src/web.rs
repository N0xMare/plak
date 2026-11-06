//! `plak host --web` (feature `web`): the host's sessions' wire v2 over a
//! WebSocket at `/wire`, and the web UI's static files, on a private
//! listener (spec 02 §4.5, spec 04 §2.10).
//!
//! The listener is a unix socket in a 0700 directory, which `tailscale
//! serve unix:<path>` proxies to, or, in dev mode only, 127.0.0.1. Every
//! request passes the same gate (`auth`): the tailnet identity Serve stamps
//! (`Tailscale-User-Login`), trusted only on the unix listener and checked
//! against `[web] allow`, the exact `Host`, and on `/wire` the exact
//! `Origin` and the session cookie the gate issued. Everything refused gets
//! the same `403`. Each upgraded connection is a client of the host's
//! session directory (`SessionHost`), as one on the control socket is: it
//! lists, creates and attaches sessions by id, and a connection that leaves
//! detaches while the sessions run on.

mod auth;
mod forwarded;
mod listen;
mod socket;
mod ui;

use std::sync::Arc;

use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::host_service::wire::SessionHost;

use crate::product_config::ProductConfig;
use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;

use auth::{WebAccess, WebOrigin};
use listen::Listen;

/// What every request to the endpoint shares.
pub(crate) struct Served {
    /// The sessions the host serves.
    pub(crate) host: Arc<SessionHost<PlakExtension>>,
    /// Whether the process was asked to stop since it began serving.
    pub(crate) stop: StopProbe,
    pub(crate) ui: Option<ui::Ui>,
    pub(crate) gate: auth::Gate,
}

/// The bound web endpoint of a host.
pub(crate) struct WebEndpoint {
    listen: Listen,
    listener: listen::Bound,
    socket_file: Option<listen::SocketFile>,
    served: Arc<Served>,
}

impl WebEndpoint {
    /// Binds the endpoint `args` name for `host`, in the caller's runtime.
    ///
    /// # Errors
    ///
    /// A listener that is not private or cannot be bound, `[web]` keys that
    /// do not admit anyone, a UI directory without its CSP, or a cookie key
    /// that cannot be read or made.
    pub(crate) async fn bind(
        services: &Services,
        args: &crate::command::HostWeb,
        host: Arc<SessionHost<PlakExtension>>,
        stop: StopProbe,
    ) -> CliResult<Self> {
        let listen = Listen::parse(&args.listen).map_err(CliFailure::new)?;
        let paths = services.paths()?;
        let config = ProductConfig::load(paths).map_err(CliFailure::new)?;
        let access = access(&listen, &config, args.dev_login.clone())?;
        let ui = args.ui.as_deref().map(ui::Ui::load).transpose()?;
        let gate = auth::Gate::new(access, auth::CookieKey::load_or_create(paths)?);
        let (listener, socket_file) = listen::bind(&listen).await?;
        Ok(Self {
            listen,
            listener,
            socket_file,
            served: Arc::new(Served {
                host,
                stop,
                ui,
                gate,
            }),
        })
    }

    /// What the start line names.
    pub(crate) fn listener(&self) -> String {
        self.listen.to_string()
    }

    /// Serves until the process is asked to stop, then removes the socket
    /// it bound.
    pub(crate) async fn serve(self) {
        tracing::info!(target: "plak::web", listener = %self.listen, "plak host web endpoint serving");
        listen::serve(self.listener, Arc::clone(&self.served)).await;
        listen::cleanup(self.socket_file);
    }
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
                    "plak host --web: set [web] origin to the origin tailscale serve publishes",
                )
            })?;
            if config.web_allow.is_empty() {
                return Err(CliFailure::new(
                    "plak host --web: [web] allow names no login, so no one could connect",
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
            "plak host: --dev-login is for a loopback listener only",
        )),
        (Listen::Loopback(_), None) => Err(CliFailure::new(
            "plak host --web: the tailnet identity is trusted only on a unix listener; a loopback listener is dev mode and needs --dev-login",
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
    pub(crate) fn armed() -> Self {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
