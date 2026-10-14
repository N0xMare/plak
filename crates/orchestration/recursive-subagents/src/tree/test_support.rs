//! Shared fixtures for the `agent_tree` unit tests.

pub(super) use crate::protocol::{DEFAULT_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS};

use super::{
    AdoptCandidateRequest, Arc, AttemptScopedRequest, AwaitAgents, BindHints, CandidateId,
    MailRecord, MessagePurpose, PathBuf, ProductAgentTree, RosterBinding, SendAgentMessageRequest,
    SpawnAgentRequest, SpawnWorkspace, SubmitNudgeBudget, Tier, WriterMode,
};

pub(super) use tempfile::tempdir;

pub(super) fn tree_at(root: PathBuf, writer_mode: WriterMode) -> ProductAgentTree {
    ProductAgentTree::bootstrap_root_for_test(
        "tree1",
        "root",
        "owner",
        root,
        "selected-tuple",
        writer_mode,
    )
    .map(|mut tree| {
        tree.pin_bind_hints_for_test(test_bind_hints());
        tree
    })
    .unwrap()
}

pub(super) fn tree() -> (tempfile::TempDir, ProductAgentTree) {
    let root = tempdir().unwrap();
    let tree = tree_at(root.path().to_path_buf(), WriterMode::SingleLease);
    (root, tree)
}

pub(super) use plakous_workspace::occupancy_vcs::VcsOps;
pub(super) use plakous_workspace::occupancy_vcs::fake::FakeVcs;

#[test]
fn completion_and_block_helpers_rebuild_from_recorded_intents() {
    use super::BlockedReason;

    for blocked in [false, true] {
        let (_tmp, mut tree) = tree();
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        if blocked {
            tree.record_attempt_blocked(
                &child,
                BlockedReason {
                    quota: "rate_limited".to_owned(),
                    account: "fixture".to_owned(),
                    resets_at: Some(12345),
                },
            )
            .unwrap();
        } else {
            tree.record_turn_completed(&child).unwrap();
        }
        let expected_attempt = tree.attempt(&child).unwrap().clone();
        let expected_node = tree.node(&child).unwrap().clone();
        tree.clear_feed_views();
        tree.project_feed(
            &child,
            &tree.tree_id.clone(),
            &[],
            &super::TreeFeedReads::default(),
        )
        .unwrap();
        assert_eq!(
            tree.attempt(&child).unwrap(),
            &expected_attempt,
            "blocked={blocked}"
        );
        assert_eq!(tree.node(&child).unwrap(), &expected_node);
        assert!(tree.take_effects().is_empty());
    }
}

/// A test tree over [`FakeVcs`] on a plain temp dir: mints, seals, adopts,
/// hooks, and unmints are recorded rather than run.
pub(super) fn fake_tree() -> (tempfile::TempDir, Arc<FakeVcs>, ProductAgentTree) {
    let root = tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&vcs) as Arc<dyn VcsOps>,
        "tree1",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .map(|mut tree| {
        tree.pin_bind_hints_for_test(test_bind_hints());
        tree
    })
    .unwrap();
    (root, vcs, tree)
}

pub(super) fn spawn_req() -> SpawnAgentRequest {
    SpawnAgentRequest::test_review()
}

/// Pins `name` to an explicit account/tuple/effort with leaf authority.
pub(super) fn pin(
    tree: &mut ProductAgentTree,
    name: &str,
    account: &str,
    tuple: &str,
    effort: Option<&str>,
) {
    let mut binding = RosterBinding::leaf(account, tuple);
    binding.effort = effort.map(str::to_owned);
    binding.max_tier = if name == "implement" {
        Tier::Mutate
    } else {
        Tier::ReadOnly
    };
    if name == "orchestrate" {
        binding.spawns = ["review", "explore", "implement"]
            .map(|leaf| crate::protocol::TemplateName::parse(leaf).unwrap())
            .to_vec();
        binding.may_adopt = true;
    }
    tree.pin_template(name, binding);
}

pub(super) fn close_req(agent_id: &str) -> AttemptScopedRequest {
    AttemptScopedRequest {
        agent_id: agent_id.to_owned(),
        expected_attempt_id: None,
    }
}

pub(super) fn mail(agent_id: &str, message: &str) -> SendAgentMessageRequest {
    SendAgentMessageRequest {
        agent_id: agent_id.to_owned(),
        message: message.to_owned(),
        purpose: None,
        priority: None,
        in_reply_to: None,
        output_schema_json: None,
    }
}

pub(super) fn wait_for(agent_id: &str) -> AwaitAgents {
    AwaitAgents::Named(vec![agent_id.to_owned()])
}

pub(super) fn mint_req() -> SpawnAgentRequest {
    let mut request =
        SpawnAgentRequest::test_named("implement", Tier::Mutate, "implementer", "edit the parser");
    request.workspace = SpawnWorkspace::Fork;
    request
}

// ---- Base, seal, adopt, discard ----

pub(super) fn adopt_req(candidate_id: &CandidateId) -> AdoptCandidateRequest {
    AdoptCandidateRequest {
        candidate_id: candidate_id.clone(),
        expected_target: None,
        evidence: None,
    }
}

pub(super) fn candidate_of(tree: &ProductAgentTree, agent_id: &str) -> CandidateId {
    tree.attempt(agent_id)
        .unwrap_or_else(|| unreachable!("{agent_id} has an attempt"))
        .candidate_id
        .clone()
        .unwrap_or_else(|| unreachable!("{agent_id} sealed a candidate"))
}

// ---- Recovery and persistence tests ----

/// Simulates the cancelled turn's finalize: the seal the turn driver opens
/// once the turn has actually unwound.
pub(super) fn seal_after_turn_exit(tree: &mut ProductAgentTree, agent_id: &str) {
    tree.begin_seal(agent_id);
    tree.settle_for_test();
}

pub(super) fn result_mails_from(tree: &ProductAgentTree, agent_id: &str) -> Vec<MailRecord> {
    tree.messages()
        .values()
        .filter(|mail| mail.purpose == MessagePurpose::Result && mail.from == agent_id)
        .cloned()
        .collect()
}

/// The account roster the tree's own tests (and the product tests that reach
/// the tree through `bootstrap_root_for_test`) pin against. A fixture, not
/// authority: production epochs carry the product's roster in through
/// `install_snapshot`.
pub(super) fn test_bind_hints() -> BindHints {
    BindHints::from_names(
        &[
            "codex",
            "chatgpt",
            "grok",
            "supergrok",
            "xai",
            "xai-oauth",
            "claude",
            "openrouter",
            crate::epoch::RosterInput::TEST_ACCOUNT,
        ],
        &["openrouter"],
    )
}

/// One admitted submit-harness round, for the tests that stage the 1+1
/// budget; `unreachable!` rather than `unwrap` because the ceiling is a
/// constant this fixture is written against.
pub(super) fn one_round() -> SubmitNudgeBudget {
    SubmitNudgeBudget::spent(1).unwrap_or_else(|| unreachable!("1 is within the harness ceiling"))
}

/// Both admitted submit-harness rounds: the budget at its ceiling.
pub(super) fn two_rounds() -> SubmitNudgeBudget {
    SubmitNudgeBudget::spent(SubmitNudgeBudget::CEILING)
        .unwrap_or_else(|| unreachable!("the ceiling is within the ceiling"))
}
