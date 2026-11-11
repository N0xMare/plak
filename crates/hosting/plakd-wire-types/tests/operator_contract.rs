//! Protocol-7 client frames and secret-safe envelope contracts.

use plakd_wire_types::PlakExtension;
use plakous_host_protocol::{
    ProtocolExtension,
    wire::{ClientFrame, HostFrame, WIRE_VERSION},
};
use serde_json::{Value, json};

#[test]
fn protocol_seven_operator_fixtures_are_addressed_and_secret_debug_is_redacted()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(WIRE_VERSION, 7);
    for line in include_str!("fixtures/operator-v7.jsonl").lines() {
        let value: Value = serde_json::from_str(line)?;
        let encoded = if value["frame"] == "request" {
            let request: ClientFrame<PlakExtension> = serde_json::from_value(value.clone())?;
            assert!(!format!("{request:?}").contains("synthetic-fixture-value"));
            assert!(value["op"].get("attachment").is_none());
            serde_json::to_value(request)?
        } else {
            serde_json::to_value(serde_json::from_value::<HostFrame<PlakExtension>>(
                value.clone(),
            )?)?
        };
        assert_eq!(encoded, value);
    }
    Ok(())
}

#[test]
fn credential_and_config_patch_envelopes_are_bounded_before_decoding_secrets() {
    let large = "x".repeat(64 * 1024);
    for request in [
        json!({"request":"account_key_set", "account_id":"openai/work", "secret":large, "confirmation":"fixture"}),
        json!({"request":"account_login_complete", "attempt":"fixture", "pasted":large}),
        json!({"request":"config_patch", "changes":[{"key":"root.agent","value":large}], "if_match":"fixture", "confirmation":"fixture"}),
    ] {
        let line =
            json!({"frame":"request","id":1,"op":{"op":"host","request":request}}).to_string();
        assert!(!PlakExtension::host_request_size_allowed(&line));
    }
    let line = json!({"frame":"request","id":1,"op":{"op":"host","request":{"request":"template_put","id":"work","changes":[{"field":"description","value":large}],"if_match":"fixture","confirmation":"fixture"}}}).to_string();
    assert!(PlakExtension::host_request_size_allowed(&line));
}
