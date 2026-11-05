//! `plak runner`: this machine's runner daemon and the host's view of its
//! runners (spec 02 §1, spec 01 §8.4).
//!
//! On a runner machine, `serve` runs the daemon in the foreground and
//! `connect --stdio` (what a host's ssh runs) bridges to it, starting it
//! detached when none answers. `allow` pairs a host by adding its identity
//! to the allowlist, and `ps` lists the lines and jobs the daemon holds. On a
//! host, `pair <name>` pairs this host with `[runners.<name>]` over ssh and
//! `ps <name>` lists that runner's lines.

mod dial;
mod home;

pub(crate) use home::{host_id, private_directory};

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use plakous::runner::daemon;
use plakous::runner::protocol::{Bind, LineStatus, Refusal};
use plakous::runner::{
    ClientWindows, FileOwnerFloors, RunnerClient, RunnerIdentity, RunnerLimits, RunnerOptions,
    RunnerService,
};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

use crate::home::paths::{PlakPaths, SecretShelf};
use dial::ssh_command;
pub(crate) use dial::{STRICT_SSH_OPTIONS, version};
use home::RunnerHome;

/// This process's host instance, the same for its life: a runner holds the
/// lines whose transfers it accepted from it, and every connection this
/// process makes to a runner binds with it (spec 01 §8.4). The `host_id`
/// names the home and is shared by all its processes; this names one of
/// them.
pub(crate) fn host_instance() -> String {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE
        .get_or_init(|| uuid::Uuid::new_v4().hyphenated().to_string())
        .clone()
}

/// How long `connect` waits for a daemon it started to listen.
const START_WAIT: Duration = Duration::from_secs(10);

/// A `plak runner` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunnerCommand {
    Serve,
    Connect,
    Allow { host_id: String },
    Ps { name: Option<String> },
    Pair { name: String },
}

/// Runs one `plak runner` command.
pub(crate) fn run(
    paths: &PlakPaths,
    command: RunnerCommand,
    stdout: &mut dyn std::io::Write,
) -> CliResult<ProcessOutcome> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("plak-runner")
        .build()
        .map_err(|error| CliFailure::new(format!("the runner runtime did not start: {error}")))?;
    match command {
        RunnerCommand::Serve => runtime.block_on(serve(paths)),
        RunnerCommand::Connect => {
            let bridged = runtime.block_on(connect(paths));
            // The bridge's stdin copy may still sit in a blocking read of
            // the host's input; waiting for it would keep stdout open after
            // the daemon closed, so the host would not see the link end.
            runtime.shutdown_background();
            return bridged.map(|()| ProcessOutcome::Success);
        }
        RunnerCommand::Allow { host_id } => allow(paths, &host_id, stdout),
        RunnerCommand::Ps { name: None } => runtime.block_on(ps_local(paths, stdout)),
        RunnerCommand::Ps { name: Some(name) } => runtime.block_on(ps_remote(paths, &name, stdout)),
        RunnerCommand::Pair { name } => runtime.block_on(pair(paths, &name, stdout)),
    }?;
    Ok(ProcessOutcome::Success)
}

/// `plak runner serve`: the daemon, in the foreground, until it is idle.
async fn serve(paths: &PlakPaths) -> CliResult<()> {
    let home = RunnerHome::of(paths);
    // Before anything is written for the daemon: a path the platform cannot
    // bind leaves nothing behind.
    let socket = checked_socket(&home)?;
    let config = home.config()?;
    let Some(first) = config.roots.first() else {
        return Err(CliFailure::new(format!(
            "{} lists no roots; a runner serves only workspaces under its roots",
            home.config_path().display()
        )));
    };
    private_directory(home.root())?;
    let excluded = paths.home().to_path_buf();
    refuse_unsafe_roots(&home, &config, &excluded)?;
    if !home::admitted_workspace(&config.roots, &excluded, first) {
        return Err(CliFailure::new(format!(
            "runner root {} cannot be served: it does not resolve to a directory under the runner's roots",
            first.display()
        )));
    }
    let scope = ExecutionEnvironmentScope::Run(
        plakous::model::RunId::try_from(uuid::Uuid::new_v4())
            .map_err(|error| CliFailure::new(error.to_string()))?,
    );
    let environment =
        ExecutionEnvironment::host(scope, first, first, ExecutionEnvironmentConfig::default())
            .map_err(|error| {
                CliFailure::new(format!("runner root {}: {error}", first.display()))
            })?;
    let (roots, outside) = (config.roots.clone(), excluded.clone());
    let workspaces = Arc::new(move |root: &Path, cwd: &Path| {
        if !home::admitted_workspace(&roots, &outside, root) {
            return Err(plakous::tools::runner::workspace_refusal());
        }
        ExecutionEnvironment::host(scope, root, cwd, ExecutionEnvironmentConfig::default())
    });
    let roots = config.roots.clone();
    let probe_excluded = excluded.clone();
    let probe_paths = Arc::new(move |candidate: &Path| {
        home::admitted_probe_path(&roots, &probe_excluded, candidate)
    });
    let extension = vcs_extension(&config, excluded);
    let allowlist_home = home.clone();
    let hosts = Arc::new(move |host_id: &str| {
        // Read at every bind, so a pairing made while the daemon runs holds.
        allowlist_home
            .config()
            .is_ok_and(|config| config.hosts.iter().any(|allowed| allowed == host_id))
    });
    let floors = FileOwnerFloors::open(home.floors_path())
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let runner_id = RunnerId::new(home.runner_id()?)
        .map_err(|error| CliFailure::new(format!("runner id: {error}")))?;
    let service = RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id,
            version: version(),
            boot_id: boot_id(),
            binary_id: crate::build_identity::known_binary_id(),
        },
        RunnerOptions {
            limits: RunnerLimits::default(),
            workspaces: Some(workspaces),
            extension: Some(extension),
            hosts: Some(hosts),
            probe_paths: Some(probe_paths),
            local_host: Some(host_id(paths)?),
            secrets: Some(Arc::new(runner_secret(paths.clone()))),
            floors: Some(Arc::new(floors)),
            path: crate::environment::inherited_path(),
            home: crate::environment::inherited_home(),
        },
    )
    .map_err(|error| CliFailure::new(format!("the runner did not start: {error}")))?;
    match daemon::serve_socket(service, &socket, config.idle_exit()).await {
        Ok(()) => Ok(()),
        // One daemon per user and build is enough.
        Err(daemon::DaemonError::AlreadyServing(_)) => Ok(()),
        Err(error) => Err(CliFailure::new(error.to_string())),
    }
}

/// Refuses a root or copies root that is relative, or lies inside or
/// around the product home: it would hand a host this machine's own files,
/// the runner's allowlist and floors, and every secret and credential the
/// home holds.
fn refuse_unsafe_roots(
    home: &RunnerHome,
    config: &home::RunnerToml,
    excluded: &Path,
) -> CliResult<()> {
    for root in config.roots.iter().chain(&config.copies) {
        if !root.is_absolute() {
            return Err(CliFailure::new(format!(
                "{}: {} is not an absolute path",
                home.config_path().display(),
                root.display()
            )));
        }
        if home::overlaps_home(root, excluded) {
            return Err(CliFailure::new(format!(
                "{}: root {} lies inside or around the product home {}; list workspace roots apart from it",
                home.config_path().display(),
                root.display(),
                excluded.display()
            )));
        }
    }
    // Mirrors kept inside a workspace would sit in its tree, and a
    // workspace inside the copies root would be served as a copy.
    if let Some(copies) = &config.copies
        && let Some(root) = config
            .roots
            .iter()
            .find(|root| home::overlaps_home(copies, root))
    {
        return Err(CliFailure::new(format!(
            "{}: copies {} lies inside or around the root {}; keep copies apart from the roots",
            home.config_path().display(),
            copies.display(),
            root.display()
        )));
    }
    Ok(())
}

/// The tree's version control, answering under the roots and the copies
/// root and never inside the product home, and taking copies carried from
/// other runners under the copies root.
fn vcs_extension(
    config: &home::RunnerToml,
    excluded: std::path::PathBuf,
) -> Arc<dyn plakous::tools::runner::RunnerExtension> {
    let roots: Vec<_> = config.roots.iter().chain(&config.copies).cloned().collect();
    plakous_recursive_subagents::remote_vcs::vcs_runner_extension(
        Arc::new(move |candidate: &Path| home::admitted_probe_path(&roots, &excluded, candidate)),
        config.copies.clone(),
    )
}

/// This build's daemon socket in `home`, refused by name when the platform
/// cannot bind or connect to it.
fn checked_socket(home: &RunnerHome) -> CliResult<std::path::PathBuf> {
    let socket = daemon::socket_path(&home.run_directory(), &version());
    daemon::check_socket_path(&socket).map_err(|error| CliFailure::new(error.to_string()))?;
    Ok(socket)
}

/// How the daemon reads a runner secret a service names: from this home's
/// runner shelf (`plak runner secret set`), never its named secrets.
fn runner_secret(
    paths: PlakPaths,
) -> impl Fn(&str) -> Result<Option<String>, String> + Send + Sync + 'static {
    move |name| {
        crate::home::secrets::read_shelf_secret(&paths, SecretShelf::Runner, name)
            .map(|secret| secret.map(|secret| secret.expose_secret().to_owned()))
    }
}

/// `plak runner connect --stdio`: bridges stdin and stdout to the daemon,
/// starting it detached when none answers.
async fn connect(paths: &PlakPaths) -> CliResult<()> {
    let home = RunnerHome::of(paths);
    let socket = checked_socket(&home)?;
    let config = home.config()?;
    let start = || start_daemon(paths, &home, &config);
    daemon::bridge_stdio(&socket, start, START_WAIT)
        .await
        .map_err(|error| CliFailure::new(format!("the runner link failed: {error}")))
}

/// Starts `plak runner serve` detached, in its own process group, with a
/// cleared environment: `PATH` and `HOME` from `runner.toml` (a login shell
/// answers a missing `PATH`), and this product home.
fn start_daemon(
    paths: &PlakPaths,
    home: &RunnerHome,
    config: &home::RunnerToml,
) -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let user_home = config
        .home
        .clone()
        .or_else(crate::environment::inherited_home)
        .unwrap_or_default();
    let path = match &config.path {
        Some(path) => path.clone(),
        None => login_shell_path()
            .or_else(crate::environment::inherited_path)
            .unwrap_or_else(|| "/usr/bin:/bin".to_owned()),
    };
    private_directory(home.root())
        .map_err(|error| std::io::Error::other(error.message().to_owned()))?;
    let log = {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(home.log_path())?;
        // An older log keeps the mode it was created with.
        log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        log
    };
    let mut command = std::process::Command::new(executable);
    command
        .args(["runner", "serve"])
        .env_clear()
        .env("PATH", path)
        .env("HOME", user_home)
        .env("PLAK_HOME", paths.home())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().map(drop)
}

/// The `PATH` the user's login shell sets, asked once with a 5 s bound.
pub(crate) fn login_shell_path() -> Option<String> {
    login_shell_answer(&crate::environment::login_shell()?, LOGIN_SHELL_DEADLINE)
}

/// How long a login shell may take to say its `PATH`.
const LOGIN_SHELL_DEADLINE: Duration = Duration::from_secs(5);

/// What `shell -l -c 'printf %s "$PATH"'` prints, when it exits successfully
/// within `deadline` and prints one line. The shell runs in a process group
/// of its own, which is killed once it has answered or the deadline passed:
/// a startup file that leaves a job in the background (`sleep 20 &`) holds
/// the pipe open, so neither its end of output nor its exit is waited for
/// past the deadline.
fn login_shell_answer(shell: &str, deadline: Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + deadline;
    let mut command = std::process::Command::new(shell);
    command
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command.spawn().ok()?;
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    let mut stdout = child.stdout.take()?;
    let (sender, output) = std::sync::mpsc::sync_channel(1);
    // The read ends at end of output, which the group's kill below brings
    // if a background job still holds the pipe.
    std::thread::spawn(move || {
        let mut text = String::new();
        let read = std::io::Read::read_to_string(&mut stdout, &mut text);
        let _ = sender.send(read.map(|_| text));
    });
    // Waited on without reaping, so the group keeps its identity until it
    // is killed.
    let exited = loop {
        let Some(pid) = group else {
            break None;
        };
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOWAIT
                | rustix::process::WaitIdOptions::NOHANG,
        ) {
            Ok(Some(status)) => break Some(status.exit_status() == Some(0)),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => break None,
        }
    };
    if let Some(group) = group {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.wait();
    if exited != Some(true) {
        return None;
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let path = output
        .recv_timeout(remaining + Duration::from_millis(100))
        .ok()?
        .ok()?;
    (!path.is_empty() && !path.contains('\n')).then_some(path)
}

/// `plak runner allow <host-id>`: pairs a host with this machine's runner.
fn allow(paths: &PlakPaths, host_id: &str, stdout: &mut dyn std::io::Write) -> CliResult<()> {
    let home = RunnerHome::of(paths);
    let added = home.allow(host_id)?;
    let line = if added {
        format!("host {host_id} may use this runner")
    } else {
        format!("host {host_id} was already paired")
    };
    writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))
}

/// `plak runner ps`: the lines and jobs this machine's daemon holds, for
/// every host.
async fn ps_local(paths: &PlakPaths, stdout: &mut dyn std::io::Write) -> CliResult<()> {
    let home = RunnerHome::of(paths);
    let socket = checked_socket(&home)?;
    let Ok(stream) = tokio::net::UnixStream::connect(&socket).await else {
        writeln!(stdout, "no runner is running").map_err(|_| CliFailure::new("stdout failed"))?;
        return Ok(());
    };
    let bind = Bind {
        host_id: host_id(paths)?,
        host_instance: host_instance(),
        version: version(),
        inspect: true,
    };
    let (client, _) = RunnerClient::connect(stream, bind, ClientWindows::default())
        .await
        .map_err(|error| CliFailure::new(format!("the runner: {error}")))?;
    let lines = client
        .lines()
        .await
        .map_err(|error| CliFailure::new(format!("the runner: {error:?}")))?;
    render_lines(&lines, stdout)
}

/// `plak runner ps <name>`: what a configured runner holds, over ssh.
async fn ps_remote(
    paths: &PlakPaths,
    name: &str,
    stdout: &mut dyn std::io::Write,
) -> CliResult<()> {
    let destination = configured(paths, name)?;
    let dialed = dial::dial(paths, name, &destination, true).await?;
    let lines = dialed
        .client
        .lines()
        .await
        .map_err(|error| CliFailure::new(format!("runner {name}: {error:?}")))?;
    render_lines(&lines, stdout)
}

/// `plak runner pair <name>`: adds this host to `[runners.<name>]`'s
/// allowlist over ssh, then binds to it to prove the pairing, and records
/// the runner's identity.
async fn pair(paths: &PlakPaths, name: &str, stdout: &mut dyn std::io::Write) -> CliResult<()> {
    let ours = version();
    if ours.source_revision == "unknown" {
        return Err(CliFailure::new(
            "this build does not know its source revision, so it cannot pair with a remote runner; install a release or maintainer build",
        ));
    }
    let destination = configured(paths, name)?;
    let host_id = host_id(paths)?;
    let status = ssh_command(&destination, &["plak", "runner", "allow", &host_id])
        .stdin(std::process::Stdio::null())
        .status()
        .await
        .map_err(|error| CliFailure::new(format!("ssh could not be started: {error}")))?;
    if !status.success() {
        return Err(CliFailure::new(format!(
            "runner {name}: `plak runner allow` did not succeed over ssh ({status})"
        )));
    }
    let dialed = dial::dial(paths, name, &destination, false).await?;
    // The bind is proved by an answer the runner gives only a paired host.
    if let Err(error) = dialed.client.lines().await {
        let reason = match dialed.client.refusal() {
            Some(Refusal::NotPaired { .. }) => "the runner does not list this host".to_owned(),
            Some(refusal) => format!("{refusal:?}"),
            None => format!("{error:?}"),
        };
        return Err(CliFailure::new(format!("runner {name}: {reason}")));
    }
    let record = PairedRunner {
        runner_id: &dialed.hello.runner_id,
        source_revision: &dialed.hello.version.source_revision,
        flavor: &dialed.hello.version.flavor,
        binary_id: dialed.hello.binary_id.as_deref(),
    };
    let directory = paths.home().join("runners").join(name);
    private_directory(&directory)?;
    home::write_private(
        &directory.join("runner.json"),
        &serde_json::to_vec_pretty(&record).map_err(|error| CliFailure::new(error.to_string()))?,
    )?;
    writeln!(
        stdout,
        "paired host {host_id} with runner {name} ({})",
        dialed.hello.runner_id
    )
    .map_err(|_| CliFailure::new("stdout failed"))
}

/// What a host records of a runner it paired with.
#[derive(serde::Serialize)]
struct PairedRunner<'a> {
    runner_id: &'a str,
    source_revision: &'a str,
    flavor: &'a str,
    binary_id: Option<&'a str>,
}

fn configured(paths: &PlakPaths, name: &str) -> CliResult<String> {
    let config = crate::product_config::ProductConfig::load(paths).map_err(CliFailure::new)?;
    config
        .runners
        .get(name)
        .map(|runner| runner.ssh.clone())
        .ok_or_else(|| {
            CliFailure::new(format!(
                "no [runners.{name}] in config.toml; add it with an ssh destination"
            ))
        })
}

fn render_lines(lines: &[LineStatus], stdout: &mut dyn std::io::Write) -> CliResult<()> {
    let mut text = String::new();
    if lines.is_empty() {
        text.push_str("no lines\n");
    }
    for line in lines {
        let jobs = line.jobs["jobs"].as_array().map_or(0, Vec::len);
        let held = if line.adopted { "held" } else { "orphaned" };
        text.push_str(&format!(
            "host {} line {} {held} {} job{}\n",
            line.host_id,
            line.line,
            jobs,
            if jobs == 1 { "" } else { "s" }
        ));
    }
    stdout
        .write_all(text.as_bytes())
        .map_err(|_| CliFailure::new("stdout failed"))
}

/// The machine boot this daemon belongs to: Linux's boot id, or the boot
/// time elsewhere.
fn boot_id() -> String {
    if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return id.trim().to_owned();
    }
    std::process::Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
