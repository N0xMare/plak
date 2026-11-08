use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn args() -> HostInstall {
    HostInstall {
        web: "unix:/home/user/.plak/host/web/web.sock".into(),
        ui: "/home/user/.plak/web-ui".into(),
        dry_run: false,
    }
}

#[test]
fn host_install_unit_matches_the_service_golden() -> TestResult {
    let unit = render(
        Path::new("/home/user"),
        Path::new("/home/user/.plak"),
        &args(),
    )?;
    assert_eq!(
        unit,
        include_str!("../../../tests/goldens/plak-host.service")
    );
    assert!(!unit.contains("--dev-login"));
    Ok(())
}

#[test]
fn host_install_refuses_relative_dev_and_injected_paths() {
    let home = Path::new("/home/user");
    for web in [
        "unix:relative.sock",
        "127.0.0.1:1234",
        "unix:/tmp/a\nExecStart=bad",
    ] {
        assert!(
            render(
                home,
                home,
                &HostInstall {
                    web: web.into(),
                    ..args()
                }
            )
            .is_err()
        );
    }
    assert!(
        render(
            home,
            home,
            &HostInstall {
                ui: "relative".into(),
                ..args()
            }
        )
        .is_err()
    );
    assert!(render(Path::new("relative"), home, &args()).is_err());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn host_install_refuses_unsupported_platform_before_reading_paths() {
    let error = run(&args(), &mut Vec::new());
    assert!(error.is_err_and(|error| {
        error
            .to_string()
            .contains("not yet supported on this platform")
    }));
}

#[test]
fn host_install_quotes_systemd_expansions_and_shell_paths() -> TestResult {
    let unit = render(Path::new("/tmp/a $%\""), Path::new("/tmp/home"), &args())?;
    assert!(unit.contains("\"HOME=/tmp/a $$%%\\\"\""));
    assert!(
        !unit.contains("WorkingDirectory="),
        "retain the systemd user default HOME"
    );
    assert!(unit.contains("\"INVOCATION_ID=${INVOCATION_ID}\""));
    Ok(())
}

#[test]
fn host_install_never_overwrites_changed_or_symlinked_units() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("user/plak-host.service");
    let unit = format!("{MARKER}[Service]\n");
    assert!(install_file(&path, &unit)?);
    assert_eq!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777,
        0o644
    );
    assert!(!install_file(&path, &unit)?);
    let changed = format!("{unit}# edited\n");
    std::fs::write(&path, &changed)?;
    assert!(install_file(&path, &unit).is_err());
    assert_eq!(std::fs::read_to_string(&path)?, changed);
    std::fs::remove_file(&path)?;
    let other = root.path().join("other");
    std::fs::write(&other, &unit)?;
    std::os::unix::fs::symlink(&other, &path)?;
    assert!(install_file(&path, &unit).is_err());
    assert_eq!(std::fs::read_to_string(other)?, unit);
    Ok(())
}
