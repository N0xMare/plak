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
    assert_eq!(output.status.code(), Some(1));
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
        &[
            "run",
            "--account",
            "openai",
            "--model",
            "gpt-6-luna",
            "--effort",
            "none",
            "--",
            "refuse",
        ],
    ] {
        unavailable(home.cli(args));
    }
    {
        let product = home.path().join("home");
        std::fs::write(
            product.join("config.toml"),
            format!(
                "[gateway]\naddress = {:?}\n",
                format!("unix:{}", home.path().join("absent.sock").display()),
            ),
        )
        .unwrap();
        for args in [
            &["status", "codex"][..],
            &["models", "codex"],
            &[
                "run",
                "--account",
                "codex",
                "--model",
                "gpt-6.2-sol",
                "--effort",
                "low",
                "--",
                "refuse",
            ],
        ] {
            unavailable(home.cli(args));
        }
    }
    let secrets = home.path().join("home/secrets");
    assert!(!secrets.exists() || secrets.read_dir().unwrap().next().is_none());
    assert!(!home.path().join(".local/state/plak/subscriptions").exists());
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

#[cfg(feature = "web")]
#[test]
fn host_refuses_an_unreachable_tcp_gateway() {
    let home = Home::new();
    let product = home.path().join("home");
    std::fs::create_dir(&product).unwrap();
    std::fs::set_permissions(&product, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // A reserved port that never answers the versions request.
    let key = product.join("caller-key");
    std::fs::write(&key, "synthetic-fixture-key").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        product.join("config.toml"),
        format!(
            "[gateway]\naddress = \"tcp:{address}\"\nkey_file = {:?}\n",
            key.to_str().unwrap()
        ),
    )
    .unwrap();
    let result = home.cli(&["host"]);
    assert_eq!(result.status.code(), Some(1));
    let error = String::from_utf8(result.stderr).unwrap();
    assert_eq!(error.lines().count(), 1, "{error}");
    assert!(
        error.contains("gateway_unavailable:")
            && error.contains("start the gateway service on that machine"),
        "{error}"
    );
    assert!(!product.join("host").exists());
}

#[test]
fn installers_refuse_regular_and_dangling_gateway_links_before_writing() {
    let home = Home::new();
    let link = home.path().join(".local/bin/plakd-gateway");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::fs::write(&link, "keep me").unwrap();
    for dangling in [false, true] {
        if dangling {
            std::fs::remove_file(&link).unwrap();
            symlink(home.path().join("missing"), &link).unwrap();
        }
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
            assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to replace"));
            assert!(!home.unit().exists());
            assert!(!home.path().join("home").exists());
            if dangling {
                assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
            } else {
                assert_eq!(std::fs::read_to_string(&link).unwrap(), "keep me");
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn installers_refuse_systemd_drop_ins_before_writing() {
    let home = Home::new();
    for (unit, args) in [
        ("plak-gateway.service", &["gateway", "install"][..]),
        #[cfg(feature = "web")]
        (
            "plak-gateway.service",
            &[
                "host",
                "install",
                "--web",
                "unix:/tmp/web.sock",
                "--ui",
                "/tmp/ui",
            ],
        ),
        #[cfg(feature = "web")]
        (
            "plakd-host.service",
            &[
                "host",
                "install",
                "--web",
                "unix:/tmp/web.sock",
                "--ui",
                "/tmp/ui",
            ],
        ),
    ] {
        let runtime = home.path().join("runtime");
        for directory in [
            home.unit().with_file_name(format!("{unit}.d")),
            home.path()
                .join(format!(".config/systemd/user.control/{unit}.d")),
            runtime.join(format!("systemd/user/{unit}.d")),
        ] {
            std::fs::create_dir_all(&directory).unwrap();
            let drop_in = directory.join("override.conf");
            std::fs::write(&drop_in, "[Service]\n").unwrap();
            let output = home
                .command(env!("CARGO_BIN_EXE_plakd"))
                .env("XDG_RUNTIME_DIR", &runtime)
                .args(args)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains(&format!("systemctl --user revert {unit}"))
            );
            assert_eq!(std::fs::read_to_string(&drop_in).unwrap(), "[Service]\n");
            assert!(!home.path().join("home").exists());
            assert!(!home.path().join(".local/bin").exists());
            assert!(!home.unit().exists());
            std::fs::remove_file(drop_in).unwrap();
        }
    }
}

#[cfg(feature = "web")]
#[test]
fn host_install_refuses_the_legacy_host_before_writing() {
    let home = Home::new();
    let legacy = if cfg!(target_os = "macos") {
        home.unit().with_file_name("com.plak.host.plist")
    } else {
        home.unit().with_file_name("plak-host.service")
    };
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "legacy operator definition\n").unwrap();
    let output = home.cli(&[
        "host",
        "install",
        "--web",
        "unix:/tmp/web.sock",
        "--ui",
        "/tmp/ui",
    ]);
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("after the host has restarted on the new build"),
        "{error}"
    );
    assert!(
        error.contains(if cfg!(target_os = "macos") {
            "launchctl bootout"
        } else {
            "systemctl --user disable --now plak-host.service"
        }),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(legacy).unwrap(),
        "legacy operator definition\n"
    );
    assert!(!home.path().join("home").exists());
    assert!(!home.path().join(".local/bin").exists());
    assert!(!home.unit().exists());
}

#[test]
fn installers_refuse_a_gateway_unit_pinned_to_an_old_snapshot() {
    let home = Home::new();
    let unit = home.unit();
    let pinned = "ExecStart=/old/snapshot/plakd gateway serve\n";
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(&unit, pinned).unwrap();
    for (installer, args) in [
        ("gateway", &["gateway", "install"][..]),
        #[cfg(feature = "web")]
        (
            "host",
            &[
                "host",
                "install",
                "--web",
                "unix:/tmp/web.sock",
                "--ui",
                "/tmp/ui",
            ],
        ),
    ] {
        let output = home.cli(args);
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("refusing to overwrite"), "{error}");
        assert!(
            error.contains("rm -- ") && error.contains(&format!("plakd {installer} install")),
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
