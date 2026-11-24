//! The host's side of a runner link (spec 04): the host dials its runner
//! over ssh with fixed strict options and runs `plakd runner connect --stdio`
//! there. It never listens: there is no dial-in.

pub(crate) mod children;

use std::process::{Command, ExitStatus};

use plakous::runner::protocol::{Hello, VersionTriple};
use plakous::runner::{CallJournal, FileCallJournal, RunnerClient};
use plakous_hosted::error::{CliFailure, CliResult};

use crate::home::paths::PlakPaths;

/// The options every runner dial passes to ssh, ahead of the destination,
/// where they win over the user's ssh config: a known host key only, no
/// agent, credential, X11, port or tunnel forwarding, no environment sent,
/// no local command, no multiplexing, no tty, no prompt, and a 15 s
/// liveness window on a 10 s connect.
pub(crate) const STRICT_SSH_OPTIONS: [&str; 37] = [
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=yes",
    "-o",
    "UpdateHostKeys=no",
    "-o",
    "ForwardAgent=no",
    "-o",
    "GSSAPIDelegateCredentials=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "ForwardX11Trusted=no",
    "-o",
    "Tunnel=no",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "SendEnv=-*",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "RemoteCommand=none",
    "-o",
    "RequestTTY=no",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ConnectTimeout=10",
];

/// This build's version triple: the protocol, the source revision it was
/// built from (`unknown` when the build did not say) and its flavor.
pub(crate) fn version() -> VersionTriple {
    let mut version = VersionTriple::current(
        crate::test_hooks::upgrade_runner_revision()
            .unwrap_or_else(|| crate::build_identity::SOURCE_REVISION.to_owned()),
        crate::build_identity::FLAVOR,
    );
    if let Some(protocol) = crate::test_hooks::upgrade_runner_protocol() {
        version.protocol = protocol;
    }
    version
}

/// The ssh invocation that runs `remote` on `destination`.
fn ssh_command(destination: &str, remote: &[&str]) -> Command {
    let mut command = Command::new("ssh");
    command
        .args(STRICT_SSH_OPTIONS)
        .arg("--")
        .arg(destination)
        .args(remote);
    command
}

pub(super) async fn ssh_status(
    destination: &str,
    remote: &[&str],
    quiet: bool,
) -> std::io::Result<ExitStatus> {
    use std::process::Stdio;
    let mut command = ssh_command(destination, remote);
    command.stdin(Stdio::null());
    if quiet {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    children::status(&mut command).await
}

/// A dialed runner: the client, what the runner said in its hello, and the
/// ssh process carrying the link.
pub(crate) struct Dialed {
    pub(crate) client: RunnerClient,
    pub(crate) hello: Hello,
    _ssh: Option<children::SshChild>,
}

#[cfg(test)]
impl Dialed {
    pub(super) fn in_process(client: RunnerClient, hello: Hello) -> Self {
        Self {
            client,
            hello,
            _ssh: None,
        }
    }
}

/// Dials `destination` over ssh and binds as this host, or, with `inspect`,
/// as an inspecting connection. A bound host records its keyed calls in the
/// runner's journal under the product home.
pub(crate) async fn dial(
    paths: &PlakPaths,
    name: &str,
    destination: &str,
    command: &str,
    inspect: bool,
) -> CliResult<Dialed> {
    dial_placed(paths, name, destination, command, inspect)
        .await
        .map_err(|failure| CliFailure::new(format!("runner {name}: {failure:?}")))
}

pub(crate) async fn dial_placed(
    paths: &PlakPaths,
    name: &str,
    destination: &str,
    command: &str,
    inspect: bool,
) -> Result<Dialed, plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |phase| PlacementFailure::RunnerUnreachable {
        phase,
        timed_out: false,
    };
    let (child, stream) = ssh_stream(destination, command)?;
    let bind = super::bind(
        super::home::host_id(paths).map_err(|_| PlacementFailure::RunnerNotPaired)?,
        inspect,
    );
    let journal: Option<std::sync::Arc<dyn CallJournal>> = if inspect {
        None
    } else {
        Some(std::sync::Arc::new(
            FileCallJournal::open(paths.home().join("runners").join(name).join("calls"))
                .map_err(|_| unavailable(ProbePhase::Reconcile))?,
        ))
    };
    let (client, hello) =
        RunnerClient::connect_journaled(stream, bind, crate::test_hooks::runner_windows(), journal)
            .await
            .map_err(|error| match error {
                plakous::runner::ConnectError::VersionMismatch { runner } => {
                    PlacementFailure::RunnerVersionMismatch {
                        expected: version(),
                        actual: runner,
                    }
                }
                _ => unavailable(ProbePhase::Bind),
            })?;
    Ok(Dialed {
        client,
        hello,
        _ssh: Some(child),
    })
}

/// A paired, exact-triple read-only connection; never binds an owner instance.
pub(crate) async fn qualify(
    paths: &PlakPaths,
    destination: &str,
    command: &str,
) -> CliResult<Dialed> {
    let host_id = super::existing_host_id(paths)?;
    let (child, stream) = ssh_stream(destination, command).map_err(|failure| {
        CliFailure::new(format!(
            "runner qualification transport failed: {failure:?}"
        ))
    })?;
    let (client, hello) = RunnerClient::connect_qualification(stream, host_id, version(), crate::test_hooks::runner_windows()).await
        .map_err(|failure| CliFailure::new(format!("host_upgrade_runner_qualification: {failure:?}; advance the runner dedicated link to the matching snapshot first; retain the old build to inspect and drain old workers")))?;
    Ok(Dialed {
        client,
        hello,
        _ssh: Some(child),
    })
}

fn ssh_stream(
    destination: &str,
    command: &str,
) -> Result<(children::SshChild, children::SshStream), plakous::runner::PlacementFailure> {
    let unavailable = || plakous::runner::PlacementFailure::RunnerUnreachable {
        phase: plakous::runner::ProbePhase::Ssh,
        timed_out: false,
    };
    children::bridge(&mut ssh_command(
        destination,
        &[command, "runner", "connect", "--stdio"],
    ))
    .map_err(|_| unavailable())
}

pub(crate) async fn reconnect_placed(
    client: &RunnerClient,
    destination: &str,
    command: &str,
) -> Result<Dialed, plakous::runner::PlacementFailure> {
    let (child, stream) = ssh_stream(destination, command)?;
    let hello = client
        .reconnect(stream)
        .await
        .map_err(|error| match error {
            plakous::runner::ConnectError::VersionMismatch { runner } => {
                plakous::runner::PlacementFailure::RunnerVersionMismatch {
                    expected: version(),
                    actual: runner,
                }
            }
            _ => plakous::runner::PlacementFailure::RunnerUnreachable {
                phase: plakous::runner::ProbePhase::Bind,
                timed_out: false,
            },
        })?;
    Ok(Dialed {
        client: client.clone(),
        hello,
        _ssh: Some(child),
    })
}
