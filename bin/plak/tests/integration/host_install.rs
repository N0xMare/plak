//! Service installation uses a fake user manager; never a real service.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    product: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("user");
        let product = root.path().join("home");
        let log = root.path().join("manager.log");
        let bin = home.join(".local/bin");
        std::fs::create_dir(&home)?;
        std::fs::create_dir_all(&bin)?;
        let manager = bin.join("systemctl");
        std::fs::write(
            &manager,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nenv >> {}\nif [ \"$2\" = is-system-running ]; then printf 'offline\\n'; fi\n",
                shell_words::quote(&log.to_string_lossy()),
                shell_words::quote(&log.to_string_lossy())
            ),
        )?;
        std::fs::set_permissions(manager, std::fs::Permissions::from_mode(0o755))?;
        Ok(Self {
            root,
            home,
            product,
            log,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_plak"));
        command
            .args(["host", "install"])
            .env_clear()
            .env("HOME", &self.home)
            .env("PLAK_HOME", &self.product)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.home.join(".local/bin").display()),
            )
            .env("UNRELATED_SECRET", "must-not-be-copied")
            .current_dir(self.root.path());
        command
    }

    fn install(&self, dry_run: bool) -> std::io::Result<Output> {
        let mut command = self.command();
        command
            .arg("--web")
            .arg(format!(
                "unix:{}",
                self.root.path().join("web socket.sock").display()
            ))
            .arg("--ui")
            .arg(self.root.path().join("ui build"));
        if dry_run {
            command.arg("--dry-run");
        }
        command.output()
    }

    fn unit(&self) -> PathBuf {
        self.home.join(".config/systemd/user/plak-host.service")
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "test-hooks")]
impl Drop for Fixture {
    fn drop(&mut self) {
        super::offline_support::kill_home_local_runner(&self.product);
    }
}

#[cfg(feature = "test-hooks")]
fn wait_host_log(
    path: &Path,
    child: &mut super::offline_support::TestChild,
    needle: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let until = std::time::Instant::now() + super::offline_support::WAIT;
    loop {
        let log = std::fs::read_to_string(path).unwrap_or_default();
        if log.contains(needle) {
            return Ok(log);
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("host exited {status} before {needle}: {log}").into());
        }
        if std::time::Instant::now() >= until {
            return Err(format!("host never reported {needle}: {log}").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn host_install_generated_command_serves_from_home_with_a_fake_manager() -> TestResult {
    use super::offline_support::TestChild;
    use std::process::Stdio;

    let mut fixture = Fixture::new()?;
    // Match the systemd default cwd and default product home, not a workspace.
    fixture.product = fixture.home.join(".plak");
    success(&fixture.install(false)?);
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_plak"),
        fixture.home.join(".local/bin/plak"),
    )?;
    std::fs::create_dir_all(&fixture.product)?;
    std::fs::set_permissions(&fixture.product, std::fs::Permissions::from_mode(0o700))?;
    std::fs::write(
        fixture.product.join("config.toml"),
        "[web]\nallow = [\"owner@example.test\"]\norigin = \"https://example.test\"\n",
    )?;
    let ui = fixture.root.path().join("ui build");
    std::fs::create_dir(&ui)?;
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>fixture</title>",
    )?;
    std::fs::write(
        ui.join("csp.json"),
        r#"{"header":"default-src 'self'; frame-ancestors 'none'"}"#,
    )?;
    let unit = std::fs::read_to_string(fixture.unit())?;
    assert!(!unit.contains("WorkingDirectory="));
    let start = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
        .ok_or("unit has no ExecStart")?;
    // Emulate only the manager's substitutions; execute the rendered argv,
    // including env -i and the installed stable link, without a real manager.
    let start = start
        .replace("%t", &fixture.root.path().to_string_lossy())
        .replace("${INVOCATION_ID}", "fixture");
    let argv = shell_words::split(&start)?;
    let log = fixture.root.path().join("host.log");
    let mut child = TestChild::spawn(
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&fixture.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log)?),
    );
    wait_host_log(&log, &mut child, "plak host: serving on ")?;
    wait_host_log(&fixture.log, &mut child, "--user\nis-system-running\n")?;
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id())?).ok_or("invalid pid")?;
    rustix::process::kill_process(pid, rustix::process::Signal::USR1)?;
    wait_host_log(&log, &mut child, "plak host: UI build reloaded")?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    let until = std::time::Instant::now() + super::offline_support::WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            assert!(status.success(), "{}", std::fs::read_to_string(&log)?);
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "host did not stop after SIGTERM"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!fixture.root.path().join("web socket.sock").exists());
    Ok(())
}

#[test]
fn host_install_dry_run_touches_nothing_and_refuses_unsafe_flags() -> TestResult {
    let fixture = Fixture::new()?;
    let output = fixture.install(true)?;
    success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("# Generated by plak host install.")
    );
    assert!(!fixture.home.join(".config").exists());
    assert!(!fixture.product.exists());
    assert!(!fixture.log.exists());
    for arguments in [
        vec![],
        vec!["--web", "unix:relative", "--ui", "/ui"],
        vec!["--web", "unix:/socket", "--ui", "relative"],
        vec!["--web", "127.0.0.1:8000", "--ui", "/ui"],
        vec![
            "--web",
            "unix:/socket",
            "--ui",
            "/ui",
            "--dev-login",
            "anyone",
        ],
        vec!["--web", "unix:/socket", "--ui", "/ui", "--dev-login"],
    ] {
        let output = fixture
            .command()
            .args(arguments)
            .arg("--dry-run")
            .output()?;
        assert!(!output.status.success());
        assert!(!fixture.unit().exists());
    }
    Ok(())
}

#[test]
fn host_install_reports_manager_failure_without_removing_the_complete_unit() -> TestResult {
    let fixture = Fixture::new()?;
    std::fs::write(
        fixture.home.join(".local/bin/systemctl"),
        "#!/bin/sh\nexit 1\n",
    )?;
    let output = fixture.install(false)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unit written"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("daemon-reload failed"));
    assert!(std::fs::read_to_string(fixture.unit())?.ends_with("WantedBy=default.target\n"));
    Ok(())
}

#[test]
fn host_install_reloads_only_after_atomic_creation_and_is_idempotent() -> TestResult {
    let fixture = Fixture::new()?;
    let output = fixture.install(false)?;
    success(&output);
    let next = String::from_utf8(output.stdout)?;
    for line in [
        "systemctl --user enable --now plak-host.service",
        "loginctl enable-linger \"$USER\"",
        "journalctl --user -u plak-host.service -f",
    ] {
        assert!(next.contains(line), "{next}");
    }
    let serve = next
        .lines()
        .find(|line| line.starts_with("tailscale serve "))
        .ok_or("missing Serve command")?;
    assert_eq!(
        shell_words::split(serve)?,
        [
            "tailscale".to_owned(),
            "serve".to_owned(),
            "--bg".to_owned(),
            format!(
                "unix:{}",
                fixture.root.path().join("web socket.sock").display()
            ),
        ]
    );
    let log = std::fs::read_to_string(&fixture.log)?;
    assert!(log.starts_with("--user\ndaemon-reload\n"), "{log}");
    assert!(!log.contains("UNRELATED_SECRET"));
    assert!(!log.contains("enable"));
    let unit = std::fs::read_to_string(fixture.unit())?;
    assert!(unit.contains(&format!("{}/.local/bin/plak", fixture.home.display())));
    assert!(!unit.contains("must-not-be-copied"));
    assert_eq!(
        std::fs::metadata(fixture.unit())?.permissions().mode() & 0o777,
        0o644
    );
    std::fs::remove_file(&fixture.log)?;
    success(&fixture.install(false)?);
    assert!(
        !fixture.log.exists(),
        "identical install must not call the manager"
    );
    assert_eq!(std::fs::read_to_string(fixture.unit())?, unit);
    Ok(())
}

#[test]
fn host_install_refuses_an_unrelated_unit_without_calling_the_manager() -> TestResult {
    let fixture = Fixture::new()?;
    std::fs::create_dir_all(fixture.unit().parent().unwrap_or_else(|| Path::new(".")))?;
    std::fs::write(fixture.unit(), "[Service]\nExecStart=/unrelated\n")?;
    let output = fixture.install(false)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to overwrite"));
    assert_eq!(
        std::fs::read_to_string(fixture.unit())?,
        "[Service]\nExecStart=/unrelated\n"
    );
    assert!(!fixture.log.exists());
    Ok(())
}
