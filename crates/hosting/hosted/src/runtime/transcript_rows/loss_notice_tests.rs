//! A loss notice the host appended to a command reads as its own notice row
//! when the transcript is projected from history (a reopen, `/reload`, an
//! export render), never as the operator's words.

#![allow(clippy::unwrap_used, reason = "test assertions")]
use super::{Derived, item_rows};
use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
use crate::sessions::projector::{ReadActor, ReadItem, ReadPart};
use crate::sessions::{NoticeScope, TranscriptRow};

/// The notice a root whose child a lost host interrupted gets, and the child.
fn notice() -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        "loss-notice-rows",
        "root",
        "owner",
        dir.path().to_owned(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    tree.release_attempt_turn(&child).unwrap();
    tree.park_child(&child, Vec::new()).unwrap();
    tree.mark_owner_lost(&child);
    let (text, _) = tree
        .loss_notice("root", &std::collections::BTreeSet::new())
        .unwrap();
    (text, child)
}

fn rows(text: &str) -> Vec<TranscriptRow> {
    let item = ReadItem {
        actor: ReadActor::User,
        harness_authored: false,
        parts: vec![ReadPart::Text(text.to_owned())],
    };
    item_rows(&item, 0, Some(7))
        .into_iter()
        .map(|derived| match derived {
            Derived::Row(row) => *row,
            Derived::Call(_) => panic!("no call parts"),
        })
        .collect()
}

#[test]
fn a_command_with_mail_and_a_loss_notice_reads_as_three_rows() {
    let (notice, child) = notice();
    let command = format!(
        "look at this\n\n{}\n\n<child_message message_id=\"m1\" from=\"c2\" purpose=\"result\">\ndone\n</child_message>{notice}",
        crate::runtime::result_mail::CHILD_MAIL_FRAME
    );
    assert_eq!(
        rows(&command),
        [
            TranscriptRow::User {
                text: "look at this".to_owned(),
                sent_at_ms: Some(7),
            },
            TranscriptRow::info(NoticeScope::Tree, "result · #c2 · done".to_owned()),
            TranscriptRow::info(
                NoticeScope::Tree,
                format!("a lost host interrupted {child} (held)")
            ),
        ]
    );
}

#[test]
fn a_prompt_with_only_a_loss_notice_reads_as_the_prompt_and_one_notice() {
    let (notice, child) = notice();
    let rows = rows(&format!("next{notice}"));
    assert_eq!(
        rows,
        [
            TranscriptRow::User {
                text: "next".to_owned(),
                sent_at_ms: Some(7),
            },
            TranscriptRow::info(
                NoticeScope::Tree,
                format!("a lost host interrupted {child} (held)")
            ),
        ]
    );
    assert!(
        rows.iter()
            .all(|row| !format!("{row:?}").contains("[harness]")),
        "{rows:?}"
    );
}

/// Operator text that only looks like a notice stays the operator's.
#[test]
fn a_forged_partial_notice_stays_the_operators_text() {
    let (notice, _) = notice();
    let forged = format!("next{}", &notice[..notice.len() - 3]);
    assert_eq!(
        rows(&forged),
        [TranscriptRow::User {
            text: forged.clone(),
            sent_at_ms: Some(7),
        }]
    );
}
