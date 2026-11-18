//! Wire-11 advisory lifecycle and pre-handshake discovery vocabulary.

#![allow(clippy::unwrap_used, reason = "contract-owned codec values")]

use plakd_wire_types::{
    PlakExtension,
    lifecycle::{BridgeUnavailableReason, HostLifecycle},
};
use plakous_host_protocol::wire::{HandshakeRefusal, HostFrame, WIRE_VERSION};
use serde_json::json;

#[test]
fn lifecycle_is_a_closed_advisory_frame_with_a_bounded_retry_hint() {
    assert_eq!(WIRE_VERSION, 12);
    for phase in ["draining", "restarting", "stopping", "running"] {
        for reason in ["paired_upgrade", "cross_key_upgrade", "shutdown"] {
            let value = json!({"frame":"host_lifecycle","host_generation":7,
                "phase":phase,"reason":reason,"target":{"binary_id":"next","state_key":"next-key"},
                "retry_after_ms":30000});
            let frame: HostFrame<PlakExtension> = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(frame).unwrap(), value);
        }
    }
    let mut value = json!({"host_generation":7,"phase":"draining","reason":"paired_upgrade"});
    assert!(serde_json::from_value::<HostLifecycle>(value.clone()).is_ok());
    for bad in [json!(30001), json!(-1), json!(4_294_967_296_u64)] {
        value["retry_after_ms"] = bad;
        assert!(serde_json::from_value::<HostLifecycle>(value.clone()).is_err());
    }
    value["retry_after_ms"] = json!(1);
    value["accepted"] = json!(true);
    assert!(serde_json::from_value::<HostLifecycle>(value).is_err());
}

#[test]
fn bridge_refusals_have_exact_product_bytes_and_no_untrusted_detail() {
    for (reason, name) in [
        (BridgeUnavailableReason::Absent, "absent"),
        (BridgeUnavailableReason::Starting, "starting"),
        (BridgeUnavailableReason::Stopping, "stopping"),
        (BridgeUnavailableReason::StartRefused, "start_refused"),
        (
            BridgeUnavailableReason::IncompatibleState,
            "incompatible_state",
        ),
        (
            BridgeUnavailableReason::VerificationFailed,
            "verification_failed",
        ),
    ] {
        let retry_after_ms = matches!(
            reason,
            BridgeUnavailableReason::Absent
                | BridgeUnavailableReason::Starting
                | BridgeUnavailableReason::Stopping
        )
        .then_some(500);
        let mut value = json!({"frame":"refused","kind":"bridge_unavailable","reason":name});
        if let Some(hint) = retry_after_ms {
            value["retry_after_ms"] = json!(hint);
        }
        let frame = HostFrame::<PlakExtension>::Refused(HandshakeRefusal::BridgeUnavailable {
            reason,
            retry_after_ms,
        });
        assert_eq!(serde_json::to_value(&frame).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<HostFrame<PlakExtension>>(value).unwrap(),
            frame
        );
    }
    assert!(
        serde_json::from_value::<HostFrame<PlakExtension>>(
            json!({"frame":"refused","kind":"bridge_unavailable","reason":"unknown"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<HostFrame<PlakExtension>>(
            json!({"frame":"refused","kind":"bridge_unavailable","reason":"absent","detail":"private"})
        )
        .is_err()
    );
    let frame: HostFrame<PlakExtension> = serde_json::from_value(
        json!({"frame":"refused","kind":"bridge_unavailable","reason":"verification_failed"}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(frame).unwrap(),
        json!({"frame":"refused","kind":"bridge_unavailable","reason":"verification_failed"})
    );
    assert!(serde_json::from_value::<HostFrame<PlakExtension>>(json!({"frame":"refused","kind":"bridge_unavailable","reason":"absent","retry_after_ms":30001})).is_err());
}
