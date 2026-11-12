use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn args() -> HostInstall {
    HostInstall {
        web: "unix:/tmp/plak-web.sock".into(),
        ui: "/tmp/user/.plak/web-ui".into(),
        dry_run: false,
    }
}

#[test]
fn host_install_launchd_matches_the_plist_golden() -> TestResult {
    let plist = render(
        Path::new("/tmp/user"),
        Path::new("/tmp/user/.plak"),
        &args(),
    )?;
    assert_eq!(
        plist,
        include_str!("../../../../tests/goldens/com.plak.host.plist")
    );
    assert!(!plist.contains("--dev-login"));
    Ok(())
}

#[test]
fn host_install_launchd_quotes_xml_without_shell_or_manager_expansion() -> TestResult {
    let plist = render(
        Path::new("/tmp/a &<>\"' $%"),
        Path::new("/tmp/product &<>\"'"),
        &HostInstall {
            ui: "/tmp/UI &<>\"' $%".into(),
            ..args()
        },
    )?;
    assert!(plist.contains("HOME=/tmp/a &amp;&lt;&gt;&quot;&apos; $%"));
    assert!(plist.contains("<string>/tmp/UI &amp;&lt;&gt;&quot;&apos; $%</string>"));
    assert!(plist.contains("&quot;SHELL=${SHELL}&quot;"));
    assert!(plist.contains(
        "<key>WorkingDirectory</key>\n  <string>/tmp/a &amp;&lt;&gt;&quot;&apos; $%</string>"
    ));
    Ok(())
}

#[test]
fn host_install_launchd_refuses_unsafe_and_overlong_paths() {
    let home = Path::new("/tmp/home");
    for web in [
        "tcp:127.0.0.1:1000",
        "unix:relative",
        "unix:/tmp/a\n<key>bad</key>",
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
    for path in ["relative", "/tmp/a\n", "/tmp/a\u{fffe}", "/tmp/a\u{ffff}"] {
        assert!(
            render(
                home,
                home,
                &HostInstall {
                    ui: path.into(),
                    ..args()
                }
            )
            .is_err()
        );
    }
    for home in ["relative", "/tmp/a=b", "/tmp/a:b", "/tmp/a\n"] {
        assert!(render(Path::new(home), Path::new("/tmp/product"), &args()).is_err());
    }
    let web = format!("unix:/{}", "x".repeat(102));
    assert!(
        render(
            home,
            home,
            &HostInstall {
                web: web.clone(),
                ..args()
            }
        )
        .is_ok()
    );
    assert!(
        render(
            home,
            home,
            &HostInstall {
                web: format!("{web}x"),
                ..args()
            }
        )
        .is_err()
    );
}

#[test]
fn host_install_launchd_reuses_atomic_no_replace_publication() -> TestResult {
    let root = tempfile::tempdir()?;
    let path = root.path().join(PLIST);
    let plist = render(Path::new("/tmp/user"), Path::new("/tmp/product"), &args())?;
    assert!(super::super::install_file(&path, &plist)?);
    assert!(!super::super::install_file(&path, &plist)?);
    let changed = plist.replace("com.plak.host", "com.plak.hosx");
    std::fs::write(&path, &changed)?;
    assert!(super::super::install_file(&path, &plist).is_err());
    assert_eq!(std::fs::read_to_string(&path)?, changed);
    std::fs::remove_file(&path)?;
    let other = root.path().join("other");
    std::fs::write(&other, &plist)?;
    std::os::unix::fs::symlink(&other, &path)?;
    assert!(super::super::install_file(&path, &plist).is_err());
    assert_eq!(std::fs::read_to_string(other)?, plist);
    Ok(())
}

#[test]
fn host_install_launchd_next_steps_are_explicit_and_shell_quoted() -> TestResult {
    let home = Path::new("/tmp/a '$HOME");
    let instructions = next_steps(home, &args(), 501);
    let commands = instructions
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(shell_words::split)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        commands[0],
        [
            "launchctl",
            "bootstrap",
            "gui/501",
            "/tmp/a '$HOME/Library/LaunchAgents/com.plak.host.plist"
        ]
    );
    assert_eq!(
        commands[1],
        ["launchctl", "kickstart", "gui/501/com.plak.host"]
    );
    assert_eq!(
        commands[3],
        [
            "tail",
            "-F",
            "/tmp/a '$HOME/Library/Logs/Plak/host.stdout.log",
            "/tmp/a '$HOME/Library/Logs/Plak/host.stderr.log"
        ]
    );
    assert_eq!(commands[4][..3], ["launchctl", "bootout", "gui/501"]);
    assert_eq!(
        commands[5],
        [
            "rm",
            "--",
            "/tmp/a '$HOME/Library/LaunchAgents/com.plak.host.plist"
        ]
    );
    Ok(())
}

#[test]
fn host_install_launchd_command_clears_environment_and_preserves_arguments() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir()?;
    let home = root.path().join("a & '$HOME");
    let binary = home.join(".local/bin/plak");
    std::fs::create_dir_all(binary.parent().ok_or("binary parent")?)?;
    std::fs::write(
        &binary,
        "#!/bin/sh\nprintf 'argument:%s\\n' \"$@\"\n/usr/bin/env\n",
    )?;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
    let arguments = service_arguments(&home, Path::new("/tmp/product"), &args(), 103)?;
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", SHELL_COMMAND, "plak-host"])
        .args(&arguments)
        .env_clear()
        .env("SHELL", "/bin/sh")
        .env("USER", "example")
        .env("LOGNAME", "example")
        .env("UNRELATED_SECRET", "must-not-survive")
        .env("PLAK_TEST_MODEL", "must-not-survive")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout)?;
    assert!(output.starts_with("argument:host\nargument:--web\nargument:unix:/tmp/plak-web.sock\nargument:--ui\nargument:/tmp/user/.plak/web-ui\n"));
    for value in [
        format!("HOME={}", home.display()),
        "PLAK_HOME=/tmp/product".into(),
        "SHELL=/bin/sh".into(),
        "USER=example".into(),
        "LOGNAME=example".into(),
    ] {
        assert!(output.lines().any(|line| line == value), "{output}");
    }
    assert!(!output.contains("must-not-survive"));
    assert!(!output.contains("UNRELATED_SECRET"));
    assert!(!output.contains("PLAK_TEST_MODEL"));
    Ok(())
}
