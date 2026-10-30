use super::{
    API_KEY_ACCOUNTS, AuthKind, OAUTH_ACCOUNTS, ProviderDef, ProviderRowPresentation as _,
    ProviderState, StatusTone, account_remedy, remedy_catalog,
};

fn def(id: &str, kind: AuthKind, state: ProviderState, detail: &str) -> ProviderDef {
    ProviderDef {
        id: id.to_owned(),
        label: id.to_owned(),
        kind,
        state,
        detail: detail.to_owned(),
        account_label: None,
    }
}

/// Spec 02 §5.5: the status wording is a closed set of three, and the tone
/// follows the word so a colour can never contradict it.
#[test]
fn providers_render_last_failed_refresh_instead_of_expired() {
    let message = "refresh failed at 05:48: network unreachable (DNS lookup timed out); credential unchanged; retry when online";
    let row = def("claude", AuthKind::Oauth, ProviderState::Expired, message);
    assert_eq!(row.status_label(), message);
    assert_eq!(row.status_description(), message);
    assert_eq!(row.status_tone(), StatusTone::Blocked);
    let ready = def("claude", AuthKind::Oauth, ProviderState::Ready, message);
    assert_eq!(ready.status_label(), format!("ready; {message}"));
    assert_eq!(ready.status_tone(), StatusTone::Ready);
}

#[test]
fn every_state_maps_to_its_exact_status_word_and_tone() {
    let ready = def("codex", AuthKind::Oauth, ProviderState::Ready, "");
    assert_eq!(ready.status_label(), "ready");
    assert_eq!(ready.status_tone(), StatusTone::Ready);

    for absent in [
        def("grok", AuthKind::Oauth, ProviderState::NotLoggedIn, ""),
        def("openrouter", AuthKind::ApiKey, ProviderState::NoKey, ""),
    ] {
        assert_eq!(absent.status_label(), "login required");
        assert_eq!(absent.status_tone(), StatusTone::LoginRequired);
    }

    // The nuance behind an expiry never leaks into the status word.
    for detail in ["", "revoked", "refresh required"] {
        let expired = def("claude", AuthKind::Oauth, ProviderState::Expired, detail);
        assert_eq!(expired.status_label(), "re-login required (expired)");
        assert_eq!(expired.status_tone(), StatusTone::Blocked);
    }

    let unknown = def(
        "grok",
        AuthKind::Oauth,
        ProviderState::RefreshUnknown,
        "quarantined",
    );
    assert_eq!(unknown.status_label(), "refresh outcome unknown");
    assert_eq!(unknown.status_tone(), StatusTone::Blocked);

    // An unreadable store is neither ready nor fixable by logging in.
    let unknown = def(
        "codex",
        AuthKind::Oauth,
        ProviderState::Unavailable,
        "credential store unreadable",
    );
    assert_eq!(
        unknown.status_label(),
        "unavailable (credential store unreadable)"
    );
    assert_eq!(unknown.status_tone(), StatusTone::Blocked);
}

/// Spec 02 §5.5: every vertical prints its CLI direction, ready or not, and
/// the direction follows the auth route rather than the state.
#[test]
fn every_vertical_names_its_exact_cli_command() {
    for state in [
        ProviderState::Ready,
        ProviderState::NotLoggedIn,
        ProviderState::Expired,
        ProviderState::RefreshUnknown,
        ProviderState::Unavailable,
    ] {
        let grok = def("grok", AuthKind::Oauth, state, "");
        assert_eq!(grok.cli_direction(), "plak login grok");
    }
    let openrouter = def("openrouter", AuthKind::ApiKey, ProviderState::NoKey, "");
    assert_eq!(openrouter.cli_direction(), "plak key set openrouter");
    assert!(remedy_catalog().contains(&openrouter.cli_direction()));
}

/// Spec 02 §5.5: `refresh outcome unknown` is explained in prose, since the
/// status word deliberately does not carry quarantine nuance.
#[test]
fn a_quarantined_credential_is_explained_rather_than_merely_labelled() {
    let quarantined = def(
        "grok",
        AuthKind::Oauth,
        ProviderState::RefreshUnknown,
        "quarantined",
    );
    let description = quarantined.status_description();
    assert!(description.contains("unknown"), "{description}");
    assert!(description.contains("retry automatically"), "{description}");
    assert!(
        !description.contains("plak login grok"),
        "grok retries without re-login: {description}"
    );

    let codex = def(
        "codex",
        AuthKind::Oauth,
        ProviderState::RefreshUnknown,
        "quarantined",
    );
    let codex_description = codex.status_description();
    assert!(
        codex_description.contains("plak login codex"),
        "{codex_description}"
    );

    let plain = def("grok", AuthKind::Oauth, ProviderState::Expired, "");
    assert!(
        !plain.status_description().contains("quarantined"),
        "a plain expiry is not described as a quarantine"
    );
}

#[test]
fn an_api_key_account_is_never_given_an_oauth_remedy() {
    for account in API_KEY_ACCOUNTS {
        assert_eq!(account_remedy(account), format!("plak key set {account}"));
    }
    for account in OAUTH_ACCOUNTS {
        assert_eq!(account_remedy(account), format!("plak login {account}"));
    }
    // An account outside both closed sets falls to the key route rather
    // than manufacturing an OAuth login the CLI would refuse.
    assert_eq!(account_remedy("mystery"), "plak key set mystery");
}

#[test]
fn the_catalog_covers_both_auth_routes_and_names_no_legacy_alias() {
    let catalog = remedy_catalog();
    assert!(catalog.contains(&"plak login codex".to_owned()));
    assert!(catalog.contains(&"plak logout claude".to_owned()));
    assert!(catalog.contains(&"plak key set openrouter".to_owned()));
    assert!(catalog.contains(&"plak key clear anthropic".to_owned()));
    assert!(
        catalog.iter().all(|command| !command.contains("plak auth")),
        "no product remedy names the legacy alias: {catalog:?}"
    );
}
