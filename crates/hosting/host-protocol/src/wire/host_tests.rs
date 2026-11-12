use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Extension;

impl ProtocolExtension for Extension {
    type Intent = crate::Never;
    type Observation = crate::Never;
    type HostRequest = bool;
    type HostResponse = u8;
    type WelcomeExtension = Option<bool>;
}

fn welcome() -> Welcome<Extension> {
    Welcome {
        proto: WIRE_VERSION,
        binary_id: "fixture".to_owned(),
        host_generation: 1,
        caps: Vec::new(),
        session: None,
        extension: None,
    }
}

#[test]
fn host_extension_has_private_request_response_and_required_nullable_welcome() {
    let request = ClientFrame::<Extension>::Request {
        id: 7,
        op: Op::Host { request: true },
    };
    assert_eq!(
        encode_line(&request).unwrap().trim_end(),
        r#"{"frame":"request","id":7,"op":{"op":"host","request":true}}"#
    );
    let response = HostFrame::<Extension>::Response {
        id: 7,
        outcome: Outcome::Ok(Answer::Host { response: 42 }),
    };
    let encoded = encode_line(&response).unwrap();
    assert_eq!(
        encoded.trim_end(),
        r#"{"frame":"response","id":7,"outcome":{"ok":{"answer":"host","response":42}}}"#
    );
    assert_eq!(
        decode_line::<HostFrame<Extension>>(&encoded).unwrap(),
        response
    );
    assert!(
        decode_line::<ClientFrame<crate::Core>>(
            r#"{"frame":"request","id":7,"op":{"op":"host","request":true}}"#
        )
        .is_err()
    );

    let mut value = serde_json::to_value(welcome()).unwrap();
    assert!(value["extension"].is_null());
    assert!(serde_json::from_value::<Welcome<Extension>>(value.clone()).is_ok());
    value.as_object_mut().unwrap().remove("extension");
    assert!(serde_json::from_value::<Welcome<Extension>>(value).is_err());
    let schema = serde_json::to_value(schemars::schema_for!(Welcome<Extension>)).unwrap();
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("extension"))
    );
    assert_eq!(
        schema["properties"]["extension"]["type"],
        serde_json::json!(["boolean", "null"])
    );
    let core = serde_json::to_value(schemars::schema_for!(Welcome<crate::Core>)).unwrap();
    let extension = &core["properties"]["extension"];
    assert!(
        extension["type"] == "null"
            || extension["anyOf"]
                .as_array()
                .is_some_and(|branches| branches.iter().any(|branch| branch["type"] == "null")),
        "{core}"
    );
}

#[test]
fn protocol_eight_rejects_both_adjacent_versions_without_negotiation() {
    assert_eq!(WIRE_VERSION, 8);
    for proto in [7, 9] {
        let hello = Hello {
            proto,
            binary_id: "fixture".to_owned(),
            caps: Vec::new(),
        };
        assert_eq!(
            admit_hello(&hello, welcome()),
            Err(HandshakeRefusal::VersionMismatch {
                host: 8,
                client: proto
            })
        );
    }
    let hello = Hello {
        proto: 8,
        binary_id: "fixture".to_owned(),
        caps: Vec::new(),
    };
    assert_eq!(admit_hello(&hello, welcome()), Ok(welcome()));
}
