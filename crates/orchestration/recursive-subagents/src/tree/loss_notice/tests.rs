use crate::tree::test_support::*;
use crate::tree::*;
use std::collections::BTreeSet;

/// A root with one child whose turn was running when its host was lost (or,
/// with `handoff`, handed off), as the next host's reopen leaves it.
fn reopened(handoff: bool) -> (tempfile::TempDir, ProductAgentTree, String) {
    let (root, mut tree) = tree();
    let child = tree
        .spawn_agent("root", &spawn_req(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "locator".to_owned())
        .unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
        .unwrap();
    if handoff {
        tree.record_handoff().unwrap();
    } else {
        tree.release_attempt_turn(&child).unwrap();
        tree.park_child(&child, Vec::new()).unwrap();
    }
    let reopened = ProductAgentTree::reopen(tree.to_persisted());
    (root, reopened, child)
}

/// The parent's notice names a child a lost host interrupted, once: the
/// keys it returns, once held, keep the next turn from naming it again.
#[test]
fn a_parent_is_told_once_about_a_child_its_lost_host_interrupted() {
    let (_root, tree, child) = reopened(false);
    let mut noticed = BTreeSet::new();
    let (text, keys) = tree.loss_notice("root", &noticed).unwrap();
    assert!(text.starts_with("\n\n[harness]\n<agent_notice"), "{text}");
    assert!(
        text.contains(&format!(
            "agent_id=\"{child}\" cause=\"owner_lost\" continuation=\"held\""
        )),
        "{text}"
    );
    assert!(
        text.contains("It waits for the operator to resume it."),
        "{text}"
    );
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].0, child);
    noticed.extend(keys);
    assert_eq!(tree.loss_notice("root", &noticed), None);
    // Only the child's own parent is told.
    assert_eq!(tree.loss_notice(&child, &BTreeSet::new()), None);
}

/// A continuation already decided reads as continuing by itself.
#[test]
fn a_continuing_child_is_named_as_continuing() {
    let (_root, mut tree, child) = reopened(false);
    tree.record_loss_continuation(&child, 7).unwrap();
    let (text, _) = tree.loss_notice("root", &BTreeSet::new()).unwrap();
    assert!(text.contains("continuation=\"continuing\""), "{text}");
    assert!(text.contains("It continues by itself"), "{text}");
}

/// A planned handoff loses nothing: the handed-off child continues by
/// itself, and its parent is not told about it.
#[test]
fn a_handed_off_child_is_not_named() {
    let (_root, tree, child) = reopened(true);
    assert_eq!(
        tree.loss_continuation(&child)
            .map(|continuation| continuation.cause),
        Some(ContinuationCause::Handoff)
    );
    assert_eq!(tree.loss_notice("root", &BTreeSet::new()), None);
}

/// What the notice names reads back from a command's text, so a transcript
/// can show it as its own row; anything less than the whole block is not one.
#[test]
fn a_loss_notice_reads_back_from_the_command_it_ends() {
    let (_root, tree, child) = reopened(false);
    let (text, _) = tree.loss_notice("root", &BTreeSet::new()).unwrap();
    let command = format!("next{text}");
    let (before, summary) = split_loss_notice(&command).unwrap();
    assert_eq!(before, "next");
    assert_eq!(summary.named, [(child.clone(), "held".to_owned())]);
    assert_eq!(
        summary.row(),
        format!("a lost host interrupted {child} (held)")
    );
    assert_eq!(split_loss_notice(&command[..command.len() - 1]), None);
    assert_eq!(split_loss_notice("next\n\n[harness]\n"), None);
    assert_eq!(split_loss_notice("next"), None);
}
