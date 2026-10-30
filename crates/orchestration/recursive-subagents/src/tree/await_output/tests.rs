use super::*;
use crate::protocol::{AttemptStatus, MessagePriority, NodeStatus};

fn row(agent_id: &str, output: Option<usize>) -> WaitAgentRow {
    WaitAgentRow {
        agent_id: agent_id.to_owned(),
        status: NodeStatus::Completed,
        attempt_id: Some(AttemptId::new(agent_id, 1)),
        attempt: AttemptStatus::Completed,
        candidate: None,
        output_json: output.map(|bytes| "o".repeat(bytes)),
        blocked: None,
        failure: None,
        usage: None,
        waiting_on: None,
    }
}

fn mail(id: &str, from: &str, purpose: MessagePurpose, bytes: usize) -> InboxMail {
    InboxMail {
        body_bytes: bytes,
        message_id: id.to_owned(),
        from: from.to_owned(),
        purpose,
        priority: MessagePriority::Deferred,
        in_reply_to: None,
        body: "m".repeat(bytes),
        attempt_id: (purpose == MessagePurpose::Result)
            .then(|| AttemptId::new(from, 1).as_str().to_owned()),
        status: None,
        candidate: None,
    }
}

fn output(agents: Vec<WaitAgentRow>, mail: Vec<InboxMail>, newly: &[&str]) -> AwaitOutput {
    AwaitOutput {
        mail,
        woke_by: WaitWakeReason::Settled,
        agents,
        timed_out: false,
        processes: Vec::new(),
        jobs: Vec::new(),
        omitted: None,
        newly_settled: newly.iter().map(|id| (*id).to_owned()).collect(),
    }
}

fn len(output: &AwaitOutput) -> usize {
    serde_json::to_string(output).unwrap().len()
}

const KIB: usize = 1024;

#[test]
fn budget_admits_new_results_then_mail_then_rereads_then_tails() {
    let mut fitted = output(
        vec![
            row("c1", Some(120 * KIB)),
            row("c2", Some(100 * KIB)),
            row("c3", Some(40 * KIB)),
        ],
        vec![
            mail("m1", "c3", MessagePurpose::Question, 2 * KIB),
            mail("m2", "c2", MessagePurpose::Result, 0),
            mail("m3", "c1", MessagePurpose::Coordinate, 2 * KIB),
        ],
        &["c2", "c3"],
    );
    fitted.jobs = vec![AwaitJobRow {
        id: "p1".to_owned(),
        status: "exited",
        exit_code: Some(0),
        output: Some("t".repeat(4 * KIB)),
    }];
    fitted.fit_budget();
    // c2 and c3 are new (target order), then both plain mails; c1's
    // re-read no longer fits and is skipped, the small tail still fits.
    assert!(fitted.agents[0].output_json.is_none());
    assert!(fitted.agents[1].output_json.is_some());
    assert!(fitted.agents[2].output_json.is_some());
    let mail: Vec<_> = fitted
        .mail
        .iter()
        .map(|mail| mail.message_id.as_str())
        .collect();
    assert_eq!(mail, ["m1", "m2", "m3"]);
    assert!(fitted.jobs[0].output.is_some());
    assert_eq!(
        fitted.omitted,
        Some(AwaitOmitted {
            outputs: Vec::new(),
            rereads: vec!["c1".to_owned()],
            mail: 0,
            job_output: Vec::new(),
        })
    );
    // A withheld re-read leaves the row delivered: an identical wait
    // would withhold it again, so the note says to name it alone.
    assert!(!fitted.output_withheld("c1"));
    assert!(!fitted.output_withheld("c2"));
    assert!(len(&fitted) <= WAIT_OUTPUT_BUDGET_BYTES);
    let rendered = serde_json::to_value(&fitted).unwrap();
    assert_eq!(rendered["omitted"]["outputs"], serde_json::json!([]));
    assert_eq!(
        rendered["omitted"]["rereads"],
        serde_json::Value::from(vec!["c1"])
    );
    assert_eq!(
        rendered["omitted"]["note"],
        format!("{OMITTED_NOTE}{REREADS_NOTE}")
    );
    assert!(REREADS_NOTE.contains("alone"));
    assert!(OMITTED_NOTE.contains(&WAIT_OUTPUT_BUDGET_BYTES.to_string()));
}

#[test]
fn a_withheld_result_keeps_its_envelope_and_mail_is_admitted_in_arrival_order() {
    let mut fitted = output(
        vec![row("c1", Some(200 * KIB)), row("c2", Some(200 * KIB))],
        (0..40)
            .map(|index| {
                mail(
                    &format!("m{index}"),
                    "c1",
                    MessagePurpose::Coordinate,
                    2 * KIB,
                )
            })
            .chain([mail("r2", "c2", MessagePurpose::Result, 0)])
            .collect(),
        &["c1", "c2"],
    );
    fitted.fit_budget();
    assert!(fitted.agents[0].output_json.is_some());
    assert!(fitted.output_withheld("c2"));
    assert!(fitted.mail.iter().all(|mail| mail.message_id != "r2"));
    let admitted: Vec<_> = fitted
        .mail
        .iter()
        .map(|mail| mail.message_id.clone())
        .collect();
    let expected: Vec<_> = (0..admitted.len())
        .map(|index| format!("m{index}"))
        .collect();
    assert_eq!(admitted, expected);
    assert!(!admitted.is_empty() && admitted.len() < 40);
    let omitted = fitted.omitted.as_ref().unwrap();
    assert_eq!(omitted.mail, 41 - admitted.len());
    assert_eq!(omitted.rereads, [] as [String; 0]);
    assert!(len(&fitted) <= WAIT_OUTPUT_BUDGET_BYTES);
    let rendered = serde_json::to_value(&fitted).unwrap();
    assert_eq!(rendered["omitted"]["note"], OMITTED_NOTE);
}

/// Plain mail is acknowledged as an arrival-order prefix of plain mail; a
/// Result envelope travels with its row's admitted output whatever its
/// position, even when earlier plain mail was withheld.
#[test]
fn plain_mail_is_an_arrival_prefix_and_a_result_envelope_travels_with_its_output() {
    let mut fitted = output(
        vec![row("c1", Some(200 * KIB)), row("c2", None)],
        (0..40)
            .map(|index| {
                mail(
                    &format!("m{index}"),
                    "c2",
                    MessagePurpose::Coordinate,
                    2 * KIB,
                )
            })
            .chain([mail("r1", "c1", MessagePurpose::Result, 0)])
            .collect(),
        &["c1"],
    );
    fitted.fit_budget();
    assert!(fitted.omitted.as_ref().unwrap().mail > 0);
    assert!(fitted.agents[0].output_json.is_some());
    let admitted: Vec<String> = fitted
        .mail
        .iter()
        .map(|mail| mail.message_id.clone())
        .collect();
    let (result, plain) = admitted.split_last().unwrap();
    assert_eq!(result, "r1");
    let arrival: Vec<String> = (0..plain.len()).map(|index| format!("m{index}")).collect();
    assert_eq!(plain, arrival.as_slice());
    assert!(plain.len() < 40);
}

#[test]
fn the_first_budgeted_item_is_returned_whole_past_the_budget() {
    let escaped = "\"".repeat(200 * KIB);
    let mut fitted = output(vec![row("c1", None)], Vec::new(), &["c1"]);
    fitted.agents[0].output_json = Some(escaped.clone());
    fitted.fit_budget();
    assert_eq!(
        fitted.agents[0].output_json.as_deref(),
        Some(escaped.as_str())
    );
    assert_eq!(fitted.omitted, None);
    assert!(len(&fitted) > WAIT_OUTPUT_BUDGET_BYTES);
}

#[test]
fn an_output_within_the_budget_is_unchanged() {
    let mut fitted = output(
        vec![row("c1", Some(KIB)), row("c2", None)],
        vec![mail("r1", "c1", MessagePurpose::Result, 0)],
        &["c1"],
    );
    let before = fitted.clone();
    fitted.fit_budget();
    assert_eq!(fitted, before);
    assert!(
        serde_json::to_value(&fitted)
            .unwrap()
            .get("omitted")
            .is_none()
    );
}

#[test]
fn the_receipt_keeps_its_shape_and_the_host_subject_is_separate() {
    let fitted = output(
        vec![row("c1", None), row(" c2 ", None), row("", None)],
        Vec::new(),
        &[],
    );
    let value = serde_json::to_value(&fitted).unwrap();
    assert!(value.get("subject").is_none());
    assert_eq!(fitted.presentation_subject().as_deref(), Some("c1,c2"));
    assert!(
        output(vec![row("", None)], Vec::new(), &[])
            .presentation_subject()
            .is_none()
    );
    let long = "a".repeat(AWAIT_SUBJECT_BYTES + 8);
    let clipped = output(vec![row(&long, None)], Vec::new(), &[])
        .presentation_subject()
        .unwrap();
    assert!(clipped.ends_with('…'));
    assert!(clipped.len() <= AWAIT_SUBJECT_BYTES);
    let many = (0..32)
        .map(|index| row(&format!("agent-{index:02}-{}", "n".repeat(20)), None))
        .collect();
    let bounded = output(many, Vec::new(), &[])
        .presentation_subject()
        .unwrap();
    assert!(bounded.len() <= AWAIT_SUBJECT_BYTES);
    assert!(!bounded.ends_with(','), "{bounded}");
    assert!(bounded.contains("agent-00"));
    // Trim and drop blanks. A repeated id stays: the subject is the
    // joined ids, not a set.
    assert_eq!(
        bounded_agent_subject([" c1 ", "c1", "", "c2"]),
        Some("c1,c1,c2".into())
    );
}
