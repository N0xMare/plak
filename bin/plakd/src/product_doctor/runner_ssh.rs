//! Doctor's `ssh -G` check of each `[runners.<name>]`: how ssh resolves the
//! destination under the strict options a dial passes, without connecting.
//! A bounded subprocess in its own group with a cleared environment, like
//! the jj probe.

use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{CheckKind, DoctorCheck, Severity};

/// How long `ssh -G` may take.
const BUDGET: Duration = Duration::from_secs(5);

/// The bytes of `ssh -G` output read; a resolved config is a few KiB.
const MAX_OUTPUT: usize = 64 * 1024;

/// One row per `(name, destination)`, from `ssh -G <options> -- <destination>`
/// on `path`.
pub fn probe(
    runners: &[(String, String)],
    options: &[&str],
    path: Option<&std::ffi::OsStr>,
) -> Vec<DoctorCheck> {
    runners
        .iter()
        .map(|(name, destination)| {
            let resolved = resolve(options, destination, path.unwrap_or_default());
            check(name, resolved)
        })
        .collect()
}

/// The row for runner `name` from what `ssh -G` printed, or why it printed
/// nothing usable.
pub(super) fn check(name: &str, resolved: Result<String, String>) -> DoctorCheck {
    let remedy = Some("check [runners.<name>] ssh in config.toml and your ssh config");
    let output = match resolved {
        Ok(output) => output,
        Err(reason) => {
            return DoctorCheck::new(
                CheckKind::RunnerSsh,
                "runners",
                Severity::Warning,
                "unavailable",
                format!("runner {name}: ssh -G {reason}"),
                remedy,
            );
        }
    };
    let field = |key: &str| {
        output.lines().find_map(|line| {
            let (found, value) = line.split_once(' ')?;
            (found == key).then(|| value.trim().to_owned())
        })
    };
    let (Some(user), Some(host), Some(port)) = (field("user"), field("hostname"), field("port"))
    else {
        return DoctorCheck::new(
            CheckKind::RunnerSsh,
            "runners",
            Severity::Warning,
            "unavailable",
            format!("runner {name}: ssh -G printed no user, hostname and port"),
            remedy,
        );
    };
    let detail = format!("runner {name}: {user}@{host}:{port}");
    // Command-line options win over ssh config, so these hold whatever the
    // config says; a mismatch is an ssh that ignored them.
    let strict = [
        ("batchmode", "yes"),
        ("stricthostkeychecking", "true"),
        ("forwardagent", "no"),
    ]
    .into_iter()
    .find(|(key, expected)| field(key).as_deref() != Some(*expected));
    if let Some((key, _)) = strict {
        return DoctorCheck::new(
            CheckKind::RunnerSsh,
            "runners",
            Severity::Warning,
            "unsafe",
            format!("{detail}; ssh did not keep the strict option {key}"),
            Some("use an OpenSSH client that honors -o on the command line"),
        );
    }
    DoctorCheck::new(
        CheckKind::RunnerSsh,
        "runners",
        Severity::Ok,
        "resolved",
        detail,
        None,
    )
}

/// What `ssh -G` prints for `destination`, within [`BUDGET`].
fn resolve(options: &[&str], destination: &str, path: &std::ffi::OsStr) -> Result<String, String> {
    let deadline = Instant::now() + BUDGET;
    let mut child = Command::new("ssh")
        .arg("-G")
        .args(options)
        .arg("--")
        .arg(destination)
        .env_clear()
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("could not start: {error}"))?;
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    let read = |stream: Option<Box<dyn std::io::Read + Send>>| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(stream) = stream {
                let _ = stream
                    .take(u64::try_from(MAX_OUTPUT).unwrap_or(u64::MAX))
                    .read_to_end(&mut bytes);
            }
            let _ = sender.send(bytes);
        });
        receiver
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|stream| Box::new(stream) as Box<dyn std::io::Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|stream| Box::new(stream) as Box<dyn std::io::Read + Send>),
    );
    // Waited on without reaping, so the group keeps its identity until every
    // descendant is killed.
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
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break None,
        }
    };
    if let Some(group) = group {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    // A child that moved itself out of its group escapes the group's kill;
    // killing it by pid keeps the reap bounded.
    let _ = child.kill();
    let status = child.wait();
    let remaining =
        || deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100);
    let Some(success) = exited else {
        return Err("timed out (5 s)".to_owned());
    };
    let output = stdout.recv_timeout(remaining()).unwrap_or_default();
    if !success {
        let status = status.map_or_else(|error| error.to_string(), |status| status.to_string());
        let error = stderr.recv_timeout(remaining()).unwrap_or_default();
        let first = String::from_utf8_lossy(&error)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        return Err(format!("failed ({status}): {first}"));
    }
    String::from_utf8(output).map_err(|_| "printed text that is not UTF-8".to_owned())
}
