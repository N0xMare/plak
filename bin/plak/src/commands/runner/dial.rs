//! The host's side of a runner link (spec 04): the host dials its runner
//! over ssh with fixed strict options and runs `plak runner connect --stdio`
//! there. It never listens: there is no dial-in.

use std::process::Stdio;

use plakous::runner::protocol::{Hello, VersionTriple};
use plakous::runner::{CallJournal, ClientWindows, FileCallJournal, RunnerClient};
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

/// The command a host runs on its runner through ssh.
pub(crate) const REMOTE_CONNECT: [&str; 4] = ["plak", "runner", "connect", "--stdio"];

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
    inspect: bool,
) -> CliResult<Dialed> {
    let mut ssh = ssh_command(destination, &REMOTE_CONNECT);
    ssh.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = ssh
        .spawn()
        .map_err(|error| CliFailure::new(format!("ssh could not be started: {error}")))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(CliFailure::new("ssh did not give a link"));
    };
    let stream = tokio::io::join(stdout, stdin);
    let bind = super::bind(super::home::host_id(paths)?, inspect);
    let journal: Option<std::sync::Arc<dyn CallJournal>> = if inspect {
        None
    } else {
        Some(std::sync::Arc::new(
            FileCallJournal::open(paths.home().join("runners").join(name).join("calls")).map_err(
                |error| CliFailure::new(format!("the runner call journal failed: {error}")),
            )?,
        ))
    };
    let (client, hello) =
        RunnerClient::connect_journaled(stream, bind, ClientWindows::default(), journal)
            .await
            .map_err(|error| CliFailure::new(format!("runner {name}: {error}")))?;
    Ok(Dialed {
        client,
        hello,
        _ssh: child,
    })
}
