//! Bounded execution of the exact destination image, never a shell or PATH lookup.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::{CliFailure, CliResult};

/// Filesystem identity of the bytes hashed during qualification.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct VerifiedImage {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    mode: u32,
}

impl VerifiedImage {
    fn of(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
            mode: metadata.mode(),
        }
    }

    pub(crate) fn recheck(&self, path: &Path) -> CliResult<()> {
        if path
            .metadata()
            .is_ok_and(|metadata| Self::of(&metadata) == *self)
        {
            Ok(())
        } else {
            Err(CliFailure::new(
                "host_upgrade_binary_mismatch: qualified image changed",
            ))
        }
    }
}

pub(crate) fn verify_image(path: &Path, expected: &str) -> CliResult<VerifiedImage> {
    use std::os::unix::fs::MetadataExt as _;
    let mut image = File::open(path).map_err(|error| CliFailure::new(error.to_string()))?;
    let metadata = image
        .metadata()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if !metadata.is_file() || metadata.mode() & 0o022 != 0 || metadata.mode() & 0o111 == 0 {
        return Err(CliFailure::new(
            "host_upgrade_image_refused: destination must be a non-shared-writable executable regular file",
        ));
    }
    let verified = VerifiedImage::of(&metadata);
    let mut hash = Sha256::new();
    let mut buffer = [0; 8 * 1024];
    loop {
        let count = image
            .read(&mut buffer)
            .map_err(|error| CliFailure::new(error.to_string()))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let id: String = hash
        .finalize()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !super::binary_id_valid(expected) || id != expected {
        return Err(CliFailure::new(
            "host_upgrade_binary_mismatch: executable does not match its expected binary identity",
        ));
    }
    verified.recheck(path)?;
    Ok(verified)
}

pub(crate) fn command(executable: &Path, home: &Path, key: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .env("PLAK_HOME", home)
        .current_dir(home)
        .env_remove(crate::environment::HOST_REEXEC)
        .env_remove("PLAK_TEST_STATE_KEY");
    command.envs(crate::test_hooks::upgrade_key_environment(key));
    command
}

#[cfg(target_os = "macos")]
pub(crate) fn ps_field(pid: u32, field: &str, timeout: Duration) -> CliResult<String> {
    let mut command = Command::new("/bin/ps");
    command
        .args(["-ww", "-p", &pid.to_string(), "-o", field])
        .envs([("LC_ALL", "C"), ("TZ", "UTC")]);
    String::from_utf8(run(command, timeout)?.stdout)
        .map_err(|_| CliFailure::new("host upgrade could not read the running process"))
}

pub(crate) fn run(command: Command, timeout: Duration) -> CliResult<Output> {
    let output = run_output(command, timeout, 1024 * 1024)?;
    if !output.status.success() {
        return Err(CliFailure::new(format!(
            "host_upgrade_destination_refused: {}\n{}",
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stderr)),
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stdout)),
        )));
    }
    Ok(output)
}

/// Bounded raw output: a read-only inspector may return its structured refusal
/// on stdout with a nonzero status. The caller must examine both.
pub(crate) fn run_output(mut command: Command, timeout: Duration, limit: u64) -> CliResult<Output> {
    use std::os::unix::process::CommandExt as _;
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            CliFailure::new(format!("host upgrade destination could not start: {error}"))
        })?;
    let group = rustix::process::Pid::from_raw(
        i32::try_from(child.id())
            .map_err(|_| CliFailure::new("destination pid is out of range"))?,
    )
    .ok_or_else(|| CliFailure::new("destination has no process group"))?;
    let read = |pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.take(limit + 1).read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout =
        read(Box::new(child.stdout.take().ok_or_else(|| {
            CliFailure::new("destination stdout missing")
        })?));
    let stderr =
        read(Box::new(child.stderr.take().ok_or_else(|| {
            CliFailure::new("destination stderr missing")
        })?));
    let deadline = Instant::now() + timeout;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(CliFailure::new(error.to_string())),
            Ok(None) if Instant::now() >= deadline => {
                break Err(CliFailure::new(
                    "host_upgrade_probe_timeout: destination did not finish within its bound",
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Descendants may retain a pipe even after the image exits. This process
    // group belongs exclusively to the probe started above.
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let _ = child.wait();
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let status = result?;
    if stdout.len() as u64 > limit || stderr.len() as u64 > limit {
        return Err(CliFailure::new(
            "host upgrade destination exceeded its output bound",
        ));
    }
    let output = Output {
        status,
        stdout,
        stderr,
    };
    Ok(output)
}
