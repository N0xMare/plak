//! Explicit local startup: curated inputs, detached custody and a bounded log.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{CliFailure, CliResult, ProcessOutcome, product_paths::PlakPaths};

const LOG_LIMIT: usize = 1024 * 1024;

pub(super) fn start(paths: &PlakPaths, deadline: Instant) -> CliResult<()> {
    if crate::environment::host::remote_shell() {
        return Err(CliFailure::new(
            "--auto-start is local only; start the remote host or its user unit explicitly",
        ));
    }
    let inputs = crate::environment::host::start_environment();
    refuse_installed_unit(&inputs, crate::environment::host::unit_path().as_deref())?;
    paths.ensure_layout().map_err(CliFailure::new)?;
    crate::commands::runner::private_directory(&paths.home().join("host"))?;
    drop(open_log(paths)?);
    let mut command = executable(paths)?;
    command
        .args(["host", "__start-auto"])
        .env_clear()
        .envs(inputs)
        .env("PLAK_HOME", paths.home())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut helper = command.spawn().map_err(failed)?;
    loop {
        if let Some(status) = helper.try_wait().map_err(failed)? {
            return if status.success() {
                Ok(())
            } else {
                Err(CliFailure::new("the detached host starter failed"))
            };
        }
        if Instant::now() >= deadline {
            let _ = helper.kill();
            let _ = helper.wait();
            return Err(CliFailure::new("the detached host starter timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// The first helper already has a separate process group, protecting it from
// caller signals. Its child is not a group leader, so setsid needs neither
// unsafe pre_exec nor a fork in this multithreaded product process.
pub(super) fn helper(paths: &PlakPaths) -> CliResult<ProcessOutcome> {
    let mut supervisor = executable(paths)?
        .args(["host", "__supervise-auto"])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(failed)?;
    // Do not let discovery use another winner before this starter has either
    // claimed custody or lost. Otherwise a delayed loser can start after the
    // winning host has already been stopped.
    let mut acknowledgement = [0_u8; 1];
    supervisor
        .stdout
        .take()
        .ok_or_else(|| CliFailure::new("missing starter acknowledgement"))?
        .read_exact(&mut acknowledgement)
        .map_err(failed)?;
    Ok(ProcessOutcome::Success)
}

pub(super) fn supervise(paths: &PlakPaths) -> CliResult<ProcessOutcome> {
    rustix::process::setsid().map_err(|error| failed(error.into()))?;
    let mut log = open_log(paths)?;
    match log.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            acknowledge();
            return Ok(ProcessOutcome::Success);
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(failed(error)),
    }
    cap_log(&mut log, &[]).map_err(failed)?;
    let mut child = executable(paths)?
        .args(["host", "__serve-auto"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(failed)?;
    let claimed = await_claim(paths, &mut child);
    acknowledge();
    let captured = if let Some(mut stderr) = child.stderr.take() {
        collect_log(&mut child, &mut stderr, &mut log)
    } else {
        Ok(None)
    };
    // A log failure must not abandon the live host's reaping owner.
    let waited = child.wait().map_err(failed)?;
    claimed?;
    let status = captured.map_err(failed)?.unwrap_or(waited);
    if status.success() {
        Ok(ProcessOutcome::Success)
    } else {
        Err(CliFailure::new("the detached host exited unsuccessfully"))
    }
}

fn acknowledge() {
    // The launching helper may already have reached its bounded deadline.
    // Its disappearance must not kill a host that already owns custody.
    let mut output = io::stdout().lock();
    let _ = output.write_all(&[1]).and_then(|()| output.flush());
}

fn await_claim(paths: &PlakPaths, child: &mut std::process::Child) -> CliResult<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().map_err(failed)?.is_some() {
            return Ok(());
        }
        if crate::wire_transport::hosts::lock_holder(&crate::wire_transport::hosts::home_lock(
            paths,
        ))?
        .and_then(|holder| {
            holder
                .split_whitespace()
                .next()
                .and_then(|pid| pid.parse::<u32>().ok())
        }) == Some(child.id())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(CliFailure::new(
                "the detached host did not settle its startup claim",
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn collect_log(
    child: &mut std::process::Child,
    stderr: &mut (impl io::Read + std::os::fd::AsFd),
    log: &mut File,
) -> io::Result<Option<std::process::ExitStatus>> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(&*stderr)?;
    fcntl_setfl(&*stderr, flags | OFlags::NONBLOCK)?;
    let mut bytes = [0_u8; 8192];
    let mut trailing = LOG_LIMIT;
    loop {
        let exited = child.try_wait()?;
        match stderr.read(&mut bytes) {
            Ok(0) => return Ok(exited),
            Ok(count) => {
                // Keep draining on log-write failure so stderr backpressure
                // cannot deadlock an otherwise healthy host.
                let _ = cap_log(log, &bytes[..count]);
                if exited.is_some() {
                    trailing = trailing.saturating_sub(count);
                    if trailing == 0 {
                        return Ok(exited);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Descendants may retain stderr after the host exits. EOF is
                // not its lifecycle result; only the direct child is reaped.
                if exited.is_some() {
                    return Ok(exited);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn executable(paths: &PlakPaths) -> CliResult<Command> {
    let executable = std::env::current_exe().map_err(failed)?;
    let mut command = Command::new(executable);
    command.current_dir(paths.home());
    Ok(command)
}

fn open_log(paths: &PlakPaths) -> CliResult<File> {
    use rustix::fs::{Mode, OFlags};
    let file = File::from(
        rustix::fs::open(
            paths.home().join("host/auto-start.log"),
            OFlags::CREATE
                | OFlags::RDWR
                | OFlags::APPEND
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| failed(error.into()))?,
    );
    let metadata = file.metadata().map_err(failed)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(CliFailure::new(
            "auto-start log is not this user's private regular file",
        ));
    }
    Ok(file)
}

fn cap_log(log: &mut File, bytes: &[u8]) -> io::Result<()> {
    let bytes = &bytes[bytes.len().saturating_sub(LOG_LIMIT)..];
    if log.metadata()?.len().saturating_add(bytes.len() as u64) > LOG_LIMIT as u64 {
        log.set_len(0)?;
    }
    log.write_all(bytes)
}

fn refuse_installed_unit(
    inputs: &[(&str, OsString)],
    unit_path: Option<&std::ffi::OsStr>,
) -> CliResult<()> {
    let value = |name| {
        inputs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| PathBuf::from(value))
    };
    let config =
        value("XDG_CONFIG_HOME").or_else(|| value("HOME").map(|home| home.join(".config")));
    let data =
        value("XDG_DATA_HOME").or_else(|| value("HOME").map(|home| home.join(".local/share")));
    let mut directories = vec![
        PathBuf::from("/etc/systemd/user"),
        PathBuf::from("/run/systemd/user"),
        PathBuf::from("/usr/local/lib/systemd/user"),
        PathBuf::from("/usr/lib/systemd/user"),
        PathBuf::from("/lib/systemd/user"),
        PathBuf::from("/usr/local/share/systemd/user"),
        PathBuf::from("/usr/share/systemd/user"),
    ];
    directories.extend(config.map(|root| root.join("systemd/user")));
    directories.extend(data.map(|root| root.join("systemd/user")));
    directories.extend(value("XDG_RUNTIME_DIR").map(|root| root.join("systemd/user")));
    for (name, fallback) in [
        ("XDG_CONFIG_DIRS", "/etc/xdg"),
        ("XDG_DATA_DIRS", "/usr/local/share:/usr/share"),
    ] {
        let list = inputs
            .iter()
            .find(|(key, _)| *key == name)
            .map_or_else(|| OsString::from(fallback), |(_, value)| value.clone());
        directories.extend(std::env::split_paths(&list).map(|root| root.join("systemd/user")));
    }
    if let Some(paths) = unit_path {
        directories
            .extend(std::env::split_paths(paths).filter(|path| !path.as_os_str().is_empty()));
    }
    for directory in directories {
        if !directory.is_absolute() {
            return Err(CliFailure::new(
                "host unit lookup needs absolute OS configuration paths; absence is unproven",
            ));
        }
        let path = directory.join("plak-host.service");
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(CliFailure::new(
                    "plak-host.service is installed; use systemctl --user start plak-host.service; no unmanaged host was started",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(failed(error)),
        }
    }
    Ok(())
}

fn failed(error: io::Error) -> CliFailure {
    CliFailure::new(format!("host auto-start failed: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "private bounded-log fixtures")]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn an_exited_child_is_reaped_without_waiting_for_stderr_eof() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        paths.ensure_layout().unwrap();
        crate::commands::runner::private_directory(&paths.home().join("host")).unwrap();
        let mut log = open_log(&paths).unwrap();
        let (mut reader, mut held_writer) = std::os::unix::net::UnixStream::pair().unwrap();
        held_writer
            .write_all(b"trailing host diagnostic\n")
            .unwrap();
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let status = collect_log(&mut child, &mut reader, &mut log)
            .unwrap()
            .unwrap();
        assert!(status.success());
        assert!(child.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(paths.home().join("host/auto-start.log")).unwrap(),
            "trailing host diagnostic\n"
        );
        // The writer was never closed to make the collector return.
        held_writer
            .write_all(b"descendant still holds stderr")
            .unwrap();
    }

    #[test]
    fn auto_start_log_stays_private_and_bounded_across_repeated_large_writes() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        paths.ensure_layout().unwrap();
        crate::commands::runner::private_directory(&paths.home().join("host")).unwrap();
        let mut log = open_log(&paths).unwrap();
        for _ in 0..4 {
            cap_log(&mut log, &vec![b'x'; LOG_LIMIT + 17]).unwrap();
            assert!(log.metadata().unwrap().len() <= LOG_LIMIT as u64);
        }
        assert_eq!(log.metadata().unwrap().mode() & 0o777, 0o600);
        drop(log);
        let path = paths.home().join("host/auto-start.log");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_log(&paths).is_err());
        std::fs::remove_file(&path).unwrap();
        let target = root.path().join("not-a-log");
        std::fs::write(&target, "untouched").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(open_log(&paths).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(open_log(&paths).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
    }
}
