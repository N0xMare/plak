//! The host's side of a runner link (spec 04): the host dials its runner
//! over ssh with fixed strict options and runs `plak runner connect --stdio`
//! there. It never listens: there is no dial-in.

use std::process::Stdio;

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
    VersionTriple::current(
        crate::build_identity::SOURCE_REVISION,
        crate::build_identity::FLAVOR,
    )
}

/// The ssh invocation that runs `remote` on `destination`.
pub(crate) fn ssh_command(destination: &str, remote: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("ssh");
    command
        .args(STRICT_SSH_OPTIONS)
        .arg("--")
        .arg(destination)
        .args(remote)
        .kill_on_drop(true);
    command
}

/// A dialed runner: the client, what the runner said in its hello, and the
/// ssh process carrying the link.
pub(crate) struct Dialed {
    pub(crate) client: RunnerClient,
    pub(crate) hello: Hello,
    pub(crate) _ssh: tokio::process::Child,
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
        _ssh: child,
    })
}

type SshStream = tokio::io::Join<tokio::process::ChildStdout, tokio::process::ChildStdin>;

/// A paired, exact-triple read-only connection; never binds an owner instance.
pub(crate) async fn qualify(
    paths: &PlakPaths,
    destination: &str,
    command: &str,
) -> CliResult<Dialed> {
    let host_id = std::fs::read_to_string(paths.home().join("host-id")).map_err(|_| {
        CliFailure::new("runner qualification requires this home existing paired identity")
    })?;
    let host_id = host_id.trim();
    uuid::Uuid::parse_str(host_id)
        .map_err(|_| CliFailure::new("runner qualification home identity is invalid"))?;
    let (child, stream) = ssh_stream(destination, command).map_err(|failure| {
        CliFailure::new(format!(
            "runner qualification transport failed: {failure:?}"
        ))
    })?;
    let (client, hello) = RunnerClient::connect_qualification(stream, host_id.to_owned(), version(), crate::test_hooks::runner_windows()).await
        .map_err(|failure| CliFailure::new(format!("host_upgrade_runner_qualification: {failure:?}; advance the runner dedicated link to the matching snapshot first; retain the old build to inspect and drain old workers")))?;
    Ok(Dialed {
        client,
        hello,
        _ssh: child,
    })
}

/// A dormant seed reads its fresh line's jobs under destination host custody.
/// Bind normally after source exit, without loading or replaying an old journal.
#[cfg(feature = "web")]
pub(crate) async fn passive_import(
    paths: &PlakPaths,
    destination: &str,
    command: &str,
) -> CliResult<Dialed> {
    let (child, stream) = ssh_stream(destination, command).map_err(|failure| {
        CliFailure::new(format!(
            "passive import runner transport failed: {failure:?}"
        ))
    })?;
    let bind = super::bind(super::host_id(paths)?, false);
    let (client, hello) = RunnerClient::connect(stream, bind, crate::test_hooks::runner_windows())
        .await
        .map_err(|failure| {
            CliFailure::new(format!("passive import runner bind failed: {failure:?}"))
        })?;
    Ok(Dialed {
        client,
        hello,
        _ssh: child,
    })
}

fn ssh_stream(
    destination: &str,
    command: &str,
) -> Result<(tokio::process::Child, SshStream), plakous::runner::PlacementFailure> {
    let unavailable = || plakous::runner::PlacementFailure::RunnerUnreachable {
        phase: plakous::runner::ProbePhase::Ssh,
        timed_out: false,
    };
    let mut ssh = ssh_command(destination, &[command, "runner", "connect", "--stdio"]);
    let mut child = ssh
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| unavailable())?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(unavailable());
    };
    Ok((child, tokio::io::join(stdout, stdin)))
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
        _ssh: child,
    })
}
