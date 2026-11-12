//! An external operator client consumes public DTOs without product handlers.

use plakd_wire_types::{
    PlakExtension,
    console::{ConfigChange, HostRequest, HostResponse, OperatorAction, SecretInput, SettingValue},
};
use plakous_host_protocol::wire::{Answer, ClientFrame, HostFrame, Op, Outcome, WIRE_VERSION};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(WIRE_VERSION, 8);
    let target = plakd_wire_types::console::WorkspaceTarget { session: "saved".into(), node: None };
    let _ = HostRequest::FilesRead { target: target.clone(), path: "data".into(), cursor: None, max_bytes: Some(1) };
    let _ = HostRequest::ChangesDiff { target, entry: "opaque".into() };
    let _ = plakd_wire_types::console::ReadRefusal::WorkspaceReadFailed { cause: plakd_wire_types::console::WorkspaceReadFailure::ContentChanged };
    let action = OperatorAction::AccountKeySet {
        account_id: "openai/work".into(),
    };
    let request = ClientFrame::<PlakExtension>::Request {
        id: 1,
        op: Op::Host {
            request: HostRequest::OperatorPrepare { action },
        },
    };
    assert_eq!(
        serde_json::from_value::<ClientFrame<PlakExtension>>(serde_json::to_value(&request)?)?,
        request
    );
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
    assert_eq!(
        serde_json::from_value::<ClientFrame<PlakExtension>>(serde_json::to_value(&frame)?)?,
        frame
    );
    let response = HostFrame::<PlakExtension>::Response {
        id: 2,
        outcome: Outcome::Ok(Answer::Host {
            response: HostResponse::Refused {
                reason: plakd_wire_types::console::ReadRefusal::Conflict {
                    etag: "1".repeat(64),
                },
            },
        }),
    };
    assert_eq!(
        serde_json::from_value::<HostFrame<PlakExtension>>(serde_json::to_value(&response)?)?,
        response
    );
    Ok(())
}
