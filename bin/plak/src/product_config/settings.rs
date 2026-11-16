//! Product account remedy fixtures and host settings-default checks.

pub(crate) fn oauth_accounts() -> impl Iterator<Item = &'static str> {
    crate::capabilities::ROUTES
        .iter()
        .filter(|capability| {
            capability.route.compiled
                && capability.auth_kind() == plakous_host_protocol::views::AuthKind::Oauth
        })
        .map(|capability| capability.route.account)
}

/// The canonical command that connects one OAuth subscription account.
#[must_use]
pub fn oauth_login_remedy(account: &str) -> String {
    format!("plak login {account}")
}

/// The canonical command that stores one provider API key.
///
/// API-key accounts never take `plak login`: the two auth routes do not cross,
/// and pointing an `openrouter` operator at an OAuth verb sends them to a
/// refusal.
#[must_use]
pub fn api_key_remedy(account: &str) -> String {
    format!("plak key set {account}")
}

/// Every concrete remedy command any product surface can print.
///
/// The CLI-facing regression test parses each of these with the real argument
/// parser, so a remedy that names a command `plak` does not have fails the
/// build rather than reaching an operator.
#[must_use]
#[cfg(test)]
pub fn remedy_catalog() -> Vec<String> {
    let mut catalog = Vec::new();
    for account in crate::capabilities::OAUTH_ACCOUNTS {
        catalog.push(oauth_login_remedy(account));
        catalog.push(format!("plak logout {account}"));
    }
    for account in crate::capabilities::API_KEY_ACCOUNTS {
        catalog.push(api_key_remedy(account));
        catalog.push(format!("plak key clear {account}"));
    }
    catalog
}

#[test]
fn settings_registry_defaults_match_the_config_schema() {
    let config = super::ProductConfig::default_product();
    let snapshot = crate::shell::settings::settings_view(&config);
    let key = super::config_key("compaction.enabled").expect("product setting");
    assert_eq!(key.default_value(), "true");
    assert!(snapshot.product.compaction);
    let mut changed = config;
    changed
        .set_key("compaction.enabled", "false")
        .expect("valid setting");
    assert!(
        !crate::shell::settings::settings_view(&changed)
            .product
            .compaction
    );
}
