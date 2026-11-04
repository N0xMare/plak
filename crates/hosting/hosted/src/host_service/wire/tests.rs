use plakous_host_protocol::intent::Draft;

use super::*;

fn root() -> NodeAddress {
    NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    }
}

/// A child's mail request is answered from its own answer window: accepted
/// once the session reports the mail written, refused with the tree's
/// reason or the navigator's, and not attached when its attachment ends
/// first.
#[test]
fn a_childs_mail_request_is_answered_once_the_mail_is_written() {
    let (actions, _intents) =
        plakous_host_protocol::intent_channel::<plakous_host_protocol::Core>();
    let mut stdout = Vec::new();
    let mut adapter = WireConnection::new(root(), &mut stdout, actions);
    adapter.mails.insert("mail-1".into(), 7);
    adapter.mails.insert("mail-2".into(), 8);
    adapter.mails.insert("mail-3".into(), 9);
    adapter.mails.insert("mail-4".into(), 10);
    let refusal = TranscriptRow::error(
        plakous_host_protocol::views::transcript::NoticeScope::Tree,
        "mail refused: recipient is not a message peer",
    );
    for observation in [
        SessionObservation::IntentStarted(Some("mail-2".into())),
        SessionObservation::SetBusy(true),
        SessionObservation::IntentStarted(Some("mail-1".into())),
        SessionObservation::DraftAccepted(SessionIntent::Steer(Draft::from("look"))),
        SessionObservation::IntentStarted(Some("mail-2".into())),
        SessionObservation::Row(refusal),
        SessionObservation::IntentStarted(Some("mail-4".into())),
        SessionObservation::TopologyNavigatorUnavailable {
            reason: "no topology hub is attached".into(),
        },
    ] {
        adapter.mail_answer(&observation);
    }
    adapter.unanswered();
    assert_eq!(
        adapter.settled,
        [
            (7, Outcome::Ok(Answer::Accepted)),
            (
                8,
                Outcome::Refused(RequestRefusal::MailRefused {
                    detail: "mail refused: recipient is not a message peer".into()
                })
            ),
            (
                10,
                Outcome::Refused(RequestRefusal::MailRefused {
                    detail: "no topology hub is attached".into()
                })
            ),
            (9, Outcome::Refused(RequestRefusal::NotAttached)),
        ]
    );
}
