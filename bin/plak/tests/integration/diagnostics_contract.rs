//! Process-level diagnostics contracts for optional `PLAK_LOG` file logging.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test crate may unwrap"
)]

use std::process::Command;

fn plak_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_plak"))
}

fn plak_command(home: &std::path::Path) -> Command {
    let mut command = Command::new(plak_bin());
    command
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_STATE_HOME", home.join("xdg-state"));
    command
}

fn isolated_home() -> (tempfile::TempDir, std::path::PathBuf) {
    let base =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plak-test-homes");
    std::fs::create_dir_all(&base).expect("plak test home base");
    let temporary = tempfile::tempdir_in(&base).expect("isolated tempdir");
    let home = temporary.path().join("home");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir(&home).expect("home dir");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
            .expect("private home");
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(&home).expect("home dir");
    }
    (temporary, home)
}

#[test]
fn hidden_version_probes_do_not_log_startup_attribution() {
    for probe in ["--state-key", "--reload-probe", "--export-version"] {
        let (_guard, home) = isolated_home();
        let output = plak_command(&home)
            .env("PLAK_LOG", "info")
            .args(["version", probe])
            .output()
            .expect("spawn version probe");
        assert!(output.status.success(), "{probe}: {output:?}");
        let log = std::fs::read_to_string(home.join("logs/plak.log")).unwrap_or_default();
        assert!(
            !log.contains("plak.start:"),
            "{probe} logged startup: {log}"
        );
    }
}

#[test]
fn startup_log_attributes_one_content_free_line_to_the_exact_build() {
    let (_guard, home) = isolated_home();
    let output = plak_command(&home)
        .env("PLAK_LOG", "info")
        .arg("version")
        .output()
        .expect("spawn plak version");
    assert!(output.status.success());
    let version = String::from_utf8(output.stdout).expect("version text");
    let binary = version.split("binary ").nth(1).expect("binary id");
    let binary = &binary[..12];
    assert!(binary.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let output = plak_command(&home)
        .env("PLAK_LOG", "off")
        .args(["version", "--state-key"])
        .output()
        .expect("spawn state key query");
    assert!(output.status.success());
    let state_key = String::from_utf8(output.stdout).expect("state key text");
    let log_path = home.join("logs/plak.log");
    let contents = std::fs::read_to_string(&log_path).expect("read diagnostics log");
    let lines = contents
        .lines()
        .filter(|line| line.contains("plak.start:"))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "one startup line per process: {contents}");
    assert!(lines[0].ends_with(&format!(
        "INFO plak.start: binary={binary} state_key={} command=version",
        state_key.trim()
    )));

    // A subcommand argument must never become the logged verb or another field.
    let output = plak_command(&home)
        .env("PLAK_LOG", "info")
        .args(["help", "private-argv-sentinel"])
        .output()
        .expect("spawn help");
    assert!(output.status.success());
    let contents = std::fs::read_to_string(log_path).expect("read diagnostics log");
    let lines = contents
        .lines()
        .filter(|line| line.contains("plak.start:"))
        .collect::<Vec<_>>();
    // Short commands can exit before their background identity is available.
    assert!(matches!(lines.len(), 1 | 2));
    if let Some(line) = lines.get(1) {
        assert!(line.ends_with(&format!(
            "INFO plak.start: binary={binary} state_key={} command=help",
            state_key.trim()
        )));
    }
    assert!(!contents.contains("private-argv-sentinel"));
    assert!(!contents.contains(&home.display().to_string()));
}

#[test]
fn unset_plak_log_respects_build_default() {
    let (_guard, home) = isolated_home();
    let output = plak_command(&home)
        .env_remove("PLAK_LOG")
        .args(["version"])
        .output()
        .expect("spawn plak version");
    assert!(output.status.success(), "version must succeed: {output:?}");
    let log_path = home.join("logs/plak.log");
    #[cfg(feature = "maintainer")]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert!(
            log_path.is_file(),
            "maintainer diagnostics must be enabled by default"
        );
        assert_eq!(
            std::fs::metadata(&log_path)
                .expect("log metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "default maintainer diagnostics must remain private"
        );
    }
    #[cfg(not(feature = "maintainer"))]
    assert!(
        !log_path.exists(),
        "non-maintainer diagnostics log must not be created when PLAK_LOG is unset"
    );
    assert!(
        !home.join("state").exists(),
        "opening the diagnostics log must not create durable state"
    );
}

/// Read-only diagnosis never opens the diagnostics log, so an invalid
/// `PLAK_LOG` cannot fail it; a stateful command warns and still succeeds.
#[test]
fn invalid_plak_log_warns_without_failing_commands() {
    let (_guard, home) = isolated_home();
    // Credential storage refuses `..` and symlinked components.
    let home = std::fs::canonicalize(home).expect("canonical home");
    let output = plak_command(&home)
        .env("PLAK_LOG", "info=not_a_real_level")
        .arg("doctor")
        .output()
        .expect("spawn plak doctor");
    assert!(output.status.success(), "doctor must succeed: {output:?}");
    let output = plak_command(&home)
        .env("PLAK_LOG", "info=not_a_real_level")
        .args(["config", "set", "tui.mouse", "true"])
        .output()
        .expect("spawn plak config set");
    assert!(
        output.status.success(),
        "config set must succeed: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("diagnostics unavailable"),
        "expected diagnostics warning on stderr: {stderr}"
    );
}

#[test]
fn plak_log_writes_content_free_diagnostics_log() {
    let (_guard, home) = isolated_home();
    // `telemetry status` only crosses the `telemetry_store.open` boundary span
    // when the store exists; create it first so the run emits at least one span.
    let paths = plak::home::paths::PlakPaths::explicit(home.clone());
    paths.ensure_layout().expect("layout");
    drop(plak::telemetry_store::TelemetryStore::open(&paths).expect("create telemetry store"));
    let output = plak_command(&home)
        .env("PLAK_LOG", "info")
        .args(["telemetry", "status"])
        .output()
        .expect("spawn plak telemetry status");
    assert!(
        output.status.success(),
        "telemetry status must succeed: {output:?}"
    );
    let log_path = home.join("logs/plak.log");
    assert!(log_path.is_file(), "diagnostics log should exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&log_path)
                .expect("log metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let contents = std::fs::read_to_string(&log_path).expect("read diagnostics log");
    assert!(
        !contents.is_empty(),
        "diagnostics log should contain subscriber output"
    );
}
