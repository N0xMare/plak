use super::{
    AbandonedRoot, AbandonedTurnClaim, AbandonedTurnRecoveryFold, AbandonmentCause, CommandId,
    OperationId, abandoned_turn_claim, abandoned_turn_entry_id, abandoned_turn_segment,
    abandonment_cause,
};

const CLAIMS: [AbandonedTurnClaim; 5] = [
    AbandonedTurnClaim::RunningTool,
    AbandonedTurnClaim::ModelRequest,
    AbandonedTurnClaim::CompactionNotApplied,
    AbandonedTurnClaim::NothingStarted,
    AbandonedTurnClaim::ToolRoundCompleted,
];
const CAUSES: [AbandonmentCause; 2] =
    [AbandonmentCause::ProcessLost, AbandonmentCause::Interrupted];
use plakous_model::{Actor, Provenance, TurnId};

#[test]
fn interrupted_tool_evidence_distinguishes_conversation_from_workspace_changes() {
    for evidence in [
        super::abandoned_effect_evidence(),
        super::abandoned_process_evidence(),
    ] {
        let evidence = evidence.expect("abandonment evidence");
        let (_, message): (String, String) =
            serde_json::from_slice(evidence.bytes()).expect("evidence tuple");
        assert!(message.contains("conversation"), "{message}");
        assert!(message.contains("workspace changes remain"), "{message}");
        assert!(!message.contains("nothing from it was kept"), "{message}");
    }
}

fn turn() -> TurnId {
    TurnId::try_from(uuid::Uuid::from_u128(0x5eed)).expect("a turn identity")
}

/// One fold with the operation lists a caller wants to reason about.
fn fold(provider: &[u64], effects: &[u64], unrun_effects: &[u64]) -> AbandonedTurnRecoveryFold {
    fold_with(provider, effects, unrun_effects, &[], &[])
}

fn fold_with(
    provider: &[u64],
    effects: &[u64],
    unrun_effects: &[u64],
    processes: &[u64],
    compaction: &[u64],
) -> AbandonedTurnRecoveryFold {
    AbandonedTurnRecoveryFold {
        turn_id: turn(),
        command_id: CommandId::new("cmd-1").expect("a command identity"),
        root_operation: OperationId(1),
        provider_operations: provider.iter().copied().map(OperationId).collect(),
        settled_operations: Vec::new(),
        uncommitted_effect_operations: Vec::new(),
        effect_operations: effects.iter().copied().map(OperationId).collect(),
        unrun_effect_operations: unrun_effects.iter().copied().map(OperationId).collect(),
        process_operations: processes.iter().copied().map(OperationId).collect(),
        compaction_operations: compaction.iter().copied().map(OperationId).collect(),
        root: AbandonedRoot::Dispatched,
        tool_round_completed: false,
    }
}

/// The strongest applicable claim wins, and the order is not arbitrary.
///
/// A running tool outranks an unobserved model request because it can have
/// changed something outside this session, and a user who has to act on one
/// fact should be handed that one. `NothingStarted` is reachable only when
/// neither uncertainty exists, which today means every outstanding effect
/// was proven never dispatched.
#[test]
fn the_strongest_applicable_claim_is_the_one_reported() {
    assert_eq!(
        abandoned_turn_claim(&fold(&[2], &[], &[])),
        AbandonedTurnClaim::ModelRequest
    );
    assert_eq!(
        abandoned_turn_claim(&fold(&[], &[3], &[])),
        AbandonedTurnClaim::RunningTool
    );
    assert_eq!(
        abandoned_turn_claim(&fold(&[2], &[3], &[])),
        AbandonedTurnClaim::RunningTool,
        "a running tool outranks an unobserved request"
    );
    assert_eq!(
        abandoned_turn_claim(&fold(&[], &[], &[3])),
        AbandonedTurnClaim::NothingStarted
    );
    assert_eq!(
        abandoned_turn_claim(&fold(&[2], &[], &[3])),
        AbandonedTurnClaim::ModelRequest,
        "one effect proven never to have started does not silence an \
         unobserved request"
    );
    assert_eq!(
        abandoned_turn_claim(&fold(&[], &[3], &[4])),
        AbandonedTurnClaim::RunningTool,
        "one effect proven never to have started does not silence another \
         that was running"
    );
    assert_eq!(
        abandoned_turn_claim(&fold_with(&[], &[], &[], &[5], &[])),
        AbandonedTurnClaim::RunningTool,
        "a joined process whose teardown was never proven is a running tool"
    );
    assert_eq!(
        abandoned_turn_claim(&fold_with(&[], &[], &[], &[], &[6, 7])),
        AbandonedTurnClaim::CompactionNotApplied
    );
    assert_eq!(
        abandoned_turn_claim(&fold_with(&[2], &[], &[], &[], &[6, 7])),
        AbandonedTurnClaim::ModelRequest,
        "the turn's own request outranks an unapplied compaction"
    );
    assert_eq!(
        abandoned_turn_claim(&fold_with(&[], &[], &[3], &[], &[6, 7])),
        AbandonedTurnClaim::CompactionNotApplied,
        "an unapplied compaction outranks proven-unrun work"
    );
}

/// The cause is the root's state: a still-dispatched root means the owner
/// was lost; one already terminal means a live owner fenced the turn.
#[test]
fn the_cause_follows_the_root() {
    let mut lost = fold(&[2], &[], &[]);
    assert_eq!(abandonment_cause(&lost), AbandonmentCause::ProcessLost);
    lost.root = AbandonedRoot::AlreadyTerminal;
    assert_eq!(abandonment_cause(&lost), AbandonmentCause::Interrupted);
}

/// The note is spelled the way every wire format can carry it.
///
/// `User` with `Provenance::Instruction` is the product's harness-authored
/// pair. It is not a claim that the operator typed this, and a transcript
/// labels it `harness` on the strength of exactly this pair.
///
/// The previous spelling, `System`, was truthful and unsendable. The
/// Responses route renders a system actor as `role: "system"` inside
/// `input`, and the backend answers `HTTP 400` with
/// `System messages are not allowed`; the Messages route refuses to
/// translate a historical instruction actor at all. Both mean the same
/// thing in practice: the session recovers, and then every turn after it
/// fails, which is worse than having no note. If this pair has to change,
/// the three request builders are what decides what it may change to.
#[test]
fn the_abandonment_note_is_harness_authored_so_every_route_can_send_it() {
    for (claim, cause) in CLAIMS
        .into_iter()
        .flat_map(|claim| CAUSES.map(|cause| (claim, cause)))
    {
        let segment =
            abandoned_turn_segment(turn(), claim, cause).expect("the abandonment segment");
        let items = segment.items();
        assert_eq!(items.len(), 1, "the abandonment leaves exactly one entry");
        let item = &items[0];
        assert_eq!(
            item.actor(),
            Actor::User,
            "a historical instruction actor is untranslatable on the Messages route \
             and refused by the Responses backend"
        );
        assert_eq!(
            item.provenance(),
            Provenance::Instruction,
            "the pair is what marks the entry harness-authored rather than typed"
        );
    }
}

/// Each claim says its own thing and never borrows another's warning.
///
/// The two failure modes this guards are symmetrical. Telling an operator
/// their workspace may have changed, over a command that provably never
/// started, teaches them to disregard the warning that matters. Telling
/// them a request may have been billed, over a turn that never reached a
/// provider, is simply false.
#[test]
fn each_abandonment_claim_says_only_what_is_true_of_it() {
    for cause in CAUSES {
        let text = |claim| {
            let segment = abandoned_turn_segment(turn(), claim, cause).expect("the segment");
            format!("{:?}", segment.items()[0].blocks())
        };

        let running_tool = text(AbandonedTurnClaim::RunningTool);
        assert!(running_tool.contains("changed the"), "{running_tool}");
        assert!(running_tool.contains("Check the"), "{running_tool}");
        assert!(!running_tool.contains("billed"), "{running_tool}");

        let model_request = text(AbandonedTurnClaim::ModelRequest);
        assert!(model_request.contains("billed"), "{model_request}");
        assert!(
            !model_request.contains("workspace"),
            "a turn cut inside its model request must not claim a tool ran: {model_request}"
        );

        let compaction = text(AbandonedTurnClaim::CompactionNotApplied);
        assert!(compaction.contains("was not applied"), "{compaction}");
        assert!(compaction.contains("billed"), "{compaction}");
        assert!(!compaction.contains("workspace"), "{compaction}");

        let nothing_started = text(AbandonedTurnClaim::NothingStarted);
        if cause == AbandonmentCause::ProcessLost {
            assert!(
                nothing_started.contains("the tool its last round asked for never started"),
                "{nothing_started}"
            );
            assert!(
                nothing_started.contains("nothing after that round ran"),
                "{nothing_started}"
            );
            assert!(
                !nothing_started.contains("No result from this turn is part of this history"),
                "earlier rounds may already be in the history: {nothing_started}"
            );
        } else {
            assert!(nothing_started.contains("nothing ran"), "{nothing_started}");
        }
        assert!(
            !nothing_started.contains("workspace"),
            "a turn whose outstanding work provably never started must not warn about the \
             workspace: {nothing_started}"
        );
        assert!(
            !nothing_started.contains("billed"),
            "the nothing-started note must not claim a charge: {nothing_started}"
        );
        let lost = cause == AbandonmentCause::ProcessLost;
        for note in [&running_tool, &model_request, &compaction, &nothing_started] {
            assert_eq!(
                note.contains("process was lost"),
                lost,
                "only a lost process is described as one: {note}"
            );
        }
    }
}

/// The entry identity is a function of the turn, so a retried publication
/// reproduces the same history rather than appending a second note.
#[test]
fn the_abandonment_entry_identity_is_stable_for_one_turn() {
    let first = abandoned_turn_entry_id(turn()).expect("an entry identity");
    let second = abandoned_turn_entry_id(turn()).expect("an entry identity");
    assert_eq!(first, second);
    let other = TurnId::try_from(uuid::Uuid::from_u128(0xfeed)).expect("a turn identity");
    assert_ne!(
        first,
        abandoned_turn_entry_id(other).expect("an entry identity"),
        "two abandoned turns do not share one entry"
    );
}
