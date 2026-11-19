//! Service topology contracts over the shipped binary, with no live manager.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "offline process contracts"
)]

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Home(tempfile::TempDir);

impl Home {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn command(&self, path: impl AsRef<Path>) -> Command {
        let mut command = Command::new(path.as_ref());
        command
            .env_clear()
            .env("HOME", self.path())
            .env("PLAK_HOME", self.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .current_dir(self.path());
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_plakd"))
            .args(args)
            .output()
            .unwrap()
    }

    fn unit(&self) -> PathBuf {
        self.path().join(if cfg!(target_os = "macos") {
            "Library/LaunchAgents/com.plakd.gateway.plist"
        } else {
            ".config/systemd/user/plak-gateway.service"
        })
    }
}

fn successful(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn unavailable(output: Output) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(error.lines().count(), 1, "{error}");
    assert!(error.contains("gateway_unavailable:"), "{error}");
    assert!(error.contains("plakd gateway install"), "{error}");
}

#[test]
fn account_and_status_verbs_refuse_without_a_gateway() {
    let home = Home::new();
    for args in [
        &["accounts", "list"][..],
        &["status"],
        &["key", "status"],
        &["key", "set", "openai"],
        &["gateway", "status"],
        &["gateway", "check"],
    ] {
        unavailable(home.cli(args));
    }
    assert!(!home.path().join("home").exists());
    assert!(!home.path().join(".local/state").exists());
}

#[cfg(feature = "web")]
#[test]
fn host_refuses_without_a_configured_reachable_gateway() {
    let home = Home::new();
    unavailable(home.cli(&["host"]));
    let product = home.path().join("home");
    std::fs::create_dir_all(&product).unwrap();
    std::fs::set_permissions(&product, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        product.join("config.toml"),
        format!(
            "[gateway]\naddress = {:?}",
            format!("unix:{}", home.path().join("absent.sock").display())
        ),
    )
    .unwrap();
    unavailable(home.cli(&["host"]));
    assert!(!product.join("host").exists());
    assert!(!home.path().join(".local/state/plak/subscriptions").exists());
}

#[test]
fn installers_refuse_a_gateway_unit_pinned_to_an_old_snapshot() {
    let home = Home::new();
    let unit = home.unit();
    let pinned = "ExecStart=/old/snapshot/plakd gateway serve\n";
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, pinned).unwrap();
    for args in [
        &["gateway", "install"][..],
        #[cfg(feature = "web")]
        &[
            "host",
            "install",
            "--web",
            "unix:/tmp/web.sock",
            "--ui",
            "/tmp/ui",
        ],
    ] {
        let output = home.cli(args);
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("refusing to overwrite"), "{error}");
        assert!(
            error.contains("rm -- ") && error.contains("plakd gateway install"),
            "{error}"
        );
        assert!(error.contains(unit.to_str().unwrap()), "{error}");
        assert!(!home.path().join(".local/bin/plakd-gateway").exists());
        assert!(!home.path().join("home").exists());
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), pinned);
    }
}

#[test]
fn gateway_upgrade_flips_only_its_command_link() {
    let home = Home::new();
    let dry = successful(home.cli(&["gateway", "install", "--dry-run"]));
    let link = home.path().join(".local/bin/plakd-gateway");
    assert!(dry.contains(link.to_str().unwrap()));
    assert!(!link.exists() && !home.unit().exists());
    successful(home.cli(&["gateway", "install"]));
    let definition = std::fs::read(home.unit()).unwrap();
    let stable = home.path().join(".local/bin/plakd");
    let old = std::fs::read_link(&link).unwrap();
    symlink(&old, &stable).unwrap();
    let socket = home.path().join("home/gateway/gateway.sock");
    let mut gateway = super::gateway_process::GatewayProcess::start(
        &mut home.command(&link),
        &socket,
        &home.path().join("old.log"),
    );
    let first = successful(home.cli(&["gateway", "check"]));
    successful(home.cli(&["gateway", "drain"]));
    gateway.wait_for_drain();

    let replacement = home.path().join("new-plakd");
    let marker = home.path().join("activated");
    std::fs::write(
        &replacement,
        format!(
            "#!/bin/sh\nprintf activated > {}\nexec {} \"$@\"\n",
            shell_words::quote(marker.to_str().unwrap()),
            shell_words::quote(old.to_str().unwrap()),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
    let staged = link.with_extension("next");
    symlink(&replacement, &staged).unwrap();
    std::fs::rename(staged, &link).unwrap();
    successful(home.cli(&["gateway", "install"]));
    let _gateway = super::gateway_process::GatewayProcess::start(
        &mut home.command(&link),
        &socket,
        &home.path().join("new.log"),
    );
    let second = successful(home.cli(&["gateway", "check"]));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "activated");
    assert_ne!(first, second, "the restarted service has a new instance");
    assert_eq!(std::fs::read_link(&stable).unwrap(), old);
    assert_eq!(std::fs::read(home.unit()).unwrap(), definition);
}
