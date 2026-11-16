use super::*;
use plakous::agent::BindingChange;
use plakous::subscriptions::NativeAuthStatus;

#[test]
fn host_reload_probe_round_trips_without_terminal_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let local = reload_probe(&PlakPaths::explicit(root.path().join("home")));
    let wire = plakd_wire_types::ReloadProbe::from(local.clone());
    assert!(
        serde_json::to_value(&wire)
            .unwrap()
            .get("host_resume")
            .is_none()
    );
    let restored = ReloadProbe::from(wire);
    assert_eq!(restored.state_key, local.state_key);
    assert_eq!(restored.binary_id, local.binary_id);
}

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
        "the codex login is quarantined; run `plakd login codex`, then retry the host upgrade"
    );
    assert!(matches!(
        credential_verdict("grok", Some(NativeAuthStatus::SignedOut), fresh),
        Some(ReopenProbe::Refused { .. })
    ));
    assert_eq!(
        credential_verdict("claude", Some(NativeAuthStatus::LoginRequired), fresh),
        Some(ReopenProbe::Refused {
            reason: "the claude login is stored by an earlier build; run `plakd login claude`, then retry the host upgrade"
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
            Some("anthropic.claude.oauth.2026-11-15.v10")
        ),
        "model capability binding (adapter revision, declared features or context contract; this build: anthropic.claude.oauth.2026-11-15.v10)"
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
