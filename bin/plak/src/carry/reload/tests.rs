use super::*;
use plakous::agent::BindingChange;
use plakous::subscriptions::NativeAuthStatus;

#[test]
fn the_probe_answers_unknown_rather_than_renew_a_login() {
    let fresh = || Some(false);
    let expiring = || Some(true);
    assert_eq!(
        credential_verdict("claude", Some(NativeAuthStatus::Ready), fresh),
        None
    );
    let needs_refresh = Some(ReopenProbe::Unknown {
        reason: "needs_refresh".to_owned(),
    });
    assert_eq!(
        credential_verdict("claude", Some(NativeAuthStatus::Ready), expiring),
        needs_refresh
    );
    for status in [
        Some(NativeAuthStatus::RefreshRequired),
        Some(NativeAuthStatus::Refreshing),
        None,
    ] {
        assert_eq!(
            credential_verdict("codex", status, || panic!("no expiry read")),
            needs_refresh
        );
    }
    let Some(ReopenProbe::Refused { reason }) =
        credential_verdict("codex", Some(NativeAuthStatus::Quarantined), fresh)
    else {
        panic!("a quarantined login is refused");
    };
    assert_eq!(
        reason,
        "the codex login is quarantined; run `plak login codex`, then /reload"
    );
    assert!(matches!(
        credential_verdict("grok", Some(NativeAuthStatus::SignedOut), fresh),
        Some(ReopenProbe::Refused { .. })
    ));
    assert_eq!(
        credential_verdict("claude", Some(NativeAuthStatus::LoginRequired), fresh),
        Some(ReopenProbe::Refused {
            reason: "the claude login is stored by an earlier build; run `plak login claude`, then /reload"
                .to_owned()
        })
    );
    let Some(ReopenProbe::Refused { reason }) =
        credential_verdict("codex", Some(NativeAuthStatus::WrittenByNewerBuild), fresh)
    else {
        panic!("a newer build's login is refused");
    };
    assert!(reason.contains("written by a newer build"), "{reason}");
    assert_eq!(
        serde_json::to_string(&ReopenProbe::Unknown {
            reason: "needs_refresh".to_owned()
        })
        .unwrap(),
        r#"{"verdict":"unknown","reason":"needs_refresh"}"#
    );
}

#[test]
fn a_digest_only_capability_change_names_its_category_and_this_builds_revision() {
    assert_eq!(
        binding_change_words(
            &BindingChange::ModelCapabilities,
            Some("anthropic.claude.oauth.2026-10-30.v9")
        ),
        "model capability binding (adapter revision, declared features or context contract; this build: anthropic.claude.oauth.2026-10-30.v9)"
    );
    assert_eq!(
        binding_change_words(&BindingChange::ModelCapabilities, None),
        BindingChange::ModelCapabilities.to_string()
    );
    let profile = BindingChange::AdapterProfile {
        recorded: "a".to_owned(),
        current: "b".to_owned(),
    };
    assert_eq!(
        binding_change_words(&profile, Some("ignored")),
        profile.to_string()
    );
}
