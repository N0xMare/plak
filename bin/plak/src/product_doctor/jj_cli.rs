//! The sole bounded subprocess probe used by doctor; native JJ needs no CLI.
use super::{CheckKind, DoctorCheck, Severity};
use std::io::Read as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Inspect one non-shim CLI on the supplied PATH, without inheriting credentials.
pub fn probe(path: Option<&std::ffi::OsStr>, cwd: &Path) -> DoctorCheck {
    let executable = path.and_then(|path| {
        std::env::split_paths(path)
            .filter(|entry| entry.is_absolute() && !entry.ends_with(".plak/bin"))
            .map(|entry| entry.join("jj"))
            .find(|path| executable(path))
    });
    let Some(executable) = executable else {
        return row(
            Severity::Ok,
            "missing",
            "native workspace operations need no jj CLI".into(),
            None,
            cwd,
        );
    };
    let output = version(&executable, path.unwrap_or_default(), cwd);
    let Some((major, minor, patch_version)) = output.as_deref().and_then(parse_version) else {
        return row(
            Severity::Warning,
            "unavailable",
            "jj --version failed, timed out (5 s), or returned an unrecognized version".into(),
            Some("check jj on PATH"),
            cwd,
        );
    };
    let range = plakous_hosted::JJ_CLI_SUPPORTED;
    let detail = format!(
        "{major}.{minor}.{patch_version} (supported 0.{}-0.{}, library {})",
        range.start(),
        range.end(),
        plakous_hosted::built_jj_library_version().unwrap_or("not built")
    );
    if major == 0 && range.contains(&minor) {
        row(Severity::Ok, "available", detail, None, cwd)
    } else {
        let remedy = if major > 0 || minor > *range.end() {
            format!("pin jj at 0.{} (brew pin jj) or update plak", range.end())
        } else {
            format!("upgrade jj to 0.{} or newer", range.start())
        };
        row(Severity::Warning, "untested", detail, Some(&remedy), cwd)
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    let mut words = output.split_whitespace();
    if words.next()? != "jj" {
        return None;
    }
    let mut parts = words.next()?.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    (parts.next().is_none()).then_some(version)
}

fn row(
    severity: Severity,
    result: &str,
    mut detail: String,
    remedy: Option<&str>,
    cwd: &Path,
) -> DoctorCheck {
    if plakous_hosted::built_jj_library_version().is_none() && !detail.contains("library not built")
    {
        detail.push_str("; library not built");
    }
    if let Some(root) = cwd.ancestors().find(|root| root.join(".jj").is_dir()) {
        let kind = if root.join(".git").is_dir() {
            "colocated"
        } else if root.join(".jj/repo").is_file() {
            "jj workspace"
        } else {
            "jj (not colocated)"
        };
        detail.push_str(&format!("; repo: {kind}"));
    }
    let mut check = DoctorCheck::new(
        CheckKind::JjCli,
        "environment",
        severity,
        result,
        detail,
        None,
    );
    check.remediation = remedy.map(str::to_owned);
    check
}

fn version(executable: &Path, path: &std::ffi::OsStr, cwd: &Path) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut child = Command::new(executable)
        .arg("--version")
        .current_dir(cwd)
        .env_clear()
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let group = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw);
    let stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let _reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(4097).read_to_end(&mut bytes).ok();
        let _ = sender.send(read.and_then(|_| (bytes.len() <= 4096).then_some(bytes)));
    });
    let success = loop {
        let Some(pid) = group else {
            break false;
        };
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOWAIT
                | rustix::process::WaitIdOptions::NOHANG,
        ) {
            Ok(Some(status)) => break status.exit_status() == Some(0),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break false,
        }
    };
    // Pin the group identity with its unreaped leader until descendants retire.
    if let Some(group) = group {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.wait();
    let bytes = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()??;
    success.then(|| String::from_utf8(bytes).ok()).flatten()
}
