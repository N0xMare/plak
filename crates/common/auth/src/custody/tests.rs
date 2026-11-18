use std::sync::{Arc, Mutex};

use super::*;
use crate::api_key::{ApiKey, CredentialProviderTag};

#[test]
fn fields_carry_exact_bytes_and_never_print() {
    let field = CredentialField::bearer(b"synthetic-token");
    assert_eq!(format!("{field:?}"), "CredentialField(<redacted>)");
    assert_eq!(field.into_header_bytes(), b"Bearer synthetic-token");
    assert_eq!(
        CredentialField::exact(b"raw".to_vec()).into_header_bytes(),
        b"raw"
    );
}

#[test]
fn slot_grammar_matches_account_references() {
    for valid in ["codex", "codex/work", "openai-api/a1"] {
        let slot = CustodySlot::new(valid).unwrap();
        assert_eq!(slot.as_str(), valid);
    }
    assert_eq!(CustodySlot::new("codex/work").unwrap().kind(), "codex");
    assert_eq!(CustodySlot::new("codex/work").unwrap().name(), Some("work"));
    for invalid in ["", "Codex", "codex/default", "codex/", "/x", "a/b/c", "1x"] {
        assert_eq!(
            CustodySlot::new(invalid),
            Err(CustodyError::UnknownAccount),
            "{invalid}"
        );
    }
}

#[test]
fn digest_hex_round_trips() {
    let digest = IdentityDigest::derive(b"domain", &[b"a", b"b"]);
    assert_eq!(IdentityDigest::from_hex(&digest.to_hex()), Some(digest));
    assert_ne!(digest, IdentityDigest::derive(b"domain", &[b"ab"]));
    assert_eq!(IdentityDigest::from_hex("AB"), None);
    assert_eq!(IdentityDigest::from_hex(&"G".repeat(64)), None);
}

struct Keys(Mutex<Option<&'static str>>);

impl LocalKeyStore for Keys {
    fn key(&self, _slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError> {
        Ok(self.0.lock().unwrap().map(|key| LocalKey {
            key: ApiKey::new(key).unwrap(),
            source: KeySource::File,
            env_var: None,
        }))
    }
}

fn injection(kind: &str) -> Option<KeyInjection> {
    match kind {
        "bearer-kind" => Some(KeyInjection {
            provider: CredentialProviderTag::OpenAiPlatform,
            header: "authorization",
            form: KeyHeaderForm::Bearer,
        }),
        "raw-kind" => Some(KeyInjection {
            provider: CredentialProviderTag::AnthropicPlatform,
            header: "x-api-key",
            form: KeyHeaderForm::Raw,
        }),
        _ => None,
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("local key custody never waits"),
    }
}

#[test]
fn local_key_custody_supplies_fields_only_at_its_fence() {
    let keys = Arc::new(Keys(Mutex::new(Some("synthetic-key-one"))));
    let custodian = LocalKeyCustodian::new(keys.clone(), injection);
    let slot = CustodySlot::new("bearer-kind/work").unwrap();
    let facts = block_on(custodian.facts(&slot)).unwrap();
    assert_eq!(facts.status, CustodyStatus::Ready);
    let fence = facts.expected_fence().unwrap();
    let fingerprint = ApiKey::new("synthetic-key-one")
        .unwrap()
        .generation_grant(CredentialProviderTag::OpenAiPlatform, ())
        .fingerprint();
    assert_eq!(
        facts.key.as_ref().unwrap().fingerprint.as_deref(),
        Some(fingerprint.to_hex().as_str())
    );

    let fields = block_on(custodian.fields(&slot, &fence)).unwrap();
    let fields: Vec<_> = fields
        .into_iter()
        .map(|(name, field)| (name, field.into_header_bytes()))
        .collect();
    assert_eq!(
        fields,
        vec![("authorization", b"Bearer synthetic-key-one".to_vec())]
    );

    *keys.0.lock().unwrap() = Some("synthetic-key-two");
    let moved = block_on(custodian.fields(&slot, &fence)).unwrap_err();
    let CustodyError::FenceMoved { facts: current } = moved else {
        panic!("a replaced key moves the fence: {moved:?}");
    };
    assert_eq!(
        current.account_binding,
        Some(fence.account_binding),
        "a replaced key keeps the account"
    );

    let other = CustodySlot::new("bearer-kind/other").unwrap();
    let other_fence = block_on(custodian.facts(&other))
        .unwrap()
        .expected_fence()
        .unwrap();
    assert_ne!(other_fence.account_binding, fence.account_binding);
    assert_eq!(
        block_on(custodian.fields(&slot, &other_fence)).unwrap_err(),
        CustodyError::AccountChanged
    );

    *keys.0.lock().unwrap() = None;
    let signed_out = block_on(custodian.facts(&slot)).unwrap();
    assert_eq!(signed_out.status, CustodyStatus::SignedOut);
    assert!(matches!(
        block_on(custodian.fields(&slot, &fence)),
        Err(CustodyError::NotReady { .. })
    ));
}

#[test]
fn local_key_custody_lowers_raw_keys_and_refuses_unknown_kinds() {
    let keys = Arc::new(Keys(Mutex::new(Some("synthetic-raw"))));
    let custodian = LocalKeyCustodian::new(keys, injection);
    let slot = CustodySlot::new("raw-kind").unwrap();
    let fence = block_on(custodian.refresh(&slot, RefreshRequest::Operator))
        .unwrap()
        .expected_fence()
        .unwrap();
    let mut fields = block_on(custodian.fields(&slot, &fence)).unwrap();
    assert_eq!(fields.names().collect::<Vec<_>>(), ["x-api-key"]);
    assert_eq!(
        fields.take("x-api-key").unwrap().into_header_bytes(),
        b"synthetic-raw"
    );
    assert_eq!(
        block_on(custodian.facts(&CustodySlot::new("unknown").unwrap())),
        Err(CustodyError::UnknownAccount)
    );
}

#[test]
fn facts_debug_names_wire_identity_keys_but_not_values() {
    let mut facts = AccountFacts::signed_out(CustodySlot::new("claude").unwrap());
    facts
        .wire_identity
        .insert("claude_account_uuid".into(), "sentinel-account-uuid".into());
    let printed = format!("{facts:?}");
    assert!(printed.contains("claude_account_uuid"), "{printed}");
    assert!(!printed.contains("sentinel-account-uuid"), "{printed}");
}

#[cfg(feature = "gateway")]
#[test]
fn wire_facts_refuse_identity_that_does_not_fit_instead_of_dropping_it() {
    let instance =
        plakous_gateway_protocol::UuidText::new("0b9e5f4c-7d3a-4e21-9f80-1a2b3c4d5e6f").unwrap();
    let mut facts = AccountFacts::signed_out(CustodySlot::new("grok").unwrap());
    facts
        .wire_identity
        .insert("grok_subject".into(), "fixture-subject".into());
    let wire = gateway::wire_facts(&facts, &instance, 1).unwrap();
    assert_eq!(
        wire.wire_identity.get("grok_subject"),
        Some("fixture-subject")
    );

    let mut oversize = facts.clone();
    oversize.wire_identity.insert(
        "grok_authority_id".into(),
        "x".repeat(plakous_gateway_protocol::facts::MAX_WIRE_IDENTITY_VALUE_BYTES + 1),
    );
    assert_eq!(
        gateway::wire_facts(&oversize, &instance, 1).err(),
        Some(CustodyError::Internal)
    );
    let mut crowded = facts;
    for index in 0..=plakous_gateway_protocol::facts::MAX_WIRE_IDENTITY_ENTRIES {
        crowded
            .wire_identity
            .insert(format!("extra_{index}"), "v".into());
    }
    assert_eq!(
        gateway::wire_facts(&crowded, &instance, 1).err(),
        Some(CustodyError::Internal)
    );
    // A moved fence whose facts cannot be carried is an internal refusal.
    assert_eq!(
        gateway::gateway_refusal(
            &CustodyError::FenceMoved {
                facts: Box::new(crowded),
            },
            &instance
        ),
        plakous_gateway_protocol::refusal::GatewayRefusal::Internal {}
    );
}
