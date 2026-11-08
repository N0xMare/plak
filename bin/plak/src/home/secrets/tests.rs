use super::*;
use crate::home::paths::PlakPaths;
use proptest::prelude::*;
use std::ffi::OsStr;
use tempfile::tempdir;

#[test]
fn missing_key_remedies_target_the_process_that_resolves_them() {
    use crate::environment::KeyResolver;
    let auto = missing_api_key_message_for("openai", KeyResolver::AutoHost);
    assert!(auto.contains("run `plak key set openai`"));
    assert!(auto.contains("caller variable OPENAI_API_KEY is ignored"));
    assert!(!auto.contains("or set OPENAI_API_KEY"));
    let host = missing_api_key_message_for("openai", KeyResolver::Host);
    assert!(host.contains("set OPENAI_API_KEY in the host's or unit's environment"));
    let cli = missing_api_key_message_for("openai", KeyResolver::Cli);
    assert!(cli.contains("or set OPENAI_API_KEY; effective key resolver: this CLI process"));
}

#[test]
fn named_api_keys_are_disjoint_from_defaults_siblings_and_generic_secrets() {
    let directory = tempdir().unwrap();
    let paths = PlakPaths::explicit(directory.path().join("home"));
    set_named_secret(&paths, "work", "generic-secret").unwrap();
    for vertical in ["openai", "anthropic", "xai-api", "openrouter"] {
        let account = format!("{vertical}/work");
        let sibling = format!("{vertical}/personal");
        assert_eq!(env_var_for_account(&account), None);
        assert!(resolve_api_key(&paths, &account).unwrap().is_none());
        set_api_key(&paths, &account, &format!("{vertical}-work-key")).unwrap();
        set_api_key(&paths, &sibling, &format!("{vertical}-personal-key")).unwrap();
        assert_eq!(
            resolve_api_key(&paths, &account).unwrap().unwrap(),
            format!("{vertical}-work-key")
        );
        assert_eq!(
            resolve_api_key(&paths, &sibling).unwrap().unwrap(),
            format!("{vertical}-personal-key")
        );
        assert_eq!(
            key_status(&paths, &account).unwrap().source,
            KeySource::File
        );
        assert!(clear_api_key(&paths, &account).unwrap());
        assert!(resolve_api_key(&paths, &account).unwrap().is_none());
        assert_eq!(
            resolve_api_key(&paths, &sibling).unwrap().unwrap(),
            format!("{vertical}-personal-key")
        );
    }
    assert_eq!(
        read_named_secret(&paths, "work")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "generic-secret"
    );
}

fn unshadowed_api_key_account() -> Option<&'static str> {
    ["openai", "xai-api", "anthropic", "openrouter"]
        .into_iter()
        .find(|account| {
            env_var_for_account(account).is_some_and(|name| std::env::var_os(name).is_none())
        })
}

#[test]
fn file_round_trip_and_fingerprint() {
    let dir = tempdir().unwrap();
    let paths =
        PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
    paths.ensure_layout().unwrap();
    let Some(account) = unshadowed_api_key_account() else {
        let error = set_api_key(&paths, "openai", "sk-test-secret-key-9999").unwrap_err();
        assert!(error.contains("OPENAI_API_KEY"));
        assert!(error.contains("still wins over the file store"));
        return;
    };
    set_api_key(&paths, account, "sk-test-secret-key-9999").unwrap();
    let got = resolve_api_key(&paths, account).unwrap().unwrap();
    assert_eq!(got, "sk-test-secret-key-9999");
    let status = key_status(&paths, account).unwrap();
    assert_eq!(status.source, KeySource::File);
    let fingerprint = status.fingerprint.unwrap();
    assert!(fingerprint.starts_with("sha256:"));
    assert_eq!(fingerprint.len(), "sha256:".len() + 8);
    assert!(!fingerprint.contains("9999"));
    assert!(clear_api_key(&paths, account).unwrap());
}

#[test]
fn xai_api_product_account_shares_only_the_xai_key_store() {
    let dir = tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().to_owned());
    paths.ensure_layout().unwrap();
    set_api_key(&paths, "xai-api", "offline-fixture-key").unwrap();
    assert!(paths.api_key_file("xai").is_file());
    assert!(!paths.api_key_file("xai-api").exists());
    assert!(clear_api_key(&paths, "xai-api").unwrap());
    assert!(!clear_api_key(&paths, "xai-api").unwrap());
    assert_eq!(env_var_for_account("xai-api"), Some("XAI_API_KEY"));
    assert_eq!(env_var_for_account("grok"), None);
}

#[test]
fn rejects_oauth_account_names() {
    let dir = tempdir().unwrap();
    let paths =
        PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
    assert!(set_api_key(&paths, "grok", "x").is_err());
    assert!(set_api_key(&paths, "codex", "x").is_err());
}

#[test]
fn direct_key_file_enforces_raw_size_limit() {
    let Some(account) = unshadowed_api_key_account() else {
        return;
    };
    let dir = tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().to_owned());
    paths.ensure_layout().unwrap();
    let path = paths.api_key_file(account);

    fs::write(&path, vec![b'x'; MAX_API_KEY_BYTES]).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(
        resolve_api_key(&paths, account).unwrap().unwrap().len(),
        MAX_API_KEY_BYTES
    );
    assert!(file_fingerprint_if_present(&paths, account).is_some());

    fs::write(&path, vec![b'x'; MAX_API_KEY_BYTES + 1]).unwrap();
    assert!(
        resolve_api_key(&paths, account)
            .unwrap_err()
            .contains("too large")
    );
    assert_eq!(file_fingerprint_if_present(&paths, account), None);
}

#[test]
fn secret_names_follow_the_template_name_grammar() {
    for good in ["a", "github", "linear-ws", "a1-b2", &"a".repeat(32)] {
        assert_eq!(validate_secret_name(good), Ok(()), "{good}");
    }
    for bad in [
        "",
        "1a",
        "-a",
        "A",
        "a_b",
        "a.b",
        "a/b",
        "../x",
        &"a".repeat(33),
    ] {
        assert!(validate_secret_name(bad).is_err(), "{bad}");
    }
    assert_eq!(
        validate_secret_name("Bad").unwrap_err(),
        "secret name \"Bad\" must match [a-z][a-z0-9-]{0,31}"
    );
}

#[test]
fn named_secrets_are_private_files_listed_by_name_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().join("home"));
    assert_eq!(list_named_secrets(&paths).unwrap(), Vec::<String>::new());
    assert!(read_named_secret(&paths, "github").unwrap().is_none());
    assert!(!remove_named_secret(&paths, "github").unwrap());

    set_named_secret(&paths, "github", "  ghp-offline-fixture\n").unwrap();
    set_named_secret(&paths, "linear", "lin-offline-fixture").unwrap();
    let file = paths.named_secret_file("github");
    assert_eq!(file, paths.secrets_dir().join("named/github.secret"));
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&paths.named_secrets_dir()), 0o700);
    assert_eq!(
        read_named_secret(&paths, "github")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "ghp-offline-fixture"
    );
    let listed = list_named_secrets(&paths).unwrap();
    assert_eq!(listed, ["github", "linear"]);
    let debug = format!("{:?}", read_named_secret(&paths, "github").unwrap());
    assert!(!debug.contains("ghp-offline-fixture"), "{debug}");

    // A replacement overwrites; no staging file survives.
    set_named_secret(&paths, "github", "ghp-second").unwrap();
    assert_eq!(
        read_named_secret(&paths, "github")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "ghp-second"
    );
    assert_eq!(fs::read_dir(paths.named_secrets_dir()).unwrap().count(), 2);

    // A file other users can read is refused, not used.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let refused = read_named_secret(&paths, "github").unwrap_err();
    assert!(
        refused.contains("is not a private single-link file"),
        "{refused}"
    );
    assert!(refused.contains("chmod 600"), "{refused}");
    // So is a permissive named directory.
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(paths.named_secrets_dir(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        read_named_secret(&paths, "github")
            .unwrap_err()
            .contains("chmod 700")
    );
    assert!(list_named_secrets(&paths).is_err());
    assert!(set_named_secret(&paths, "github", "x").is_err());
    fs::set_permissions(paths.named_secrets_dir(), fs::Permissions::from_mode(0o700)).unwrap();

    // A hard link could alias the secret elsewhere: refused too.
    let alias = dir.path().join("alias");
    fs::hard_link(&file, &alias).unwrap();
    assert!(
        read_named_secret(&paths, "github")
            .unwrap_err()
            .contains("single-link")
    );
    fs::remove_file(&alias).unwrap();

    assert!(remove_named_secret(&paths, "github").unwrap());
    assert!(!remove_named_secret(&paths, "github").unwrap());
    assert_eq!(list_named_secrets(&paths).unwrap(), ["linear"]);

    for (name, value) in [("Bad", "v"), ("ok", ""), ("ok", "a\u{7}b")] {
        assert!(set_named_secret(&paths, name, value).is_err(), "{name}");
    }
    assert!(set_named_secret(&paths, "big", &"x".repeat(MAX_NAMED_SECRET_BYTES + 1)).is_err());
}

#[test]
fn all_secret_shelves_accept_owner_read_only_and_refuse_shared_or_unreadable_modes() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    for shelf in [
        SecretShelf::Named,
        SecretShelf::Runner,
        SecretShelf::Web,
        SecretShelf::Api(Vertical::OpenAi),
    ] {
        set_shelf_secret(&paths, shelf, "work", "synthetic-secret").unwrap();
        let path = paths.secret_shelf_dir(shelf).join("work.secret");
        for mode in [0o400, 0o600] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read_shelf_secret(&paths, shelf, "work")
                    .unwrap()
                    .unwrap()
                    .expose_secret(),
                "synthetic-secret"
            );
        }
        for mode in [0o000, 0o200, 0o440, 0o604, 0o644] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                read_shelf_secret(&paths, shelf, "work").is_err(),
                "{shelf:?}: {mode:o}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn api_key_file_round_trip_prop(suffix in "[a-zA-Z0-9]{8,32}") {
        let dir = tempdir().unwrap();
        let paths = PlakPaths::resolve(
            Some(OsStr::new(dir.path())),
            Some(OsStr::new("/home/u")),
        )
        .unwrap();
        paths.ensure_layout().unwrap();
        let key = format!("sk-test-{suffix}");
        if let Some(account) = unshadowed_api_key_account() {
            set_api_key(&paths, account, &key).unwrap();
            let got = resolve_api_key(&paths, account).unwrap().unwrap();
            prop_assert_eq!(got, key.clone());
            let status = key_status(&paths, account).unwrap();
            prop_assert_eq!(status.source, KeySource::File);
            let fp = status.fingerprint.unwrap();
            prop_assert!(!fp.contains(&key));
            prop_assert!(clear_api_key(&paths, account).unwrap());
            prop_assert!(resolve_api_key(&paths, account).unwrap().is_none());
        } else {
            let error = set_api_key(&paths, "openai", &key).unwrap_err();
            prop_assert!(error.contains("OPENAI_API_KEY"));
        }
    }
}

#[test]
fn runner_secrets_are_a_shelf_apart_from_named_secrets() {
    use plakous_hosted::mcp_binding::NamedSecrets as _;
    let dir = tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().join("home"));
    set_shelf_secret(&paths, SecretShelf::Runner, "github", "runner-value").unwrap();
    assert_eq!(
        paths.secret_shelf_file(SecretShelf::Runner, "github"),
        paths.secrets_dir().join("runner/github.secret")
    );
    assert_eq!(list_named_secrets(&paths).unwrap(), Vec::<String>::new());
    assert_eq!(
        list_shelf_secrets(&paths, SecretShelf::Runner).unwrap(),
        ["github"]
    );
    // Hosting reads each shelf by its own port method only.
    let product = ProductNamedSecrets::shared(&paths);
    assert!(product.read("github").unwrap().is_none());
    assert!(product.read_runner("github").unwrap().is_some());
    set_named_secret(&paths, "github", "host-value").unwrap();
    assert_eq!(
        read_shelf_secret(&paths, SecretShelf::Runner, "github")
            .unwrap()
            .unwrap()
            .expose_secret(),
        "runner-value"
    );
    assert!(remove_shelf_secret(&paths, SecretShelf::Runner, "github").unwrap());
    assert!(product.read_runner("github").unwrap().is_none());
    assert!(product.read("github").unwrap().is_some());
}
