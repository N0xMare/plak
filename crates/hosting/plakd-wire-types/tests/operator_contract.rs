//! Protocol-9 client frames and secret-safe envelope contracts.

use plakd_wire_types::PlakExtension;
use plakous_host_protocol::{
    ProtocolExtension,
    wire::{ClientFrame, HostFrame, WIRE_VERSION},
};
use serde_json::{Value, json};

#[test]
fn protocol_ten_operator_fixtures_are_addressed_and_secret_debug_is_redacted()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(WIRE_VERSION, 10);
    for line in include_str!("fixtures/operator-v9.jsonl").lines() {
        let value: Value = serde_json::from_str(line)?;
        let encoded = if value["frame"] == "request" {
            let request: ClientFrame<PlakExtension> = serde_json::from_value(value.clone())?;
            assert!(!format!("{request:?}").contains("synthetic-fixture-value"));
            assert!(value["op"].get("attachment").is_none());
            serde_json::to_value(request)?
        } else {
            let response = serde_json::from_value::<HostFrame<PlakExtension>>(value.clone())?;
            if value["outcome"]["ok"]["response"]["response"] == "login_progress" {
                let mut missing = value.clone();
                missing["outcome"]["ok"]["response"]
                    .as_object_mut()
                    .ok_or("response")?
                    .remove("warning");
                assert!(serde_json::from_value::<HostFrame<PlakExtension>>(missing).is_err());
            }
            let debug = format!("{response:?}");
            assert!(!debug.contains("https://example.invalid/fixture-authorization"));
            assert!(!debug.contains("synthetic-device-code"));
            serde_json::to_value(response)?
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

#[test]
fn protocol_eight_files_have_exact_bytes_and_only_saved_target_authority()
-> Result<(), Box<dyn std::error::Error>> {
    for line in include_str!("fixtures/files-v8.jsonl").lines() {
        let value: Value = serde_json::from_str(line)?;
        let encoded = if value["frame"] == "request" {
            serde_json::to_value(serde_json::from_value::<ClientFrame<PlakExtension>>(
                value.clone(),
            )?)?
        } else {
            let response = serde_json::from_value::<HostFrame<PlakExtension>>(value.clone())?;
            let debug = format!("{response:?}");
            assert!(
                !debug.contains("AP/i")
                    && !debug.contains("bmVlZGxl")
                    && !debug.contains("cGF0Y2g=")
            );
            serde_json::to_value(response)?
        };
        assert_eq!(encoded, value);
    }
    for extra in [
        json!({"offset":1}),
        json!({"runner":"arbitrary"}),
        json!({"regex":".*"}),
    ] {
        let mut request =
            json!({"request":"files_read","target":{"session":"fixture"},"path":"data"});
        request
            .as_object_mut()
            .ok_or("object")?
            .extend(extra.as_object().ok_or("object")?.clone());
        assert!(serde_json::from_value::<plakd_wire_types::console::HostRequest>(request).is_err());
    }
    Ok(())
}
