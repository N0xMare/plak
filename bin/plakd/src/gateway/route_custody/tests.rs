use super::*;
use crate::shell::services::Services;

fn home(directory: &tempfile::TempDir) -> PlakPaths {
    let paths = PlakPaths::explicit(directory.path().join("home"));
    std::fs::create_dir_all(paths.home()).unwrap();
    paths
}

#[test]
fn an_unset_address_refuses_with_the_install_remedy() {
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    let error = HostCustody::resolve(&paths).unwrap_err();
    assert_eq!(error.code(), Some("gateway_unavailable"));
    assert!(error.message().contains("plakd gateway install"));
}

#[test]
fn a_configured_address_is_an_admin_link_without_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"unix:/tmp/plak-gateway-test.sock\"\n",
    )
    .unwrap();
    let custody = HostCustody::resolve(&paths).unwrap();
    let link = custody.admin().unwrap();
    assert!(
        matches!(&link.address, GatewayAddress::Unix(path) if path.ends_with("plak-gateway-test.sock"))
    );
    assert!(link.key().is_none(), "a unix peer sends no key by default");
}

#[test]
fn a_config_that_does_not_parse_is_an_error_not_local_custody() {
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    std::fs::write(paths.config_file(), "[gateway\naddress = \"unix:/x\"\n").unwrap();
    assert!(HostCustody::resolve(&paths).is_err());
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"relative.sock\"\n",
    )
    .unwrap();
    assert!(HostCustody::resolve(&paths).is_err());
}

#[test]
fn a_unix_key_is_sent_only_from_an_explicit_owner_only_key_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    // The default key file is never sent to a unix gateway.
    super::super::io_file::write_private(
        &paths.secrets_dir().join("gateway.key"),
        b"plakgw1.host.default",
    )
    .unwrap();
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"unix:/tmp/plak-gateway-test.sock\"\n",
    )
    .unwrap();
    let custody = HostCustody::resolve(&paths).unwrap();
    assert!(custody.admin().unwrap().key().is_none());

    let key = directory.path().join("caller.key");
    std::fs::write(&key, "plakgw1.host.explicit\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[gateway]\naddress = \"unix:/tmp/plak-gateway-test.sock\"\nkey_file = \"{}\"\n",
            key.display()
        ),
    )
    .unwrap();
    let refused = HostCustody::resolve(&paths).err().unwrap();
    assert!(refused.message().contains("0600"), "{refused}");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let custody = HostCustody::resolve(&paths).unwrap();
    assert_eq!(
        custody.admin().unwrap().key(),
        Some("plakgw1.host.explicit")
    );
}

#[test]
fn a_tcp_gateway_needs_a_key() {
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"tcp:127.0.0.1:9\"\n",
    )
    .unwrap();
    assert!(HostCustody::resolve(&paths).is_err());
}

/// A host keeps the custody it started with: a config that later breaks or
/// loses `[gateway]` neither flips it to local custody nor makes it open the
/// store or read `secrets/`.
#[test]
fn a_latched_gateway_custody_survives_a_config_that_breaks_or_drops_the_gateway() {
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    let state = directory.path().join("host-state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        paths.config_file(),
        "[gateway]\naddress = \"unix:/tmp/plak-latched-gateway.sock\"\n[runtime_store]\nmax_sessions = 41\nretire_finished_children = true\nretire_requires_archive_receipt = true\nchild_retention_hours = 12\n",
    )
    .unwrap();
    let services = Services::for_home_with_state(paths.clone(), state.clone());
    let started = services.custody().unwrap();
    assert!(started.address().is_some());
    assert_eq!(
        services.child_retirement_policy().unwrap(),
        (true, true, Some(std::time::Duration::from_hours(12)))
    );
    for changed in [
        "[gateway\naddress = 1\n",
        "",
        "[web]\nconfig_read = false\n",
    ] {
        std::fs::write(paths.config_file(), changed).unwrap();
        let custody = services.clone().custody().unwrap();
        assert_eq!(custody.label(), started.label(), "after {changed:?}");
        let _roots = services.state_roots().unwrap();
        assert_eq!(services.runtime_store_bounds().unwrap()[3].get(), 41);
        assert_eq!(
            services.child_retirement_policy().unwrap(),
            (true, true, Some(std::time::Duration::from_hours(12)))
        );
        // No key file exists: a local read would refuse this account.
        {
            use plakous_hosted::platform::OpenAiCredentials as _;
            let platform = crate::platform_product::ProductPlatformCredentials::shared(
                &paths,
                custody.clone(),
            );
            assert!(platform.openai(&"openai".parse().unwrap()).is_ok());
        }
        assert!(
            !state.join("plak/subscriptions").exists(),
            "a gateway host must not create its subscription store"
        );
        assert!(!paths.secrets_dir().exists());
    }
    std::fs::write(paths.config_file(), "[gateway\n").unwrap();
    let fresh = Services::for_home_with_state(paths.clone(), state.clone());
    assert!(
        fresh.custody().is_err(),
        "a fresh process must refuse, not go local"
    );
    assert!(!state.join("plak/subscriptions").exists());
}

#[test]
fn a_unix_gateway_must_advertise_this_homes_store_at_the_configured_socket() {
    let directory = tempfile::tempdir().unwrap();
    let paths = home(&directory);
    let configured = directory.path().join("configured.sock");
    std::fs::write(
        paths.config_file(),
        format!("[gateway]\naddress = \"unix:{}\"\n", configured.display()),
    )
    .unwrap();
    let custody = HostCustody::resolve(&paths).unwrap();
    let host_state = directory.path().join("host-state");
    let other_state = directory.path().join("other-state");
    // Nothing holds the host's store.
    let address = custody.address().unwrap();
    assert!(!verify_holder(address, &host_state).unwrap());
    assert!(!host_state.join("plak/subscriptions").exists());
    // A gateway holds another state root at the configured socket.
    let other = super::super::holder::lock_path(&other_state);
    let held = super::super::holder::HolderLock::try_acquire(
        &other,
        super::super::holder::HolderMode::Exclusive,
    )
    .unwrap();
    held.advertise(&configured).unwrap();
    assert!(!verify_holder(address, &host_state).unwrap());
    // A gateway holds the host's store but serves another socket.
    let mine = super::super::holder::lock_path(&host_state);
    let elsewhere = super::super::holder::HolderLock::try_acquire(
        &mine,
        super::super::holder::HolderMode::Exclusive,
    )
    .unwrap();
    assert!(!verify_holder(address, &host_state).unwrap());
    elsewhere
        .advertise(&directory.path().join("elsewhere.sock"))
        .unwrap();
    let refused = verify_holder(address, &host_state).unwrap_err();
    assert!(refused.contains("not the configured"), "{refused}");
    elsewhere.advertise(&configured).unwrap();
    assert!(verify_holder(address, &host_state).unwrap());
    drop(held);
}
