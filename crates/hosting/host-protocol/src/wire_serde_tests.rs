//! Wire v2 serde: every intent and observation encodes as a tagged JSON
//! object (`kind`, and `body` when it carries one) and decodes back to the
//! same value. A peer cannot construct a snapshot or patch the host would
//! refuse to emit, and image bytes travel as base64.

use serde_json::{Value, json};

use crate::intent::{ApprovalChoice, Core, Draft, DraftImage, SessionIntent};
use crate::observation::SessionObservation;
use crate::status::DiagnosticText;
use crate::views::PresentationSnapshot;
use crate::views::transcript::{TranscriptPatch, TranscriptScope};

type Intent = SessionIntent<Core>;
type Observation = SessionObservation<Core>;

fn round_trips<T>(value: &T) -> Value
where
    T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq,
{
    let encoded = serde_json::to_value(value).unwrap();
    let decoded: T = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(&decoded, value);
    encoded
}

fn intents() -> Vec<Intent> {
    let id = || "q1".to_owned();
    vec![
        Intent::Prompt(Draft::from("hi")),
        Intent::FollowUp(Draft::from("more")),
        Intent::Steer(Draft::from("steer")),
        Intent::Background,
        Intent::Interrupt,
        Intent::StopProcesses {
            id: Some("p1".into()),
            interrupt: true,
        },
        Intent::DropQueued,
        Intent::SendQueuedNow {
            id: id(),
            revision: 1,
        },
        Intent::DeleteQueued {
            id: id(),
            revision: 1,
        },
        Intent::EditQueued {
            id: id(),
            revision: 1,
            text: "t".into(),
        },
        Intent::MoveQueued {
            id: id(),
            revision: 1,
            new_index: 0,
        },
        Intent::ResumeQueueDrain,
        Intent::ResolveApproval {
            approval_id: "a1".into(),
            choice: ApprovalChoice::Allow,
        },
        Intent::Quit,
        Intent::NewSession,
        Intent::Compact,
        Intent::ResumeSession { id: None },
        Intent::ResumeSessionHandoff { id: "s1".into() },
        Intent::ForkSession {
            account: None,
            compact: false,
        },
        Intent::RefreshReadiness,
        Intent::RetryAttach,
        Intent::Recover,
        Intent::OpenTopologyNavigator,
        Intent::OpenAgentTranscript {
            agent_id: "c1".into(),
        },
        Intent::CloseAgentTranscript,
        Intent::ResyncTranscript {
            scope: TranscriptScope::Agent("c1".into()),
        },
        Intent::RefreshTopologyTree,
        Intent::ResumeTopologyTree,
        Intent::InterruptTopologyNode {
            agent_id: "c1".into(),
        },
        Intent::CloseTopologyNode {
            agent_id: "c1".into(),
        },
        Intent::StopAllTopology,
        Intent::RecoverTopologyNode {
            tree_id: "t1".into(),
            revision: 2,
            agent_id: "c1".into(),
            expected_attempt_id: "c1#1".into(),
        },
        Intent::RefreshTemplates,
        Intent::InvokeTool {
            correlation_id: None,
            name: "list_agents".into(),
            arguments: "{}".into(),
            agent_id: None,
        },
        Intent::RequestCallDetail {
            call_id: "call1".into(),
            agent_id: Some("c1".into()),
            session_id: "s1".into(),
        },
    ]
}

/// Every core intent round-trips; its tag is its snake-case name.
#[test]
fn every_intent_round_trips_tagged_by_kind() {
    let intents = intents();
    assert_eq!(intents.len(), 35, "every core intent but Extension");
    for intent in &intents {
        let encoded = round_trips(intent);
        assert!(encoded["kind"].is_string(), "{encoded}");
    }
    assert_eq!(
        round_trips(&Intent::Interrupt),
        json!({ "kind": "interrupt" })
    );
    assert_eq!(
        round_trips(&Intent::ResolveApproval {
            approval_id: "a1".into(),
            choice: ApprovalChoice::Deny,
        }),
        json!({ "kind": "resolve_approval", "body": { "approval_id": "a1", "choice": "deny" } })
    );
}

/// Image bytes travel as base64 text and come back byte-exact.
#[test]
fn draft_images_travel_as_base64() {
    let draft = Draft::new(
        "look".into(),
        vec![DraftImage {
            name: "a.png".into(),
            media_type: "image/png".into(),
            bytes: vec![0, 159, 146, 150],
            source_dimensions: Some((10, 20)),
        }],
    );
    let encoded = round_trips(&Intent::Prompt(draft));
    assert_eq!(encoded["body"]["images"][0]["bytes"], json!("AJ+Slg=="));
}

/// A field the type does not know is refused, never ignored.
#[test]
fn unknown_fields_are_refused() {
    let refused = serde_json::from_value::<Intent>(json!({
        "kind": "close_topology_node",
        "body": { "agent_id": "c1", "extra": true },
    }));
    assert!(refused.is_err());
}

/// Snapshots and patches decode only when they validate.
#[test]
fn a_snapshot_or_patch_that_does_not_validate_is_refused() {
    let snapshot = Observation::PresentationSnapshot(PresentationSnapshot::empty(4));
    let mut encoded = round_trips(&snapshot);
    encoded["body"]["schema"] = json!(99);
    assert!(serde_json::from_value::<Observation>(encoded).is_err());

    let patch = TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 1,
        to_revision: 2,
        upserts: Vec::new(),
        deletes: Vec::new(),
    };
    let mut encoded = round_trips(&Observation::TranscriptPatch(patch));
    encoded["body"]["to_revision"] = json!(1);
    assert!(serde_json::from_value::<Observation>(encoded).is_err());
}

/// A received diagnostic is bounded like one made here.
#[test]
fn a_received_diagnostic_is_bounded() {
    let text: DiagnosticText = serde_json::from_value(json!("x".repeat(10_000))).unwrap();
    assert_eq!(text.as_str().len(), 4096);
    assert_eq!(round_trips(&text), json!("x".repeat(4096)));
}

/// The plain-valued observations round-trip.
#[test]
fn plain_observations_round_trip() {
    for observation in crate::observation::class_samples() {
        round_trips(&observation);
    }
}
