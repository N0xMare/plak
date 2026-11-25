//! An external operator client consumes public DTOs without product handlers.

use plakd_wire_types::{
    PlakExtension,
    console::{ConfigChange, HostRequest, OperatorAction, SecretInput, SettingValue},
    v13,
};
use plakous_host_protocol::wire::{ClientFrame, Op};

fn round_trip(request: &ClientFrame<PlakExtension>) -> Result<(), serde_json::Error> {
    let encoded = serde_json::to_value(request)?;
    let served: v13::ClientFrame = serde_json::from_value(encoded.clone())?;
    assert_eq!(serde_json::to_value(served)?, encoded);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(v13::WIRE_VERSION, 13);
    let _ = plakd_wire_types::console::CredentialRefreshObservation {
        outcome: plakd_wire_types::console::CredentialRefreshOutcome::Restored,
        observed_at: "2026-01-01T00:00:00Z".into(),
        failure_class: Some("timeout".into()),
    };
    let target = plakd_wire_types::console::WorkspaceTarget {
        session: "saved".into(),
        node: None,
    };
    let _ = HostRequest::FilesRead {
        target: target.clone(),
        path: "data".into(),
        cursor: None,
        max_bytes: Some(1),
    };
    let _ = HostRequest::ChangesDiff {
        target,
        entry: "opaque".into(),
    };
    let _ = plakd_wire_types::console::ReadRefusal::WorkspaceReadFailed {
        cause: plakd_wire_types::console::WorkspaceReadFailure::ContentChanged,
        version: None,
    };
    let action = OperatorAction::AccountKeySet {
        account_id: "openai/work".into(),
    };
    let request = ClientFrame::<PlakExtension>::Request {
        id: 1,
        op: Op::Host {
            request: HostRequest::OperatorPrepare { action },
        },
    };
    round_trip(&request)?;
    let secret = SecretInput::new(zeroize::Zeroizing::new("synthetic-fixture-value".into()))?;
    let request = HostRequest::AccountKeySet {
        account_id: "openai/work".into(),
        secret,
        confirmation: "one-shot-fixture".into(),
    };
    assert!(!format!("{request:?}").contains("synthetic-fixture-value"));
    let request = HostRequest::ConfigPatch {
        if_match: "0".repeat(64),
        confirmation: "one-shot-fixture".into(),
        changes: vec![ConfigChange {
            key: "turn.auto_continue".into(),
            value: SettingValue::Bool(false),
        }],
    };
    let frame = ClientFrame::<PlakExtension>::Request {
        id: 2,
        op: Op::Host { request },
    };
    round_trip(&frame)?;
    let response = v13::HostFrameV13::Response {
        id: 2,
        outcome: v13::Outcome::Ok(Box::new(v13::Answer::Draft(v13::AnswerV13::Host {
            response: Box::new(v13::HostResponse::Draft(v13::HostResponseV13::Refused {
                reason: v13::ReadRefusal::Shared(
                    plakd_wire_types::console::ReadRefusal::Conflict {
                        etag: "1".repeat(64),
                    },
                ),
            })),
        }))),
    };
    assert_eq!(
        serde_json::from_value::<v13::HostFrameV13>(serde_json::to_value(&response)?)?,
        response
    );
    Ok(())
}
