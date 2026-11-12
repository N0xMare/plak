use super::*;
use crate::Core;

type Client = ClientFrame<Core>;
type Host = HostFrame<Core>;

#[test]
fn rename_title_schema_requires_a_nullable_bounded_string() {
    let schema = serde_json::to_value(schemars::schema_for!(Client)).unwrap();
    let rename = schema["$defs"]["Op"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|operation| operation["properties"]["op"]["const"] == "rename_session")
        .unwrap();
    let title = &rename["properties"]["title"];
    assert_eq!(title["type"], serde_json::json!(["string", "null"]));
    assert_eq!(title["minLength"], 1);
    assert_eq!(title["maxLength"], 128);
    assert!(
        rename["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "title")
    );
}

#[test]
fn attached_session_info_requires_runner_and_stays_outside_jsonl_history() {
    let info = crate::views::presentation::SessionInfoView {
        runner: RunnerLabel {
            name: "machine".to_owned(),
            runner_id: "saved-id".to_owned(),
        },
        ..Default::default()
    };
    let mut value = serde_json::to_value(&info).unwrap();
    assert_eq!(
        value["runner"],
        serde_json::json!({"name":"machine","runner_id":"saved-id"})
    );
    assert_eq!(
        serde_json::from_value::<crate::views::presentation::SessionInfoView>(value.clone())
            .unwrap(),
        info
    );
    value.as_object_mut().unwrap().remove("runner");
    assert!(serde_json::from_value::<crate::views::presentation::SessionInfoView>(value).is_err());
    let schema = serde_json::to_value(schemars::schema_for!(
        crate::views::presentation::SessionInfoView
    ))
    .unwrap();
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "runner")
    );
    assert!(
        crate::jsonl::project_host_event::<Core>(
            &SessionObservation::SessionInfo(info),
            crate::envelope::JsonlCorrelation {
                prompt_id: None,
                omit_unknown_turn_id: false,
                command_id: None,
                pending_tree_snapshot_id: None
            },
        )
        .is_none()
    );
}

fn pin_client(value: &str) {
    let frame: Client = decode_line(value).unwrap();
    assert_eq!(encode_line(&frame).unwrap().trim_end(), value);
}

fn pin_host(value: &str) {
    let frame: Host = decode_host_line(value).unwrap();
    assert_eq!(encode_line(&frame).unwrap().trim_end(), value);
}

#[test]
fn protocol_five_directory_requests_have_exact_closed_shapes() {
    assert_eq!(WIRE_VERSION, 9);
    for request in [
        r#"{"frame":"request","id":1,"op":{"op":"new_session","runner":{"name":"machine","runner_id":"pinned-id"},"workspace":"/work","profile":"claude-default","request_id":"00000000-0000-4000-8000-000000000001"}}"#,
        r#"{"frame":"request","id":2,"op":{"op":"session_options","workspace":{"runner":{"name":"machine","runner_id":"pinned-id"},"path":"/work"}}}"#,
        r#"{"frame":"request","id":3,"op":{"op":"rename_session","session":"session-id","title":null}}"#,
        r#"{"frame":"request","id":4,"op":{"op":"archive_session","session":"session-id","archived":true}}"#,
        r#"{"frame":"request","id":5,"op":{"op":"delete_session","session":"session-id","abandon_children":false}}"#,
        r#"{"frame":"request","id":6,"op":{"op":"list_sessions","workspace":"/work","runner":"pinned-id","include_archived":true}}"#,
        r#"{"frame":"request","id":7,"op":{"op":"watch_sessions","include_archived":true}}"#,
    ] {
        pin_client(request);
    }
    for malformed in [
        r#"{"frame":"request","id":1,"op":{"op":"new_session","workspace":"/work","request_id":"00000000-0000-4000-8000-000000000001"}}"#,
        r#"{"frame":"request","id":1,"op":{"op":"rename_session","session":"session-id"}}"#,
        r#"{"frame":"request","id":1,"op":{"op":"archive_session","session":"session-id","archived":true,"force":true}}"#,
    ] {
        assert!(decode_line::<Client>(malformed).is_err(), "{malformed}");
    }
}

#[test]
fn protocol_five_verdicts_distinguish_mailbox_durable_pending_and_runner_stop_debt() {
    for response in [
        r#"{"frame":"response","id":1,"outcome":{"ok":{"answer":"accepted","duplicate":false}}}"#,
        r#"{"frame":"response","id":2,"outcome":{"ok":{"answer":"accepted","duplicate":true}}}"#,
        r#"{"frame":"response","id":3,"outcome":{"refused":{"refusal":"draft_pending","client_id":"00000000-0000-4000-8000-000000000001"}}}"#,
        r#"{"frame":"response","id":4,"outcome":{"refused":{"refusal":"draft_registry_full","limit":100000}}}"#,
        r#"{"frame":"response","id":5,"outcome":{"ok":{"answer":"stopped","runner_pending":true}}}"#,
        r#"{"frame":"response","id":6,"outcome":{"ok":{"answer":"renamed","session":"session-id","title":"kept title"}}}"#,
        r#"{"frame":"response","id":7,"outcome":{"ok":{"answer":"archive_updated","session":"session-id","archived":true}}}"#,
        r#"{"frame":"response","id":8,"outcome":{"ok":{"answer":"deleted","session":"session-id"}}}"#,
        r#"{"frame":"response","id":9,"outcome":{"refused":{"refusal":"session_archived","session":"session-id"}}}"#,
        r#"{"frame":"response","id":10,"outcome":{"refused":{"refusal":"session_lifecycle_refused","session":"session-id","reason":{"reason":"invalid_title","kind":"control_character"}}}}"#,
        r#"{"frame":"response","id":11,"outcome":{"refused":{"refusal":"placement_refused","runner":{"name":"machine","runner_id":"pinned-id"},"reason":{"reason":"runner_unreachable","phase":"reconcile","timed_out":false}}}}"#,
        r#"{"frame":"response","id":12,"outcome":{"refused":{"refusal":"binding_refused","kind":"unknown_profile"}}}"#,
        r#"{"frame":"options_progress","request_id":13,"runner":{"name":"machine","runner_id":"pinned-id"},"phase":null,"row":null}"#,
    ] {
        pin_host(response);
    }
    assert!(
        decode_host_line::<Core>(
            r#"{"frame":"response","id":1,"outcome":{"ok":{"answer":"accepted"}}}"#
        )
        .is_err()
    );
}

#[test]
fn discovery_head_states_are_distinct_and_metadata_has_no_second_root() {
    for (head, spelling) in [
        (
            RepositoryHead::Branch {
                name: "main".into(),
            },
            r#"{"kind":"branch","name":"main"}"#,
        ),
        (RepositoryHead::Detached, r#"{"kind":"detached"}"#),
        (
            RepositoryHead::Unborn { name: "new".into() },
            r#"{"kind":"unborn","name":"new"}"#,
        ),
        (RepositoryHead::Unavailable, r#"{"kind":"unavailable"}"#),
    ] {
        assert_eq!(serde_json::to_string(&head).unwrap(), spelling);
        assert_eq!(
            serde_json::from_str::<RepositoryHead>(spelling).unwrap(),
            head
        );
    }
    assert!(
        serde_json::from_str::<RepositoryMetadata>(
            r#"{"name":"repo","head":{"kind":"unavailable"},"commit":null,"root":"/work"}"#
        )
        .is_err()
    );
}
