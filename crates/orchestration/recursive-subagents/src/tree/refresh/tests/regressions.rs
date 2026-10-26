use super::*;
use crate::protocol::{WorkspaceIdRequest, WorkspaceStatus};
use crate::tree::test_support::adopt_req;
use std::collections::BTreeMap;

fn applying(f: &mut Fixture) -> RefreshTicket {
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    let answer = f.tree.take_effects().run(f.tree.vcs());
    assert_eq!(f.tree.complete(answer).cleanup_failures, Vec::new());
    f.tree.take_effects().refreshes.remove(0)
}

#[test]
fn rehydrated_live_fork_restores_its_logical_pin_then_refreshes_and_adopts() {
    let mut f = Fixture::new(false);
    f.tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    assert!(!f.path.exists());
    f.tree
        .rehydrate_followup("root", &f.request(None), "followup")
        .unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    assert_eq!(f.pin(), f.base);
    assert_ne!(text(&f.path, &["rev-parse", "HEAD"]), f.base);
    let receipt = f.followup(None, false);
    assert_eq!(receipt.refresh.unwrap().status, RefreshStatus::Refreshed);
    assert!(!f.tree.refresh_pending(&f.workspace));
    f.tree.record_turn_completed(&f.agent).unwrap();
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    let candidate = candidate_of(&f.tree, &f.agent);
    f.tree
        .adopt_candidate("root", &adopt_req(&candidate), None)
        .unwrap();
    assert!(f.repo.path().join("CHILD.txt").exists());
    assert!(f.repo.path().join("PARENT.txt").exists());
}

#[test]
fn adopted_uncommitted_work_is_not_a_false_modify_delete_conflict() {
    let mut f = Fixture::new(false);
    let source = candidate_of(&f.tree, &f.agent);
    f.tree
        .adopt_candidate("root", &adopt_req(&source), None)
        .unwrap();
    std::fs::write(
        f.repo.path().join("CHILD.txt"),
        "parent edited adopted work\n",
    )
    .unwrap();
    std::fs::write(f.path.join("LOCAL.txt"), "still uncommitted\n").unwrap();
    let receipt = f.followup(None, false);
    assert_eq!(receipt.refresh.unwrap().status, RefreshStatus::Refreshed);
    assert_eq!(
        std::fs::read_to_string(f.path.join("CHILD.txt")).unwrap(),
        "parent edited adopted work\n"
    );
    assert_eq!(
        text(&f.path, &["show", "HEAD:CHILD.txt"]),
        "parent edited adopted work"
    );
    assert_eq!(
        git(&f.path, &["diff", "--cached", "--name-only"]),
        Vec::<u8>::new()
    );
    assert!(text(&f.path, &["status", "--porcelain"]).contains("?? LOCAL.txt"));
}

#[test]
fn refresh_keeps_prior_committed_and_uncommitted_child_work_unstaged() {
    let mut f = Fixture::new(false);
    std::fs::write(f.path.join("COMMITTED.txt"), "child commit\n").unwrap();
    git(&f.path, &["add", "COMMITTED.txt"]);
    git(&f.path, &["commit", "-m", "child checkpoint"]);
    assert_eq!(
        f.followup(None, false).refresh.unwrap().status,
        RefreshStatus::Refreshed
    );
    assert_eq!(
        git(&f.path, &["diff", "--cached", "--name-only"]),
        Vec::<u8>::new()
    );
    let status = text(&f.path, &["status", "--porcelain"]);
    assert!(status.contains("?? CHILD.txt"), "{status}");
    assert!(status.contains("?? COMMITTED.txt"), "{status}");
    assert_eq!(
        std::fs::read_to_string(f.path.join("COMMITTED.txt")).unwrap(),
        "child commit\n"
    );
}

#[test]
fn fresh_refresh_preconditions_are_checked_before_any_copy_write() {
    for changed in ["pin", "missing_pin", "branch", "head", "content"] {
        let mut f = Fixture::new(false);
        let ticket = applying(&mut f);
        let pin = format!(
            "refs/plak/{}/bases/{}",
            &super::super::super::digest_text("tree1")[..16],
            f.agent
        );
        match changed {
            "pin" => {
                let Step::Apply(plan) = &ticket.step else {
                    panic!("apply plan")
                };
                git(&f.path, &["update-ref", &pin, &plan.child]);
            }
            "missing_pin" => {
                git(&f.path, &["update-ref", "-d", &pin]);
            }
            "branch" => {
                git(&f.path, &["checkout", "--detach"]);
            }
            "head" => {
                git(
                    &f.path,
                    &["commit", "--allow-empty", "-m", "operator checkpoint"],
                );
            }
            "content" => std::fs::write(f.path.join("CHILD.txt"), "operator work\n").unwrap(),
            _ => unreachable!(),
        }
        let head = git(&f.path, &["rev-parse", "HEAD"]);
        let index = git(&f.path, &["write-tree"]);
        let pins = git(
            &f.path,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/plak",
            ],
        );
        let content = std::fs::read(f.path.join("CHILD.txt")).unwrap();
        let answer = ticket.run(f.tree.vcs());
        f.tree.complete_refresh(ticket, answer).unwrap();
        assert!(!f.tree.refresh_pending(&f.workspace), "{changed}");
        assert!(!f.path.join("PARENT.txt").exists(), "{changed}");
        assert_eq!(git(&f.path, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&f.path, &["write-tree"]), index);
        assert_eq!(
            git(
                &f.path,
                &[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/plak"
                ]
            ),
            pins
        );
        assert_eq!(std::fs::read(f.path.join("CHILD.txt")).unwrap(), content);
        assert_eq!(
            f.tree
                .workspace(&f.workspace)
                .unwrap()
                .base_commit
                .as_deref(),
            Some(f.base.as_str())
        );
    }
}

#[test]
fn recovery_abandons_an_unstarted_refresh_with_an_externally_moved_pin() {
    let mut f = Fixture::new(false);
    let ticket = applying(&mut f);
    let Step::Apply(plan) = &ticket.step else {
        panic!("apply plan")
    };
    let external_pin = plan.child.clone();
    let reference = format!(
        "refs/plak/{}/bases/{}",
        &plan.request.namespace[..16],
        f.agent
    );
    let head = git(&f.path, &["rev-parse", "HEAD"]);
    let index = git(&f.path, &["write-tree"]);
    let content = std::fs::read(f.path.join("CHILD.txt")).unwrap();
    let store = MemoryTreeStore::new();
    f.tree.acknowledge_revision(store.save(&f.tree).unwrap());
    git(&f.path, &["update-ref", &reference, &external_pin]);
    f.tree = ProductAgentTree::reopen(store.load("tree1").unwrap().unwrap());
    assert_eq!(f.tree.settle_for_test().cleanup_failures, Vec::new());
    assert!(!f.tree.refresh_pending(&f.workspace));
    let report = f
        .tree
        .refresh_report(&f.workspace, Some("followup"))
        .unwrap();
    assert_eq!(report.status, RefreshStatus::Skipped);
    assert!(
        report
            .reason
            .unwrap()
            .contains("base pin missing or diverged")
    );
    assert_eq!(git(&f.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&f.path, &["write-tree"]), index);
    assert_eq!(std::fs::read(f.path.join("CHILD.txt")).unwrap(), content);
    assert!(!f.path.join("PARENT.txt").exists());
    assert_eq!(f.pin(), external_pin);
    assert_eq!(
        f.tree
            .workspace(&f.workspace)
            .unwrap()
            .base_commit
            .as_deref(),
        Some(f.base.as_str())
    );
}

#[test]
fn applying_refresh_fences_discard_and_dormant_reclamation() {
    let mut f = Fixture::new(false);
    let _ticket = applying(&mut f);
    let error = f
        .tree
        .discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: f.workspace.clone(),
            },
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert_eq!(
        f.tree.reclaim_dormant(u64::MAX, &BTreeMap::new()).unwrap(),
        Vec::<String>::new()
    );
    assert!(f.tree.take_effects().is_empty());
    assert!(f.path.exists());
}

#[test]
fn applying_refresh_fences_retirement_even_with_a_removed_checkout_view() {
    let mut f = Fixture::new(false);
    let _ticket = applying(&mut f);
    // A removed-copy observation cannot relinquish a separately owned intent.
    f.tree.workspaces.get_mut(&f.workspace).unwrap().status = WorkspaceStatus::CleanRemoved;
    assert_eq!(
        f.tree.prepare_child_retention(u64::MAX).unwrap(),
        Vec::<crate::tree::retention::RetirementTicket>::new()
    );
    assert!(!f.tree.conversation_retired(&f.agent));
}

#[test]
fn prepared_refresh_refuses_a_moved_logical_base_before_publication() {
    let mut f = Fixture::new(false);
    f.tree
        .prepare_followup_refresh("root", &f.request(None), "followup", false)
        .unwrap();
    let ticket = f.tree.take_effects().refreshes.remove(0);
    let answer = ticket.run(f.tree.vcs());
    f.tree.workspaces.get_mut(&f.workspace).unwrap().base_commit = Some("moved-base".into());
    let error = f.tree.complete_refresh(ticket, answer).unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(f.tree.effects.refreshes.is_empty());
    assert!(matches!(
        f.tree.refresh_intent(&f.workspace).unwrap().unwrap().phase,
        Phase::Preparing { .. }
    ));
    assert!(!f.path.join("PARENT.txt").exists());
}

#[test]
fn mailbox_capacity_including_the_start_line_is_checked_before_refresh() {
    for cause in ["records", "bytes"] {
        let mut f = Fixture::new(false);
        let template = f.tree.messages.values().next().unwrap().clone();
        let count = if cause == "records" {
            super::super::super::MAX_MAILBOX_RECORDS
        } else {
            1
        };
        for index in 0..count {
            let id = format!("queued-{index}");
            let mut message = template.clone();
            message.message_id.clone_from(&id);
            message.to.clone_from(&f.agent);
            message.durable = crate::tree::MailDurableState::Accepted;
            message.body = if cause == "bytes" {
                "x".repeat(
                    super::super::super::MAX_QUEUED_MAIL_BYTES - f.request(None).message.len(),
                )
            } else {
                "queued".into()
            };
            f.tree.messages.insert(id.clone(), message);
            f.tree.inbound.entry(f.agent.clone()).or_default().push(id);
        }
        let error = f
            .tree
            .prepare_followup_refresh("root", &f.request(None), "followup", false)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::MailboxFull, "{cause}");
        assert!(!f.tree.refresh_pending(&f.workspace));
        assert!(f.tree.effects.refreshes.is_empty());
        assert!(!f.path.join("PARENT.txt").exists());
        assert_eq!(f.pin(), f.base);
    }
}
