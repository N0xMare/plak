//! `LaunchAgent` installation never contacts a real service manager.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let bin = root.path().join(".local/bin");
        std::fs::create_dir_all(&bin)?;
        let manager = bin.join("launchctl");
        std::fs::write(
            &manager,
            "#!/bin/sh\ntouch \"$HOME/manager-called\"\nexit 99\n",
        )?;
        std::fs::set_permissions(manager, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self { root })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plak"));
        command
            .args([
                "host",
                "install",
                "--web",
                "unix:/tmp/plak-install.sock",
                "--ui",
                "/tmp/ui build",
            ])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PLAK_HOME", self.root.path().join("product"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    self.root.path().join(".local/bin").display()
                ),
            )
            .env("UNRELATED_SECRET", "must-not-be-copied")
            .current_dir(self.root.path());
        command
    }

    fn plist(&self) -> PathBuf {
        self.root
            .path()
            .join("Library/LaunchAgents/com.plak.host.plist")
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn launchd_install_dry_run_creates_no_files_or_manager_calls() -> TestResult {
    let fixture = Fixture::new()?;
    let output = fixture.command().arg("--dry-run").output()?;
    success(&output);
    assert!(output.stdout.starts_with(b"<?xml version="));
    assert!(!fixture.root.path().join("Library").exists());
    assert!(!fixture.root.path().join("product").exists());
    assert!(!fixture.root.path().join("manager-called").exists());
    let output = fixture.command().args(["--dev-login", "anyone"]).output()?;
    assert!(!output.status.success());
    assert!(!fixture.root.path().join("Library").exists());
    Ok(())
}

#[test]
fn launchd_install_is_atomic_idempotent_and_never_loads_or_starts() -> TestResult {
    let fixture = Fixture::new()?;
    let output = fixture.command().output()?;
    success(&output);
    let instructions = String::from_utf8(output.stdout)?;
    for command in [
        "launchctl bootstrap gui/",
        "launchctl kickstart gui/",
        "tail -F ",
        "launchctl bootout gui/",
        "rm -- ",
    ] {
        assert!(instructions.contains(command), "{instructions}");
    }
    assert!(!fixture.root.path().join("manager-called").exists());
    assert!(!fixture.root.path().join("product").exists());
    let plist = std::fs::read_to_string(fixture.plist())?;
    assert!(plist.contains(&format!(
        "{}/.local/bin/plak",
        fixture.root.path().display()
    )));
    assert!(!plist.contains("must-not-be-copied"));
    assert!(!plist.contains("UNRELATED_SECRET"));
    assert_eq!(
        std::fs::metadata(fixture.plist())?.permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        std::fs::metadata(fixture.root.path().join("Library/Logs/Plak"))?
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for log in ["host.stdout.log", "host.stderr.log"] {
        let path = fixture.root.path().join("Library/Logs/Plak").join(log);
        assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
    }
    let reinstalled = fixture.command().output()?;
    success(&reinstalled);
    assert!(String::from_utf8_lossy(&reinstalled.stdout).starts_with("Already installed"));
    assert_eq!(std::fs::read_to_string(fixture.plist())?, plist);
    assert!(!fixture.root.path().join("manager-called").exists());
    Ok(())
}

#[test]
fn launchd_install_refuses_foreign_edited_and_symlinked_agents() -> TestResult {
    let fixture = Fixture::new()?;
    let path = fixture.plist();
    std::fs::create_dir_all(path.parent().ok_or("plist parent")?)?;
    std::fs::write(&path, "foreign")?;
    assert!(!fixture.command().output()?.status.success());
    assert_eq!(std::fs::read_to_string(&path)?, "foreign");
    assert!(!fixture.root.path().join("Library/Logs").exists());
    std::fs::remove_file(&path)?;
    success(&fixture.command().output()?);
    let plist = std::fs::read_to_string(&path)?;
    let changed = plist.replace("com.plak.host", "com.plak.hosx");
    std::fs::write(&path, &changed)?;
    assert!(!fixture.command().output()?.status.success());
    assert_eq!(std::fs::read_to_string(&path)?, changed);
    std::fs::remove_file(&path)?;
    let other = fixture.root.path().join("other.plist");
    std::fs::write(&other, &plist)?;
    std::os::unix::fs::symlink(&other, &path)?;
    assert!(!fixture.command().output()?.status.success());
    assert!(std::fs::symlink_metadata(&path)?.is_symlink());
    assert_eq!(std::fs::read_to_string(other)?, plist);
    assert!(!fixture.root.path().join("manager-called").exists());
    Ok(())
}

#[test]
fn launchd_install_refuses_symlinked_and_public_logs_without_overwriting_them() -> TestResult {
    let fixture = Fixture::new()?;
    let logs = fixture.root.path().join("Library/Logs/Plak");
    std::fs::create_dir_all(&logs)?;
    let other = fixture.root.path().join("other");
    std::fs::write(&other, "do not overwrite")?;
    let log = logs.join("host.stderr.log");
    std::os::unix::fs::symlink(&other, &log)?;
    assert!(!fixture.command().output()?.status.success());
    assert!(!fixture.plist().exists());
    assert_eq!(std::fs::read_to_string(other)?, "do not overwrite");
    std::fs::remove_file(&log)?;
    std::fs::write(&log, "existing public log")?;
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644))?;
    assert!(!fixture.command().output()?.status.success());
    assert!(!fixture.plist().exists());
    assert_eq!(std::fs::read_to_string(log)?, "existing public log");
    assert!(!fixture.root.path().join("manager-called").exists());
    Ok(())
}
