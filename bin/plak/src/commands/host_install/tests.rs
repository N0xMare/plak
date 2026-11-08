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
    for home in ["/tmp/home=bad", "/tmp/home:bad"] {
        assert!(
            render(Path::new(home), Path::new("/tmp/product"), &args())
                .is_err_and(|error| error.to_string().contains("HOME must not contain"))
        );
    }
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

#[test]
fn host_install_refuses_same_length_edits_and_same_length_symlinks() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("plak-host.service");
    let unit = format!("{MARKER}[Service]\n#{}\n", "x".repeat(128));
    let changed = unit.replace("[Service]", "[Servicx]");
    assert_eq!(unit.len(), changed.len());
    std::fs::write(&path, &changed)?;
    assert!(install_file(&path, &unit).is_err());
    assert_eq!(std::fs::read_to_string(&path)?, changed);
    std::fs::remove_file(&path)?;
    let prefix = root.path().as_os_str().len() + 1;
    let target = root.path().join(
        "t".repeat(
            unit.len()
                .checked_sub(prefix)
                .ok_or("fixture path too long")?,
        ),
    );
    std::fs::write(&target, &unit)?;
    std::os::unix::fs::symlink(&target, &path)?;
    assert_eq!(std::fs::symlink_metadata(&path)?.len(), unit.len() as u64);
    assert!(install_file(&path, &unit).is_err());
    assert!(std::fs::symlink_metadata(&path)?.is_symlink());
    assert_eq!(std::fs::read_to_string(target)?, unit);
    Ok(())
}

#[test]
fn host_install_publication_cannot_replace_a_racing_unit() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join("plak-host.service");
    let unit = format!("{MARKER}[Service]\n");
    assert!(!matching_unit(&path, &unit)?);
    let planted = "[Service]\nExecStart=/unrelated\n";
    std::fs::write(&path, planted)?;
    assert!(publish_unit(&path, &unit).is_err());
    assert_eq!(std::fs::read_to_string(path)?, planted);
    Ok(())
}

#[test]
fn host_install_config_home_must_resolve_to_the_unit_search_path() -> TestResult {
    let root = tempfile::tempdir()?;
    let expected = root.path().join(".config");
    validate_config_home(root.path(), None)?;
    validate_config_home(root.path(), Some(&expected))?;
    assert!(validate_config_home(root.path(), Some(&root.path().join("other"))).is_err());
    std::fs::create_dir(&expected)?;
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&expected, &alias)?;
    validate_config_home(root.path(), Some(&alias))?;
    Ok(())
}
