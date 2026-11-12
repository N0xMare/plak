//! Exact product terminal frames remain independent of agent observations.

#![allow(clippy::unwrap_used, reason = "exact terminal wire fixtures")]

use plakd_wire_types::{PlakExtension, console::HostRequest};
use plakous_host_protocol::wire::{ClientFrame, HostFrame, WIRE_VERSION};

#[test]
fn protocol_nine_terminals_have_separate_bytes_cursors_and_refusals() {
    assert_eq!(WIRE_VERSION, 9);
    for line in include_str!("fixtures/terminal-v9.jsonl").lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let encoded = if value["frame"] == "request" {
            serde_json::to_value(
                serde_json::from_value::<ClientFrame<PlakExtension>>(value.clone()).unwrap(),
            )
            .unwrap()
        } else {
            serde_json::to_value(
                serde_json::from_value::<HostFrame<PlakExtension>>(value.clone()).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(encoded, value);
    }
    let invalid = serde_json::json!({"request":"terminal_open","session":"session","node":"node","terminal_id":"terminal","dimensions":{"rows":24,"columns":80},"confirmation":"grant","command":"not allowed"});
    assert!(serde_json::from_value::<HostRequest>(invalid).is_err());
    assert!(
        serde_json::from_value::<plakd_wire_types::console::ReadRefusal>(
            serde_json::json!({"reason":"terminal_backpressure"})
        )
        .is_err(),
        "untimed backpressure must explicitly carry null, not omit the field"
    );
}
