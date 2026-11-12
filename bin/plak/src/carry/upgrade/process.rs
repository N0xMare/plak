//! Bounded execution of the exact destination image, never a shell or PATH lookup.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;
#[cfg(any(test, feature = "web"))]
use std::process::{Command, Output, Stdio};
#[cfg(any(test, feature = "web"))]
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::{CliFailure, CliResult};

pub(crate) fn verify_image(path: &Path, expected: &str) -> CliResult<()> {
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
            "host_upgrade_binary_mismatch: executable does not match --expect-binary; source remains authoritative",
        ));
    }
    Ok(())
}

#[cfg(any(test, feature = "web"))]
pub(crate) fn command(executable: &Path, home: &Path, key: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .env("PLAK_HOME", home)
        .current_dir(home)
        .env_remove("PLAK_TEST_STATE_KEY");
    #[cfg(feature = "test-hooks")]
    command.env("PLAK_TEST_STATE_KEY", key);
    #[cfg(not(feature = "test-hooks"))]
    let _ = key;
    command
}

#[cfg(any(test, feature = "web"))]
pub(crate) fn run(mut command: Command, timeout: Duration) -> CliResult<Output> {
    use std::os::unix::process::CommandExt as _;
    const LIMIT: u64 = 1024 * 1024;
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
            let _ = pipe.take(LIMIT + 1).read_to_end(&mut bytes);
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
    if stdout.len() as u64 > LIMIT || stderr.len() as u64 > LIMIT {
        return Err(CliFailure::new(
            "host upgrade destination exceeded its output bound",
        ));
    }
    let output = Output {
        status,
        stdout,
        stderr,
    };
    if !output.status.success() {
        return Err(CliFailure::new(format!(
            "host_upgrade_destination_refused: {}",
            crate::cli_output::safe_terminal_block(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    Ok(output)
}
