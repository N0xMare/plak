//! Real-VCS conflict resolution, durable lineage and unrecorded pin cuts.
#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used, reason = "deterministic real-VCS law fixtures")]

use plakous_recursive_subagents::{
    protocol::{
        AdoptCandidateRequest, CandidateId, ListAgentsRequest, SpawnAgentRequest, Tier,
        TopologyErrorCode, WorkspaceIdRequest, WriterMode,
    },
    tree::{CandidateKind, ProductAgentTree, TreeStore},
};
use plakous_recursive_subagents_store::SqliteTreeStore;
use plakous_workspace::{
    occupancy_vcs::{AdoptFailure, SystemVcs},
    vcs_fixtures::{git_repo, jj_available, jj_repo},
};
use std::{path::Path, process::Command};

struct Fixture {
    repo: tempfile::TempDir,
    tree: ProductAgentTree,
    source: CandidateId,
    child_path: std::path::PathBuf,
}

impl Fixture {
    fn new(jj: bool) -> Self {
        Self::with_binary(jj, false)
    }

    fn with_binary(jj: bool, binary: bool) -> Self {
        let repo = if jj { jj_repo() } else { git_repo() };
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            "conflict-tests",
            "root",
            "owner",
            repo.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named("implement", Tier::Mutate, "worker", "work")
                    .with_fork(),
                None,
            )
            .unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("SEED.txt"), "child\n").unwrap();
        std::fs::write(repo.path().join("SEED.txt"), "parent\n").unwrap();
        if binary {
            std::fs::write(child_path.join("BINARY.dat"), b"child\0binary").unwrap();
            std::fs::write(repo.path().join("BINARY.dat"), b"parent\0binary").unwrap();
        }
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let source = tree
            .attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap();
        Self {
            repo,
            tree,
            source,
            child_path,
        }
    }

    fn conflict(&mut self) -> CandidateId {
        let error = self
            .tree
            .adopt_candidate("root", &request(&self.source, false), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::MergeConflict);
        let candidate = self
            .tree
            .candidates()
            .values()
            .find(|row| row.kind == CandidateKind::Conflict)
            .unwrap();
        assert!(
            error.message.contains(candidate.candidate_id.as_str()),
            "{}",
            error.message
        );
        candidate.candidate_id.clone()
    }

    fn resolver(&mut self, conflict: &CandidateId, resolved: bool) -> CandidateId {
        let mut spawn =
            SpawnAgentRequest::test_named("implement", Tier::Mutate, "resolver", "resolve")
                .with_fork();
        spawn.inputs = Some(vec![conflict.to_string()]);
        let child = self.tree.spawn_agent("root", &spawn, None).unwrap();
        let row = self.tree.workspace(&child.workspace.id).unwrap();
        assert_eq!(
            row.base_commit.as_ref(),
            Some(&self.tree.candidates()[conflict].base_commit)
        );
        assert_eq!(row.input_candidate_id.as_ref(), Some(conflict));
        let path = self.tree.node(&child.agent_id).unwrap().path.clone();
        assert!(
            std::fs::read_to_string(path.join("SEED.txt"))
                .unwrap()
                .contains("<<<<<<<")
        );
        if resolved {
            std::fs::write(path.join("SEED.txt"), "resolved\n").unwrap();
            // Marker examples outside the recorded conflict paths are ordinary work.
            std::fs::write(
                path.join("EXAMPLE.txt"),
                "<<<<<<< example\n>>>>>>> example\n",
            )
            .unwrap();
        }
        self.tree.record_turn_completed(&child.agent_id).unwrap();
        self.tree.settle_for_test();
        let id = self
            .tree
            .attempt(&child.agent_id)
            .unwrap()
            .candidate_id
            .clone()
            .unwrap();
        assert_eq!(
            self.tree.candidates()[&id].resolves.as_ref(),
            Some(&self.source)
        );
        id
    }
}

fn request(id: &CandidateId, accept: bool) -> AdoptCandidateRequest {
    AdoptCandidateRequest {
        revert: false,
        candidate_id: id.clone(),
        accept_conflicts: accept,
        expected_target: None,
        evidence: None,
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn conflicting_adopt_pins_conflict_candidate_and_leaves_target_untouched() {
    let mut f = Fixture::new(false);
    let head = git(f.repo.path(), &["rev-parse", "HEAD"]);
    let index = git(f.repo.path(), &["write-tree"]);
    let conflict = f.conflict();
    let row = &f.tree.candidates()[&conflict];
    assert_eq!(row.source_candidate_id.as_ref(), Some(&f.source));
    assert_eq!(row.conflict_target.as_deref(), Some("root"));
    assert_eq!(row.conflict_paths[0].path, "SEED.txt");
    assert_eq!(
        git(
            f.repo.path(),
            &[
                "rev-parse",
                &format!(
                    "refs/plak/{}/candidates/{conflict}",
                    &plakous_workspace::occupancy_vcs::digest_text("conflict-tests")[..16]
                )
            ]
        ),
        row.commit
    );
    assert_eq!(
        git(f.repo.path(), &["rev-parse", &format!("{}^", row.commit)]),
        row.base_commit
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt")).unwrap(),
        "parent\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.child_path.join("SEED.txt")).unwrap(),
        "child\n"
    );
    assert_eq!(git(f.repo.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(git(f.repo.path(), &["write-tree"]), index);
    let list = f
        .tree
        .list_agents(
            "root",
            &ListAgentsRequest {
                include_completed: Some(true),
                include_self: None,
                include_read: None,
            },
        )
        .unwrap();
    assert!(list.agents.iter().any(|row| {
        row["candidates"].as_array().is_some_and(|candidates| {
            candidates.iter().any(|candidate| {
                candidate["candidate_id"] == conflict.as_str()
                    && candidate["candidate_kind"] == "conflict"
                    && candidate["source_candidate"] == f.source.as_str()
            })
        })
    }));
}

#[test]
fn resolver_lands_resolution_when_target_moved_and_source_is_accepted_once() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let resolver = f.resolver(&conflict, true);
    let other_resolver = f.resolver(&conflict, true);
    std::fs::write(f.repo.path().join("LATER.txt"), "keep later work\n").unwrap();
    f.tree
        .adopt_candidate("root", &request(&resolver, false), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt")).unwrap(),
        "resolved\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("LATER.txt")).unwrap(),
        "keep later work\n"
    );
    assert_eq!(
        f.tree.candidates()[&f.source].adopted_via.as_ref(),
        Some(&resolver)
    );
    assert_eq!(
        f.tree.candidates()[&conflict].adopted_by.as_deref(),
        Some("root")
    );
    let source = &f.tree.candidates()[&f.source];
    assert_eq!(
        f.tree
            .workspace(&source.workspace_id)
            .unwrap()
            .base_commit
            .as_ref(),
        Some(&source.commit)
    );
    for id in [&f.source, &conflict, &resolver, &other_resolver] {
        let error = f
            .tree
            .adopt_candidate("root", &request(id, true), None)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    }
    assert!(
        std::fs::read_to_string(f.repo.path().join("EXAMPLE.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
    assert_lineage_reopens(&mut f);
}

fn assert_lineage_reopens(f: &mut Fixture) {
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteTreeStore::open(&tmp.path().join("tree.sqlite3")).unwrap();
    f.tree
        .acknowledge_revision(store.save_in_custody(&f.tree).unwrap());
    let reopened = ProductAgentTree::reopen(store.load("conflict-tests").unwrap().unwrap());
    assert_eq!(reopened.candidates(), f.tree.candidates());
    assert_eq!(reopened.workspaces(), f.tree.workspaces());
}

#[test]
fn resolver_with_markers_requires_accept_conflicts() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let resolver = f.resolver(&conflict, false);
    let error = f
        .tree
        .adopt_candidate("root", &request(&resolver, false), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    assert!(error.message.contains("unresolved markers"));
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt")).unwrap(),
        "parent\n"
    );
    f.tree
        .adopt_candidate("root", &request(&resolver, true), None)
        .unwrap();
    assert!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
}

#[test]
fn resolver_uses_the_named_conflict_base_even_when_another_row_has_the_same_commit() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let mut alias = f.tree.candidates()[&conflict].clone();
    alias.candidate_id = CandidateId::from_persisted("a-shadow".into());
    alias
        .base_commit
        .clone_from(&f.tree.candidates()[&f.source].base_commit);
    alias
        .base_content
        .clone_from(&f.tree.candidates()[&f.source].base_content);
    assert_ne!(
        alias.base_commit,
        f.tree.candidates()[&conflict].base_commit
    );
    f.tree.insert_candidate_for_test(alias);
    f.resolver(&conflict, true);
}

#[test]
fn resolver_followup_after_acceptance_seals_an_ordinary_delta() {
    use plakous_recursive_subagents::protocol::{MessagePurpose, SendAgentMessageRequest};
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let resolver = f.resolver(&conflict, true);
    let agent = f.tree.candidates()[&resolver].producing_agent_id.clone();
    let path = f.tree.node(&agent).unwrap().path.clone();
    f.tree
        .adopt_candidate("root", &request(&resolver, false), None)
        .unwrap();
    f.tree
        .send_agent_message(
            "root",
            &SendAgentMessageRequest {
                acknowledge_recovery: None,
                refresh: None,
                agent_id: agent.clone(),
                message: "a later ordinary edit".to_owned(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    std::fs::write(path.join("NEXT.txt"), "later delta\n").unwrap();
    f.tree.record_turn_completed(&agent).unwrap();
    f.tree.settle_for_test();
    let next = f
        .tree
        .attempt(&agent)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    assert!(f.tree.candidates()[&next].resolves.is_none());
    f.tree
        .adopt_candidate("root", &request(&next, false), None)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("NEXT.txt")).unwrap(),
        "later delta\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt")).unwrap(),
        "resolved\n"
    );
}

#[test]
fn accept_conflicts_lands_markers_and_marks_source_adopted_via() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let error = f
        .tree
        .adopt_candidate("root", &request(&conflict, false), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    f.tree
        .adopt_candidate("root", &request(&conflict, true), None)
        .unwrap();
    assert!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
    assert_eq!(
        f.tree.candidates()[&f.source].adopted_via.as_ref(),
        Some(&conflict)
    );
    assert!(
        f.tree
            .adopt_candidate("root", &request(&f.source, false), None)
            .unwrap_err()
            .message
            .contains("already adopted via")
    );
}

#[test]
fn accepting_a_later_conflict_preserves_the_resolvers_original_input_lineage() {
    use plakous_recursive_subagents::protocol::{MessagePurpose, SendAgentMessageRequest};
    let mut f = Fixture::new(false);
    let k0 = f.conflict();
    let resolver = f.resolver(&k0, true);
    let agent = f.tree.candidates()[&resolver].producing_agent_id.clone();
    f.tree
        .adopt_candidate("root", &request(&resolver, false), None)
        .unwrap();
    f.tree
        .send_agent_message(
            "root",
            &SendAgentMessageRequest {
                acknowledge_recovery: None,
                refresh: None,
                agent_id: agent.clone(),
                message: "later change".into(),
                purpose: Some(MessagePurpose::Followup),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            None,
        )
        .unwrap();
    let path = f.tree.node(&agent).unwrap().path.clone();
    std::fs::write(path.join("SEED.txt"), "next child\n").unwrap();
    std::fs::write(f.repo.path().join("SEED.txt"), "next parent\n").unwrap();
    f.tree.record_turn_completed(&agent).unwrap();
    f.tree.settle_for_test();
    let next = f
        .tree
        .attempt(&agent)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    assert!(
        f.tree
            .adopt_candidate("root", &request(&next, false), None)
            .is_err()
    );
    let k1 = f
        .tree
        .candidates()
        .values()
        .find(|row| row.source_candidate_id.as_ref() == Some(&next))
        .unwrap()
        .candidate_id
        .clone();
    f.tree
        .adopt_candidate("root", &request(&k1, true), None)
        .unwrap();
    assert_eq!(
        f.tree.candidates()[&k0].adopted_via.as_ref(),
        Some(&resolver)
    );
    assert_eq!(f.tree.candidates()[&next].adopted_via.as_ref(), Some(&k1));
}

#[test]
fn conflict_candidate_refused_for_another_adopter() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let mut other = ProductAgentTree::bootstrap_root_for_test(
        "other-tree",
        "other",
        "owner",
        f.repo.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    other.insert_candidate_for_test(f.tree.candidates()[&f.source].clone());
    other.insert_candidate_for_test(f.tree.candidates()[&conflict].clone());
    let error = other
        .begin_adopt_for_test("other", &request(&conflict, true))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(error.message.contains("belongs to root's checkout"));
}

#[test]
fn retry_overwrites_unrecorded_conflict_pin_and_lineage_reopens() {
    let mut f = Fixture::new(false);
    let tmp = tempfile::tempdir().unwrap();
    let store = SqliteTreeStore::open(&tmp.path().join("tree.sqlite3")).unwrap();
    f.tree
        .acknowledge_revision(store.save_in_custody(&f.tree).unwrap());
    f.tree
        .begin_adopt_for_test("root", &request(&f.source, false))
        .unwrap();
    f.tree
        .acknowledge_revision(store.save_in_custody(&f.tree).unwrap());
    let ticket = f.tree.take_effects().adopts.remove(0);
    let failed =
        plakous_recursive_subagents::tree::block_on_for_test(ticket.land(&SystemVcs)).unwrap_err();
    let AdoptFailure::Conflict {
        candidate: Some(candidate),
        ..
    } = failed
    else {
        panic!("expected pinned textual conflict")
    };
    let first = candidate.snapshot.commit;
    // Nothing records the first pin. The retry reserves the same ordinal.
    f.tree = ProductAgentTree::reopen(store.load("conflict-tests").unwrap().unwrap());
    f.tree.settle_for_test();
    f.tree
        .begin_adopt_for_test("root", &request(&f.source, false))
        .unwrap();
    let retry = f.tree.take_effects().adopts.remove(0);
    let error = f
        .tree
        .begin_adopt_for_test("root", &request(&f.source, false))
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(
        f.tree
            .finish_adopt(
                &retry,
                plakous_recursive_subagents::tree::block_on_for_test(retry.land(&SystemVcs))
            )
            .is_err()
    );
    let conflict = f
        .tree
        .candidates()
        .values()
        .find(|row| row.kind == CandidateKind::Conflict)
        .unwrap()
        .candidate_id
        .clone();
    assert_eq!(f.tree.candidates()[&conflict].commit, first);
    assert!(conflict.as_str().ends_with("-x1"));
    f.tree
        .acknowledge_revision(store.save_in_custody(&f.tree).unwrap());
    let reopened = ProductAgentTree::reopen(store.load("conflict-tests").unwrap().unwrap());
    assert_eq!(reopened.candidates(), f.tree.candidates());
}

#[test]
fn non_textual_conflict_pins_nothing_and_lists_kinds() {
    let mut f = Fixture::with_binary(false, true);
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, false), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    assert!(error.message.contains("SEED.txt (contents)"));
    assert!(error.message.contains("BINARY.dat (binary)"));
    assert_eq!(f.tree.candidates().len(), 1);
    assert!(
        !git(
            f.repo.path(),
            &["for-each-ref", "--format=%(refname)", "refs/plak/"]
        )
        .contains("-x")
    );
    assert_eq!(
        std::fs::read(f.repo.path().join("BINARY.dat")).unwrap(),
        b"parent\0binary"
    );
}

#[test]
fn conflict_accept_after_target_moved_conflicts_without_pinning_another_candidate() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    std::fs::write(f.repo.path().join("SEED.txt"), "third state\n").unwrap();
    let error = f
        .tree
        .adopt_candidate("root", &request(&conflict, true), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    assert_eq!(f.tree.candidates().len(), 2);
    assert_eq!(
        std::fs::read_to_string(f.repo.path().join("SEED.txt")).unwrap(),
        "third state\n"
    );
    assert!(f.tree.candidates()[&f.source].adopted_by.is_none());
}

#[test]
fn a_live_resolver_keeps_its_sources_workspace_referenced() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let mut spawn =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "resolver", "resolve").with_fork();
    spawn.inputs = Some(vec![conflict.to_string()]);
    f.tree.spawn_agent("root", &spawn, None).unwrap();
    let workspace = f.tree.candidates()[&f.source].workspace_id.clone();
    let error = f
        .tree
        .discard_workspace(
            "root",
            &WorkspaceIdRequest {
                workspace_id: workspace,
            },
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceReferenced);
}

#[test]
fn jj_conflicts_remain_plain_and_accept_conflicts_is_explicitly_unavailable() {
    assert!(
        jj_available(),
        "real jj is required for this integration law"
    );
    let mut f = Fixture::new(true);
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, false), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::MergeConflict);
    assert!(
        f.tree
            .candidates()
            .values()
            .all(|row| row.kind == CandidateKind::Work)
    );
    let error = f
        .tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap_err();
    assert!(
        error
            .message
            .contains("conflict candidates are not yet available for jj")
    );
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    // A harmless flag is not a blanket refusal of ordinary clean JJ adoption.
    std::fs::write(f.repo.path().join("SEED.txt"), "seed\n").unwrap();
    f.tree
        .adopt_candidate("root", &request(&f.source, true), None)
        .unwrap();
}

#[test]
fn reverting_a_conflict_or_resolver_restores_source_readoptability_and_both_bases() {
    for resolver in [false, true] {
        let mut f = Fixture::new(false);
        let conflict = f.conflict();
        let accepted = if resolver {
            f.resolver(&conflict, true)
        } else {
            conflict.clone()
        };
        f.tree
            .adopt_candidate("root", &request(&accepted, !resolver), None)
            .unwrap();
        std::fs::write(f.repo.path().join("LATER.txt"), "later edit\n").unwrap();
        let mut revert = request(&accepted, false);
        revert.revert = true;
        f.tree.adopt_candidate("root", &revert, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(f.repo.path().join("SEED.txt")).unwrap(),
            "parent\n"
        );
        assert_eq!(
            std::fs::read_to_string(f.repo.path().join("LATER.txt")).unwrap(),
            "later edit\n"
        );
        for id in [&f.source, &accepted, &conflict] {
            let row = &f.tree.candidates()[id];
            assert!(row.adopted_by.is_none());
            assert!(row.adopted_via.is_none());
        }
        let source = &f.tree.candidates()[&f.source];
        assert_eq!(
            f.tree
                .workspace(&source.workspace_id)
                .unwrap()
                .base_commit
                .as_ref(),
            Some(&source.base_commit)
        );
        let accepted_row = &f.tree.candidates()[&accepted];
        if resolver {
            assert_eq!(
                f.tree
                    .workspace(&accepted_row.workspace_id)
                    .unwrap()
                    .base_commit
                    .as_ref(),
                Some(&accepted_row.base_commit)
            );
        }
        f.tree
            .adopt_candidate("root", &request(&accepted, !resolver), None)
            .unwrap();
        assert_eq!(
            f.tree.candidates()[&f.source].adopted_via.as_ref(),
            Some(&accepted)
        );
    }
}

#[test]
fn jj_revert_is_not_applicable_before_an_adopting_hold_and_on_retry() {
    assert!(jj_available(), "real jj is required for this law");
    let mut f = Fixture::new(true);
    std::fs::write(f.repo.path().join("SEED.txt"), "seed\n").unwrap();
    f.tree
        .adopt_candidate("root", &request(&f.source, false), None)
        .unwrap();
    let mut revert = request(&f.source, false);
    revert.revert = true;
    for _ in 0..2 {
        let error = f.tree.adopt_candidate("root", &revert, None).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
        assert!(error.message.contains("revert is not yet available for jj"));
        assert!(f.tree.take_effects().is_empty());
    }
}

#[test]
fn same_tree_resolver_belongs_to_the_original_adopter() {
    let mut f = Fixture::new(false);
    let conflict = f.conflict();
    let mut snapshot = f.tree.current_epoch().clone();
    snapshot.max_depth = 3;
    snapshot
        .roster
        .get_mut(
            &plakous_recursive_subagents::protocol::TemplateName::parse("orchestrate").unwrap(),
        )
        .unwrap()
        .max_tier = Tier::Mutate;
    f.tree.install_snapshot(snapshot);
    let c2 = f
        .tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named("orchestrate", Tier::Mutate, "other", "work")
                .with_fork(),
            None,
        )
        .unwrap();
    let mut spawn =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "resolver", "resolve").with_fork();
    spawn.inputs = Some(vec![conflict.to_string()]);
    let resolver = f.tree.spawn_agent(&c2.agent_id, &spawn, None).unwrap();
    let path = f.tree.node(&resolver.agent_id).unwrap().path.clone();
    std::fs::write(path.join("SEED.txt"), "resolved\n").unwrap();
    f.tree.record_turn_completed(&resolver.agent_id).unwrap();
    f.tree.settle_for_test();
    let candidate = f
        .tree
        .attempt(&resolver.agent_id)
        .unwrap()
        .candidate_id
        .clone()
        .unwrap();
    let error = f
        .tree
        .adopt_candidate(&c2.agent_id, &request(&candidate, false), None)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::AdoptNotApplicable);
    assert!(error.message.contains("belongs to root's checkout"));
}
