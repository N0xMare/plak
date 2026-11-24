//! Offline protocol 13 example and strict-decoding contracts.

use plakd_wire_types::PlakExtension;
use plakd_wire_types::v13::{ClientFrame, HostFrame, SessionPreview};
use plakous_host_protocol::wire;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

const CLIENT: &str = include_str!("fixtures/protocol-13-client.jsonl");
const HOST: &str = include_str!("fixtures/protocol-13-host.jsonl");

fn round_trip<T: DeserializeOwned + Serialize>(
    fixture: &str,
) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    fixture
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let decoded: T = serde_json::from_str(line)
                .map_err(|error| format!("fixture line {}: {error}", index + 1))?;
            let value = serde_json::from_str::<Value>(line)?;
            assert_eq!(serde_json::to_value(decoded)?, value, "line {}", index + 1);
            Ok(value)
        })
        .collect()
}

#[test]
fn examples_round_trip_as_complete_frames() -> Result<(), Box<dyn std::error::Error>> {
    let client = round_trip::<ClientFrame>(CLIENT)?;
    let host = round_trip::<HostFrame>(HOST)?;
    assert_eq!(client[0]["proto"], 13);
    assert_eq!(wire::WIRE_VERSION, 12);

    let append = &host[5]["observation"]["body"];
    let first = &host[1];
    let baseline = &host[6];
    let original = first["items"][1]["row"]["text"]
        .as_str()
        .unwrap_or_default();
    let text = append["text"].as_str().unwrap_or_default();
    assert_eq!(append["row"], first["live_rows"][0]["row"]);
    assert_eq!(append["offset"], original.len());
    assert_eq!(append["end"], original.len() + text.len());
    assert_eq!(baseline["live_rows"][0]["len"], append["end"]);
    assert_eq!(
        baseline["items"][1]["row"]["text"],
        format!("{original}{text}"),
    );
    assert_eq!(host[3]["last"], false);
    assert_eq!(host[4]["last"], true);
    assert_eq!(host[3]["id"], host[4]["id"]);
    assert_eq!(host[3]["items"][1]["truncated_bytes"], 2);
    assert_eq!(host[9]["request_id"], client[4]["id"]);
    assert_eq!(
        host[10]["outcome"]["ok"]["transcript"]["from"],
        first["revision"]
    );
    assert_eq!(
        host[10]["outcome"]["ok"]["transcript"]["to"],
        baseline["revision"]
    );
    Ok(())
}

#[test]
fn draft_replaces_legacy_cases_instead_of_accepting_both() -> Result<(), serde_json::Error> {
    let legacy = [
        json!({"frame":"response","id":1,"outcome":{"ok":{"answer":"attached","attachment":7}}}),
        json!({"frame":"snapshot","attachment":7,"schema":1,"scope":"root","revision":10,"page":0,
            "items":[],"last":true}),
        json!({"frame":"event","attachment":7,"seq":1,"class":{"event":"droppable"},
            "observation":{"kind":"assistant_delta","body":"hello"},"blobs":[]}),
        json!({"frame":"response","id":1,"outcome":{"refused":{"refusal":"placement_refused",
            "runner":{"name":"paired","runner_id":"runner-1"},
            "reason":{"reason":"runner_unreachable","phase":"reconcile","timed_out":true}}}}),
    ];
    for frame in legacy {
        serde_json::from_value::<wire::HostFrame<PlakExtension>>(frame.clone())?;
        assert!(
            serde_json::from_value::<HostFrame>(frame.clone()).is_err(),
            "{frame}"
        );
    }
    for frame in HOST.lines().filter(|line| {
        line.contains("\"frame\":\"blob_refused\"") || line.contains("\"frame\":\"history_page\"")
    }) {
        assert!(
            serde_json::from_str::<wire::HostFrame<PlakExtension>>(frame).is_err(),
            "{frame}"
        );
    }
    Ok(())
}

#[test]
fn preview_bound_counts_bytes_not_characters() {
    assert!(SessionPreview::try_from("é".repeat(80)).is_ok());
    assert!(SessionPreview::try_from("é".repeat(81)).is_err());
    assert!(SessionPreview::try_from("two\nlines".to_owned()).is_err());
}

#[test]
fn named_template_writes_and_profile_reads_do_not_accept_legacy_tiers()
-> Result<(), Box<dyn std::error::Error>> {
    let named = CLIENT
        .lines()
        .find(|line| line.contains("\"value\":\"ultrafast\""))
        .ok_or("named template write fixture")?;
    let draft: ClientFrame = serde_json::from_str(named)?;
    assert!(!format!("{draft:?}").contains("fixture-confirmation"));
    assert!(serde_json::from_str::<wire::ClientFrame<PlakExtension>>(named).is_err());
    let mut old: Value = serde_json::from_str(named)?;
    old["op"]["request"]["changes"][0] = json!({"field":"fast_mode","value":true});
    serde_json::from_value::<wire::ClientFrame<PlakExtension>>(old.clone())?;
    assert!(serde_json::from_value::<ClientFrame>(old.clone()).is_err());
    for invalid in [
        json!({"field":"service_tier","value":"priority"}),
        json!({"field":"service_tier"}),
    ] {
        old["op"]["request"]["changes"][0] = invalid;
        assert!(serde_json::from_value::<ClientFrame>(old.clone()).is_err());
    }

    let named = HOST
        .lines()
        .find(|line| line.contains("\"kind\":\"extension\"") && line.contains("\"service_tier\""))
        .ok_or("profile fixture")?;
    let mut old: Value = serde_json::from_str(named)?;
    old["observation"]["body"]["profiles"]["rows"][0]["root"] = json!({
        "label":"root","account":"codex","model":"gpt-6.2-sol","effort":"low","fast":true,"depth":0
    });
    serde_json::from_value::<wire::HostFrame<PlakExtension>>(old.clone())?;
    assert!(serde_json::from_value::<HostFrame>(old).is_err());
    Ok(())
}
