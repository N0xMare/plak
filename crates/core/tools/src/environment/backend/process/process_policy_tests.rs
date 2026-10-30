use super::*;

#[test]
fn direct_pty_uses_revalidated_alias_as_literal_program_name() {
    let temporary = tempfile::tempdir().expect("workspace");
    let startup = temporary.path().join("startup");
    fs::write(&startup, "printf unsafe; exit 99").expect("startup trap");
    // The direct host sees the enrolled alias; shell metacharacters in its
    // path are literal executable-name data, never launcher code.
    let alias = temporary.path().join("not mounted; $(exit 98)/sh");
    let program = Path::new("/bin/bash").canonicalize().expect("bash");
    fs::create_dir(alias.parent().expect("alias parent")).expect("alias directory");
    std::os::unix::fs::symlink(&program, &alias).expect("shell alias");
    let builder = joined_pty_command(
        &program,
        Some(&alias),
        [
            "-c",
            "case :$SHELLOPTS: in *:posix:*) printf '%s' \"$0\";; *) exit 97;; esac",
        ],
    );
    let argv = builder.get_argv();
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("BASH_ENV", &startup)
        .env("ENV", &startup)
        .output()
        .expect("exec launcher");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        alias.to_str().expect("alias text").as_bytes()
    );
    assert_eq!(output.stderr, [] as [u8; 0]);
}

#[test]
fn login_profile_cannot_unset_the_discovery_ceiling() {
    let root = tempfile::tempdir().expect("profile root");
    let profile = root.path().join("profile");
    fs::write(&profile, "unset GIT_CEILING_DIRECTORIES\n").expect("profile");
    let process = HostProcessContext {
        fallback_path: "/usr/bin:/bin".into(),
        home: None,
        prefix: None,
        discovery_ceiling: Some(root.path().to_path_buf()),
    };
    let wrapped = restore_path_after_login(&process, "printf '%s' \"$GIT_CEILING_DIRECTORIES\"")
        .expect("wrapper");
    let output = Command::new("/bin/sh")
        .args([
            "-c",
            &format!(". \"$1\";\n{wrapped}"),
            "profile",
            profile.to_str().expect("path"),
        ])
        .output()
        .expect("login profile");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        root.path().to_str().expect("root").as_bytes()
    );
}

#[test]
fn prepared_path_uses_snapshot_and_nonempty_delegation_precedence() {
    let process = HostProcessContext {
        fallback_path: "/captured/bin".into(),
        home: Some("/captured/home".into()),
        prefix: None,
        discovery_ceiling: None,
    };
    let name = EnvironmentVariableName::new("PATH").expect("PATH");
    assert_eq!(
        host_process_path(&process, &[]).expect("fallback"),
        "/captured/bin"
    );
    assert_eq!(
        host_process_path(
            &process,
            &[(name.clone(), EnvironmentVariableValue("".into()))]
        )
        .expect("empty delegation"),
        "/captured/bin"
    );
    assert_eq!(
        host_process_path(
            &process,
            &[(name, EnvironmentVariableValue("/delegated/bin".into()))]
        )
        .expect("delegation"),
        "/delegated/bin"
    );
    assert_eq!(
        restore_path_after_login(&process, "printf plain").expect("no prefix"),
        "printf plain"
    );
}

#[test]
fn prepared_home_prefers_a_curated_value_over_the_captured_host_home() {
    let process = HostProcessContext {
        fallback_path: "/captured/bin".into(),
        home: Some("/captured/home".into()),
        prefix: None,
        discovery_ceiling: None,
    };
    let name = EnvironmentVariableName::new("HOME").expect("HOME");
    assert_eq!(
        host_process_home(&process, &[]),
        Some(Path::new("/captured/home"))
    );
    assert_eq!(
        host_process_home(
            &process,
            &[(name.clone(), EnvironmentVariableValue("".into()))]
        ),
        Some(Path::new("/captured/home"))
    );
    assert_eq!(
        host_process_home(
            &process,
            &[(name, EnvironmentVariableValue("/delegated/home".into()))]
        ),
        None
    );
    let homeless = HostProcessContext {
        fallback_path: "/captured/bin".into(),
        home: None,
        prefix: None,
        discovery_ceiling: None,
    };
    assert_eq!(host_process_home(&homeless, &[]), None);
}

#[test]
fn program_lookup_skips_a_non_executable_file_earlier_on_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().expect("tempdir");
    let shadow = root.path().join("shadow");
    let real = root.path().join("real");
    fs::create_dir(&shadow).expect("shadow dir");
    fs::create_dir(&real).expect("real dir");
    fs::write(shadow.join("tool"), "# sourced, not run\n").expect("shadow file");
    fs::set_permissions(shadow.join("tool"), fs::Permissions::from_mode(0o644))
        .expect("shadow mode");
    fs::write(real.join("tool"), "#!/bin/sh\n").expect("real file");
    fs::set_permissions(real.join("tool"), fs::Permissions::from_mode(0o755)).expect("real mode");
    let process = HostProcessContext {
        fallback_path: format!("{}:{}", shadow.display(), real.display()),
        home: None,
        prefix: None,
        discovery_ceiling: None,
    };
    assert_eq!(
        resolve_host_process_program("tool", &process, &[]).expect("resolved"),
        real.join("tool")
    );
    let only_shadow = HostProcessContext {
        fallback_path: shadow.display().to_string(),
        home: None,
        prefix: None,
        discovery_ceiling: None,
    };
    assert!(resolve_host_process_program("tool", &only_shadow, &[]).is_err());
}

#[test]
fn program_lookup_never_consults_relative_path_entries() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("tool"), "#!/bin/sh\n").expect("tool");
    fs::set_permissions(root.path().join("tool"), fs::Permissions::from_mode(0o755)).expect("mode");
    let cwd = std::env::current_dir().expect("cwd");
    let levels = cwd
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count();
    let relative = PathBuf::from("../".repeat(levels))
        .join(root.path().strip_prefix("/").expect("absolute tempdir"));
    assert!(relative.join("tool").is_file());
    let process = HostProcessContext {
        fallback_path: format!(".::{}", relative.display()),
        home: None,
        prefix: None,
        discovery_ceiling: None,
    };
    assert!(resolve_host_process_program("tool", &process, &[]).is_err());
    assert!(resolve_host_process_program("./Cargo.toml", &process, &[]).is_err());
    assert_eq!(
        resolve_host_process_program("/bin/sh", &process, &[]).expect("absolute"),
        Path::new("/bin/sh")
    );
}
