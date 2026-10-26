use super::*;
use crate::{
    CallId, CompactionBlock, ContentBlock, EntryId, Provenance, ReasoningBlock,
    ReasoningVisibility, TextBlock, ToolCallBlock, ToolCallState, ToolCallerLineage,
    ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind, ToolResultPart,
};
use uuid::Uuid;

#[test]
fn million_history_parents_drop_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let mut history = CanonicalHistory::empty().expect("empty history");
            for _ in 0..1_000_000 {
                history = CanonicalHistory {
                    inner: Arc::new(CanonicalHistoryInner {
                        parent: Some(history.inner.clone()),
                        roots: Arc::new([]),
                        entries: None,
                        appended_segment: None,
                        identity: history.inner.identity,
                        segment_count: 0,
                        item_count: 0,
                        segment_retained_bytes: 0,
                        tree_node_count: 0,
                    }),
                };
            }
            drop(history);
        })
        .expect("spawn drop thread")
        .join()
        .expect("history-chain drop thread");
}

fn entry(value: u128, actor: Actor, block: ContentBlock) -> ConversationItem {
    ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(value)).expect("entry ID"),
        actor,
        vec![block],
        if actor == Actor::User {
            Provenance::UserInput
        } else {
            Provenance::ModelOutput
        },
    )
    .expect("item")
}

fn text(value: u128, actor: Actor, content: &str) -> ConversationItem {
    entry(
        value,
        actor,
        ContentBlock::Text(TextBlock::new(content, None).expect("text")),
    )
}

fn complete_exchange(value: u128) -> HistorySegment {
    let call_id = CallId::try_from(Uuid::from_u128(value)).expect("call ID");
    let correlation = ToolCorrelation::default();
    let call = entry(
        value.saturating_add(1),
        Actor::Assistant,
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                call_id,
                ToolName::new("read").expect("tool"),
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                correlation.clone(),
                "{}",
            )
            .expect("call"),
        ),
    );
    let result = entry(
        value.saturating_add(2),
        Actor::Tool,
        ContentBlock::ToolResult(
            ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(
                    TextBlock::new("ok", None).expect("text"),
                )],
                correlation,
            )
            .expect("result"),
        ),
    );
    HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call, result])
        .expect("complete exchange")
}

fn reasoning(value: u128) -> HistorySegment {
    HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![entry(
            value,
            Actor::Assistant,
            ContentBlock::Reasoning(
                ReasoningBlock::new(
                    ReasoningVisibility::Summary,
                    Some("reasoning summary".to_owned()),
                    None,
                )
                .expect("reasoning"),
            ),
        )],
    )
    .expect("reasoning segment")
}

fn compaction(value: u128) -> HistorySegment {
    compaction_summary(value, "summary")
}

fn compaction_summary(value: u128, summary: &str) -> HistorySegment {
    HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![entry(
            value,
            Actor::Assistant,
            ContentBlock::Compaction(CompactionBlock::new(summary).expect("compaction")),
        )],
    )
    .expect("compaction segment")
}

fn assert_versioned_context_plan_identities(plan: &ContextHistoryPlan) {
    for identity in [plan.identity(), plan.disposition().identity()] {
        assert_eq!(identity.domain(), DigestDomain::ContextPlan);
        assert_eq!(identity.version(), 1);
    }
    for group in plan.selected_intent_groups() {
        assert_eq!(group.identity().domain(), DigestDomain::ContextPlan);
        assert_eq!(group.identity().version(), 1);
    }
}

#[test]
fn a_photo_sized_user_image_appends_to_canonical_history() {
    let mut bytes = vec![0x5a; 280_000];
    bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
    let image = crate::ImageBlock::new(
        "image/jpeg",
        crate::MediaSource::bytes(bytes).expect("bytes"),
    )
    .expect("image");
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(1)).expect("entry ID"),
        Actor::User,
        vec![
            ContentBlock::Text(TextBlock::new("what animal?", None).expect("text")),
            ContentBlock::Image(image),
        ],
        Provenance::UserInput,
    )
    .expect("item");
    let segment =
        HistorySegment::new(HistorySegmentKind::RecentIntent, vec![item]).expect("image segment");
    CanonicalHistory::new(Vec::new())
        .expect("history")
        .append(segment)
        .expect("append");
}

#[test]
fn canonical_history_append_shares_prior_segment_payload() {
    let first = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(1, Actor::Assistant, "one")],
    )
    .expect("first segment");
    let history = CanonicalHistory::new(vec![first.clone()]).expect("history");
    let next = history
        .append(
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(2, Actor::User, "two")],
            )
            .expect("second segment"),
        )
        .expect("append");

    assert!(history.segments()[0].shares_storage_with(&next.segments()[0]));
    assert_ne!(history.identity(), next.identity());
}

#[test]
fn persistent_append_work_is_logarithmically_bounded() {
    let mut history = CanonicalHistory::default();
    let mut first = None;
    for value in 1..=1_024_u128 {
        let segment = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(value, Actor::Assistant, "bounded")],
        )
        .expect("segment");
        if first.is_none() {
            first = Some(segment.clone());
        }
        let (next, work) = history.append_with_work(segment).expect("append");
        assert!(work.root_handles_copied <= HISTORY_ITER_STACK_CAPACITY);
        assert!(work.tree_nodes_created <= HISTORY_ITER_STACK_CAPACITY);
        assert!(
            work.entry_nodes_created <= HISTORY_ITER_STACK_CAPACITY.saturating_mul(3),
            "{} entry-index nodes exceeded the logarithmic append bound",
            work.entry_nodes_created
        );
        history = next;
    }

    assert_eq!(history.segments().len(), 1_024);
    assert_eq!(history.item_count(), 1_024);
    assert_eq!(history.inner.roots.len(), 1);
    assert_eq!(history.inner.tree_node_count, 2_047);
    assert!(
        first
            .as_ref()
            .is_some_and(|first| first.shares_storage_with(&history.segments()[0]))
    );
}

#[test]
fn old_history_handle_remains_stable_across_later_commits() {
    let mut history = CanonicalHistory::default();
    for value in 1..=17_u128 {
        history = history
            .append(
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(value, Actor::Assistant, "before")],
                )
                .expect("segment"),
            )
            .expect("append");
    }
    let old = history.clone();
    let old_identity = old.identity();
    let old_bytes = serde_json::to_vec(&old).expect("serialize old history");
    for value in 18..=257_u128 {
        history = history
            .append(
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(value, Actor::Assistant, "after")],
                )
                .expect("segment"),
            )
            .expect("append");
    }

    assert_eq!(old.identity(), old_identity);
    assert_eq!(old.segments().len(), 17);
    assert_eq!(
        serde_json::to_vec(&old).expect("serialize old history"),
        old_bytes
    );
    assert!(old.segments()[0].shares_storage_with(&history.segments()[0]));
    assert!(!old.shares_storage_with(&history));
}

#[test]
fn persistent_history_has_deterministic_wire_bytes_and_identity() {
    let segments = (1..=65_u128)
        .map(|value| {
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(value, Actor::User, "same")],
            )
            .expect("segment")
        })
        .collect::<Vec<_>>();
    let bulk = CanonicalHistory::new(segments.clone()).expect("bulk history");
    let mut appended = CanonicalHistory::default();
    for segment in segments {
        appended = appended.append(segment).expect("incremental append");
    }
    let bytes = serde_json::to_vec(&appended).expect("serialize history");
    let decoded: CanonicalHistory = serde_json::from_slice(&bytes).expect("deserialize history");

    assert_eq!(bulk.identity(), appended.identity());
    assert_eq!(serde_json::to_vec(&bulk).expect("serialize bulk"), bytes);
    assert_eq!(decoded, appended);
    assert!(
        decoded
            .segments()
            .iter()
            .all(|segment| segment.kind() == HistorySegmentKind::RecentIntent)
    );
}

#[test]
fn persistent_entry_index_rejects_duplicates_without_mutating_history() {
    let original = CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(7, Actor::Assistant, "original")],
        )
        .expect("segment"),
    ])
    .expect("history");
    let identity = original.identity();
    let duplicate = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(7, Actor::User, "duplicate")],
    )
    .expect("segment");

    assert_eq!(
        original.append(duplicate),
        Err(HistoryError::DuplicateEntry)
    );
    assert_eq!(original.identity(), identity);
    assert_eq!(original.segments().len(), 1);
}

#[test]
fn projection_clone_shares_history_and_has_stable_identity() {
    let history = CanonicalHistory::new(vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "intent")],
        )
        .expect("segment"),
    ])
    .expect("history");
    let projection = ConversationView::from_shared(history.clone(), WorkingProjection::default());
    let cloned = projection.clone();

    assert!(projection.shares_history_with(&history));
    assert!(cloned.shares_history_with(&history));
    assert_eq!(projection.identity(), cloned.identity());
    assert_eq!(projection.items().len(), 1);
}

#[test]
fn context_plan_empty_and_no_user_history_are_protected_only() {
    let empty = ContextHistoryPlan::new(&ConversationView::default()).expect("empty plan");
    assert_eq!(empty.policy_revision(), CONTEXT_PLANNER_POLICY_REVISION);
    assert_eq!(
        empty.strategy(),
        ContextHistoryStrategy::SemanticSummaryPrefixV1
    );
    assert_eq!(empty.latest_user_intent_position(), None);
    assert_eq!(empty.selected_intent_groups(), []);
    assert_eq!(empty.selected_segments(), []);
    assert!(empty.retained_tail().is_none());
    assert!(empty.summary_replacement_tail().is_none());
    assert_eq!(
        empty.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );

    let no_user = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(10, Actor::Assistant, "orphan output")],
            )
            .expect("conversation"),
            reasoning(20),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&no_user).expect("no-user plan");
    assert_eq!(plan.selected_intent_groups(), []);
    assert_eq!(plan.selected_segments(), []);
    assert_eq!(plan.latest_user_intent_position(), None);
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );
}

#[test]
fn semantic_summary_selects_contiguous_complete_older_intent_groups() {
    let segments = vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "first")],
        )
        .expect("first intent"),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(2, Actor::Assistant, "first answer")],
        )
        .expect("first answer"),
        complete_exchange(100),
        reasoning(200),
        HistorySegment::new(
            HistorySegmentKind::Steering,
            vec![text(300, Actor::User, "steer first")],
        )
        .expect("steering"),
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(4, Actor::User, "second")],
        )
        .expect("second intent"),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(5, Actor::Assistant, "second answer")],
        )
        .expect("second answer"),
        complete_exchange(400),
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(6, Actor::User, "latest")],
        )
        .expect("latest intent"),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(7, Actor::Assistant, "latest answer")],
        )
        .expect("latest answer"),
    ];
    let latest = segments[8].clone();
    let latest_answer = segments[9].clone();
    let history = CanonicalHistory::new(segments.clone()).expect("history");
    let source = ConversationView::from_shared(history.clone(), WorkingProjection::default());
    let plan = ContextHistoryPlan::new(&source).expect("plan");

    assert_eq!(plan.latest_user_intent_position(), Some(8));
    assert_eq!(
        plan.selected_intent_groups()
            .iter()
            .map(|group| (group.start_position(), group.end_position()))
            .collect::<Vec<_>>(),
        vec![(0, 5), (5, 8)]
    );
    assert_eq!(
        plan.selected_segments()
            .iter()
            .map(|segment| segment.position())
            .collect::<Vec<_>>(),
        (0_u32..8).collect::<Vec<_>>()
    );
    for selected in plan.selected_segments() {
        assert_eq!(
            selected.identity(),
            segments[usize::try_from(selected.position()).expect("position")].identity()
        );
    }
    assert_eq!(
        plan.retained_tail()
            .map(|tail| (tail.start_position(), tail.segment_count())),
        Some((8, 2))
    );
    assert_eq!(plan.disposition().selected_group_count(), 2);
    assert_eq!(plan.disposition().selected_segment_count(), 8);
    assert_versioned_context_plan_identities(&plan);
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::EligibleWholeGroups
    );
    assert!(plan.shares_source_history_with(&history));

    let candidate = plan
        .summary_replacement_tail()
        .expect("summary replacement tail");
    let retained = candidate.segments().collect::<Vec<_>>();
    assert_eq!(retained.len(), 2);
    assert!(retained[0].shares_storage_with(&latest));
    assert!(retained[1].shares_storage_with(&latest_answer));
}

#[test]
fn rolling_cut_selects_the_sealed_middle_of_the_active_turn() {
    // One long turn: the intent, the newest exchange (the current round's
    // batch) and steering stay verbatim; the sealed work between them is
    // the foldable prefix.
    let segments = vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "build the feature")],
        )
        .expect("intent"),
        complete_exchange(100),
        reasoning(200),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(3, Actor::Assistant, "halfway note")],
        )
        .expect("mid-turn note"),
        HistorySegment::new(
            HistorySegmentKind::Steering,
            vec![text(4, Actor::User, "also cover the edge case")],
        )
        .expect("mid-turn steering"),
        complete_exchange(500),
        complete_exchange(600),
    ];
    let source = ConversationView::from_shared(
        CanonicalHistory::new(segments.clone()).expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");

    assert_eq!(plan.latest_user_intent_position(), Some(0));
    // The newest exchange is the protected tail start (position 6); the
    // steering at 4 breaks the middle into two selected runs.
    assert_eq!(
        plan.selected_intent_groups()
            .iter()
            .map(|group| (group.start_position(), group.end_position()))
            .collect::<Vec<_>>(),
        vec![(1, 4), (5, 6)]
    );
    assert_eq!(
        plan.selected_segments()
            .iter()
            .map(|segment| segment.position())
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 5]
    );
    assert_eq!(
        plan.retained_tail()
            .map(|tail| (tail.start_position(), tail.segment_count())),
        Some((0, 3))
    );
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::EligibleActiveTurnPrefix
    );
    assert_versioned_context_plan_identities(&plan);

    let cut = SemanticCompactionCut::seal(&source, &plan).expect("rolling cut");
    // The summarizer sees the intent it is folding work for.
    assert_eq!(
        cut.summary_input()
            .history()
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::ToolExchange,
            HistorySegmentKind::ReasoningReplay,
            HistorySegmentKind::Conversation,
            HistorySegmentKind::ToolExchange,
        ]
    );
    let summary = compaction_summary(1_000, "rolled middle");
    let successor = cut.build_successor(&summary).expect("successor");
    assert_eq!(
        successor
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::Compaction,
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::Steering,
            HistorySegmentKind::ToolExchange,
        ]
    );
    assert!(successor.segments()[1].shares_storage_with(&segments[0]));
    assert!(successor.segments()[2].shares_storage_with(&segments[4]));
    assert!(successor.segments()[3].shares_storage_with(&segments[6]));
}

#[test]
fn a_rolled_successor_keeps_folding_on_the_next_pass() {
    let segments = vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "build the feature")],
        )
        .expect("intent"),
        complete_exchange(100),
        HistorySegment::new(
            HistorySegmentKind::Steering,
            vec![text(2, Actor::User, "also cover the edge case")],
        )
        .expect("mid-turn steering"),
        complete_exchange(300),
        complete_exchange(400),
    ];
    let source = ConversationView::from_shared(
        CanonicalHistory::new(segments).expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    let successor = SemanticCompactionCut::seal(&source, &plan)
        .expect("rolling cut")
        .build_successor(&compaction_summary(1_000, "rolled"))
        .expect("successor");
    // [summary, intent, steering, ex]: append the next round and the
    // steering stays protected while the older exchange folds again.
    let replanned_source = ConversationView::from_shared(
        successor
            .append(complete_exchange(700))
            .expect("append next round"),
        WorkingProjection::default(),
    );
    let replanned = ContextHistoryPlan::new(&replanned_source).expect("replanned");
    assert_eq!(replanned.latest_user_intent_position(), Some(1));
    assert_eq!(
        replanned
            .selected_segments()
            .iter()
            .map(|segment| segment.position())
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(
        replanned.disposition().kind(),
        ContextPlannerDispositionKind::EligibleActiveTurnPrefix
    );
}

#[test]
fn rolling_cut_stops_at_the_newest_exchange_and_empty_middles() {
    // Intent plus exactly one exchange: the newest exchange is the whole
    // tail, the middle is empty, and the disposition stays protected.
    let source = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "one round")],
            )
            .expect("intent"),
            complete_exchange(100),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    assert_eq!(plan.selected_segments(), []);
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );

    // No exchange at all inside the turn: only intent and text, nothing
    // foldable even though the turn may be large.
    let text_only = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(2, Actor::User, "chat only")],
            )
            .expect("intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(3, Actor::Assistant, "a long answer")],
            )
            .expect("answer"),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&text_only).expect("plan");
    assert_eq!(plan.selected_segments(), []);
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );
}

#[test]
fn rolling_cut_composes_with_older_intent_groups() {
    let segments = vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "older ask")],
        )
        .expect("older intent"),
        complete_exchange(100),
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(2, Actor::User, "current ask")],
        )
        .expect("latest intent"),
        complete_exchange(300),
        reasoning(400),
        complete_exchange(500),
    ];
    let source = ConversationView::from_shared(
        CanonicalHistory::new(segments).expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");

    // Older group folds whole; inside the active turn only the sealed
    // middle (positions 3..5 minus none-selectable kinds) joins the cut.
    assert_eq!(
        plan.selected_intent_groups()
            .iter()
            .map(|group| (group.start_position(), group.end_position()))
            .collect::<Vec<_>>(),
        vec![(0, 2), (3, 5)]
    );
    assert_eq!(
        plan.selected_segments()
            .iter()
            .map(|segment| segment.position())
            .collect::<Vec<_>>(),
        vec![0, 1, 3, 4]
    );
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::EligibleActiveTurnPrefix
    );

    let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");
    // Whole-group input order is preserved; the latest intent is inserted
    // before the intra-turn middle so the summarizer keeps the task.
    assert_eq!(
        cut.summary_input()
            .history()
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::ToolExchange,
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::ToolExchange,
            HistorySegmentKind::ReasoningReplay,
        ]
    );
    let successor = cut
        .build_successor(&compaction_summary(2_000, "folded"))
        .expect("successor");
    assert_eq!(
        successor
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::Compaction,
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::ToolExchange,
        ]
    );
}

#[test]
fn committed_compaction_is_a_retained_selection_barrier() {
    let segments = vec![
        compaction(1),
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(2, Actor::User, "after summary")],
        )
        .expect("middle intent"),
        complete_exchange(500),
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(3, Actor::User, "latest")],
        )
        .expect("latest intent"),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(4, Actor::Assistant, "latest answer")],
        )
        .expect("latest answer"),
    ];
    let source = ConversationView::from_shared(
        CanonicalHistory::new(segments).expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");

    assert_eq!(
        plan.selected_intent_groups()
            .iter()
            .map(|group| (group.start_position(), group.end_position()))
            .collect::<Vec<_>>(),
        vec![(1, 3)]
    );
    assert_eq!(
        plan.selected_segments()
            .iter()
            .map(|segment| segment.position())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let retained = plan
        .summary_replacement_tail()
        .expect("summary replacement tail")
        .segments()
        .map(HistorySegment::kind)
        .collect::<Vec<_>>();
    assert_eq!(
        retained,
        vec![
            HistorySegmentKind::Compaction,
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::Conversation,
        ]
    );
}

#[test]
fn semantic_compaction_cut_seals_whole_groups_and_builds_shared_successor() {
    let segments = vec![
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "old intent")],
        )
        .expect("old intent"),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(2, Actor::Assistant, "old answer")],
        )
        .expect("old answer"),
        complete_exchange(100),
        reasoning(200),
        HistorySegment::new(
            HistorySegmentKind::Steering,
            vec![text(300, Actor::User, "old steering")],
        )
        .expect("old steering"),
        HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(4, Actor::User, "latest intent")],
        )
        .expect("latest intent"),
        HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(5, Actor::Assistant, "latest answer")],
        )
        .expect("latest answer"),
    ];
    let history = CanonicalHistory::new(segments.clone()).expect("history");
    let source = ConversationView::from_shared(history, WorkingProjection::default());
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    let left = SemanticCompactionCut::seal(&source, &plan).expect("left cut");
    let right = SemanticCompactionCut::seal(&source, &plan).expect("right cut");

    assert_eq!(left, right);
    assert_eq!(left.identity(), right.identity());
    assert_eq!(left.identity().domain(), DigestDomain::ContextPlan);
    assert_eq!(left.identity().version(), SEMANTIC_COMPACTION_CUT_REVISION);
    assert_eq!(left.source_history_identity(), source.history().identity());
    assert_eq!(left.source_projection_identity(), source.identity());
    assert_eq!(left.plan_identity(), plan.identity());
    assert_eq!(left.selection_identity(), plan.disposition().identity());
    assert_eq!(left.policy_revision(), CONTEXT_PLANNER_POLICY_REVISION);
    assert_eq!(left.strategy_revision(), CONTEXT_PLANNER_STRATEGY_REVISION);
    assert_eq!(left.selected_segments().len(), 5);
    assert_eq!(left.summary_input().history().segments().len(), 5);
    assert_eq!(left.summary_input().working().segments(), []);
    assert_eq!(left.retained_segment_count(), 2);
    assert_eq!(
        left.retained_tail_identity(),
        plan.retained_tail().expect("retained tail").identity()
    );
    for (selected, original) in left.selected_segments().iter().zip(&segments[..5]) {
        assert!(selected.shares_storage_with(original));
    }
    for (retained, original) in left.retained_segments().iter().zip(&segments[5..]) {
        assert!(retained.shares_storage_with(original));
    }
    assert_eq!(
        serde_json::to_vec(left.summary_input()).expect("left bytes"),
        serde_json::to_vec(right.summary_input()).expect("right bytes")
    );
    let decoded_source: ConversationView =
        serde_json::from_slice(&serde_json::to_vec(&source).expect("source bytes"))
            .expect("decoded source");
    let decoded_plan = ContextHistoryPlan::new(&decoded_source).expect("decoded plan");
    let decoded_cut =
        SemanticCompactionCut::seal(&decoded_source, &decoded_plan).expect("decoded cut");
    assert_eq!(decoded_cut.identity(), left.identity());
    assert_eq!(
        decoded_cut.retained_tail_identity(),
        left.retained_tail_identity()
    );

    let summary = compaction_summary(1_000, "folded old groups");
    let successor = left.build_successor(&summary).expect("successor");
    assert_eq!(successor.segments().len(), 3);
    assert!(successor.segments()[0].shares_storage_with(&summary));
    assert_eq!(
        successor
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::Compaction,
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::Conversation,
        ]
    );
    for (retained, original) in successor.segments().iter().skip(1).zip(&segments[5..]) {
        assert!(retained.shares_storage_with(original));
    }
}

#[test]
fn semantic_compaction_cut_folds_prior_compaction_exactly_once() {
    let old_summary = compaction_summary(1, "prior summary");
    let old_intent = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(2, Actor::User, "after summary")],
    )
    .expect("old intent");
    let old_tool = complete_exchange(500);
    let latest = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(3, Actor::User, "latest")],
    )
    .expect("latest intent");
    let latest_answer = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(4, Actor::Assistant, "latest answer")],
    )
    .expect("latest answer");
    let segments = vec![
        old_summary.clone(),
        old_intent.clone(),
        old_tool.clone(),
        latest.clone(),
        latest_answer.clone(),
    ];
    let source = ConversationView::from_shared(
        CanonicalHistory::new(segments).expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");

    assert!(
        cut.prior_compaction()
            .expect("prior compaction")
            .shares_storage_with(&old_summary)
    );
    assert_eq!(
        cut.summary_input()
            .history()
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::Compaction,
            HistorySegmentKind::RecentIntent,
            HistorySegmentKind::ToolExchange,
        ]
    );
    assert_eq!(cut.selected_segments().len(), 2);
    assert!(cut.selected_segments()[0].shares_storage_with(&old_intent));
    assert!(cut.selected_segments()[1].shares_storage_with(&old_tool));
    assert_eq!(cut.retained_segments().len(), 2);
    assert!(cut.retained_segments()[0].shares_storage_with(&latest));

    let new_summary = compaction_summary(1_001, "prior plus selected groups");
    let successor = cut.build_successor(&new_summary).expect("successor");
    assert_eq!(
        successor
            .segments()
            .iter()
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .count(),
        1
    );
    assert!(successor.segments()[0].shares_storage_with(&new_summary));
    assert!(successor.segments()[1].shares_storage_with(&latest));
    assert!(successor.segments()[2].shares_storage_with(&latest_answer));
    assert!(
        successor
            .segments()
            .iter()
            .all(|segment| !segment.shares_storage_with(&old_summary))
    );
}

#[test]
fn semantic_compaction_cut_rejects_working_protected_drift_and_invalid_summary() {
    let old = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(1, Actor::User, "old")],
    )
    .expect("old intent");
    let old_answer = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(2, Actor::Assistant, "old answer")],
    )
    .expect("old answer");
    let latest = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(3, Actor::User, "latest")],
    )
    .expect("latest intent");
    let committed =
        CanonicalHistory::new(vec![old.clone(), old_answer.clone()]).expect("committed history");
    let working = WorkingProjection::new(vec![latest.clone()], WorkingProjectionLimits::default())
        .expect("working");
    let working_source = ConversationView::from_shared(committed, working);
    let working_plan = ContextHistoryPlan::new(&working_source).expect("working plan");
    assert_eq!(
        SemanticCompactionCut::seal(&working_source, &working_plan),
        Err(SemanticCompactionCutError::WorkingProjectionNotEmpty)
    );

    let protected = ConversationView::from_shared(
        CanonicalHistory::new(vec![latest.clone()]).expect("protected history"),
        WorkingProjection::default(),
    );
    let protected_plan = ContextHistoryPlan::new(&protected).expect("protected plan");
    assert_eq!(
        SemanticCompactionCut::seal(&protected, &protected_plan),
        Err(SemanticCompactionCutError::NoEligibleSelectedPrefix)
    );

    let source = ConversationView::from_shared(
        CanonicalHistory::new(vec![old.clone(), old_answer, latest.clone()])
            .expect("source history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("source plan");
    assert_eq!(
        SemanticCompactionCut::seal(&protected, &plan),
        Err(SemanticCompactionCutError::SourcePlanDrift)
    );
    let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");
    let ordinary = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(900, Actor::Assistant, "not a summary")],
    )
    .expect("ordinary segment");
    assert_eq!(
        cut.build_successor(&ordinary),
        Err(SemanticCompactionCutError::InvalidSummary)
    );
    assert_eq!(
        cut.build_successor(&compaction_summary(1, "duplicate source ID")),
        Err(SemanticCompactionCutError::InvalidSummary)
    );
}

fn native_prior_summary() -> HistorySegment {
    let binding = crate::CompactionReplayBinding::new(
        crate::ProtocolId::new("test.responses").expect("protocol"),
        None,
        crate::ModelId::new("test-model").expect("model"),
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"]).expect("account"),
    );
    HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![entry(
            90,
            Actor::Assistant,
            ContentBlock::Compaction(
                CompactionBlock::new("native summary")
                    .expect("summary")
                    .with_native_item(
                        r#"{"type":"compaction","encrypted_content":"fixture"}"#.to_owned(),
                    )
                    .with_native_binding(binding),
            ),
        )],
    )
    .expect("native prior")
}

#[test]
fn semantic_cut_preserves_the_native_instruction_prefix_at_every_prior_position() {
    for count in [1, 3, 86] {
        let prefix = (0..count)
            .map(|index| {
                let kind = [
                    HistorySegmentKind::RecentIntent,
                    HistorySegmentKind::Steering,
                    HistorySegmentKind::Conversation,
                ][index % 3];
                HistorySegment::new(
                    kind,
                    vec![text(
                        u128::try_from(index).expect("bounded position") + 1,
                        Actor::User,
                        "retained instruction",
                    )],
                )
                .expect("instruction")
            })
            .collect::<Vec<_>>();
        let prior = native_prior_summary();
        let mut segments = prefix.clone();
        segments.extend([
            prior.clone(),
            complete_exchange(500),
            complete_exchange(600),
        ]);
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("native cut");
        assert_eq!(cut.prior_compaction(), Some(&prior));
        assert_eq!(cut.selected_segments().len(), 1);
        assert!(
            cut.selected_segments()[0].shares_storage_with(&source.history().segments()[count + 1])
        );
        for (retained, original) in cut.retained_segments().iter().zip(&prefix) {
            assert!(retained.shares_storage_with(original));
        }
        let successor = cut
            .build_successor(&compaction_summary(1_001, "folded native summary and work"))
            .expect("successor");
        assert_eq!(
            successor
                .segments()
                .iter()
                .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
                .count(),
            1
        );
        assert_eq!(&successor.segments().to_vec()[1..=count], prefix);
        assert_eq!(
            SemanticCompactionCut::seal(
                &source,
                &ContextHistoryPlan::new(&source).expect("recovery plan")
            )
            .expect("recovery cut"),
            cut
        );
    }
}

#[test]
fn semantic_cut_rejects_non_native_non_intent_and_multiple_prior_compactions() {
    let intent = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(1, Actor::User, "intent")],
    )
    .expect("intent");
    let assistant = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(2, Actor::Assistant, "not retained intent")],
    )
    .expect("assistant");
    for prefix in [
        vec![intent.clone(), compaction_summary(91, "not native")],
        vec![intent.clone(), assistant, native_prior_summary()],
        vec![
            intent.clone(),
            complete_exchange(200),
            native_prior_summary(),
        ],
        vec![
            intent,
            compaction_summary(91, "first"),
            native_prior_summary(),
        ],
    ] {
        let mut segments = prefix;
        segments.extend([complete_exchange(500), complete_exchange(600)]);
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        );
        match ContextHistoryPlan::new(&source) {
            Err(HistoryPlanningError::NoncanonicalCompaction) => {
                assert_eq!(
                    source
                        .history()
                        .segments()
                        .iter()
                        .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
                        .count(),
                    2
                );
            }
            other => assert_eq!(
                SemanticCompactionCut::seal(&source, &other.expect("plan")),
                Err(SemanticCompactionCutError::InvalidSelectedSemantics)
            ),
        }
    }
}

#[test]
fn semantic_compaction_cut_rejects_nonprefix_and_noncanonical_prior_summary() {
    let leading = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(1, Actor::Assistant, "orphan")],
    )
    .expect("leading orphan");
    let old = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(2, Actor::User, "old")],
    )
    .expect("old intent");
    let latest = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(3, Actor::User, "latest")],
    )
    .expect("latest intent");
    // An orphan segment before the first intent is outside every intent
    // group, so the selection cannot fold it: it is retained verbatim and
    // only the complete older intent group is replaced.
    let nonprefix = ConversationView::from_shared(
        CanonicalHistory::new(vec![leading.clone(), old.clone(), latest.clone()])
            .expect("nonprefix history"),
        WorkingProjection::default(),
    );
    let nonprefix_plan = ContextHistoryPlan::new(&nonprefix).expect("nonprefix plan");
    let nonprefix_cut =
        SemanticCompactionCut::seal(&nonprefix, &nonprefix_plan).expect("nonprefix cut");
    assert_eq!(
        nonprefix_cut
            .selected_segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![HistorySegmentKind::RecentIntent]
    );
    let orphan_successor = nonprefix_cut
        .build_successor(&compaction_summary(20, "folded"))
        .expect("orphan successor");
    assert_eq!(
        orphan_successor
            .segments()
            .iter()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>(),
        vec![
            HistorySegmentKind::Compaction,
            HistorySegmentKind::Conversation,
            HistorySegmentKind::RecentIntent,
        ]
    );
    assert!(orphan_successor.segments()[1].shares_storage_with(&leading));

    let invalid_item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(10)).expect("entry ID"),
        Actor::Assistant,
        vec![
            ContentBlock::Compaction(CompactionBlock::new("prior").expect("compaction block")),
            ContentBlock::Text(TextBlock::new("extra", None).expect("text")),
        ],
        Provenance::ModelOutput,
    )
    .expect("invalid compaction item");
    let invalid_prior = HistorySegment::new(HistorySegmentKind::Compaction, vec![invalid_item])
        .expect("loosely valid prior compaction");
    let invalid_source = ConversationView::from_shared(
        CanonicalHistory::new(vec![invalid_prior, old, latest]).expect("invalid source"),
        WorkingProjection::default(),
    );
    let invalid_plan = ContextHistoryPlan::new(&invalid_source).expect("invalid plan");
    assert_eq!(
        SemanticCompactionCut::seal(&invalid_source, &invalid_plan),
        Err(SemanticCompactionCutError::InvalidSelectedSemantics)
    );
}

#[test]
fn large_semantic_compaction_cut_shares_every_selected_and_retained_payload() {
    const OLDER_INTENTS: usize = 2_048;
    let mut segments = Vec::with_capacity(OLDER_INTENTS.saturating_mul(2).saturating_add(2));
    for intent in 0..OLDER_INTENTS {
        let base = u128::try_from(intent)
            .expect("bounded intent position")
            .saturating_mul(10)
            .saturating_add(1);
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(base, Actor::User, "older intent")],
            )
            .expect("older intent"),
        );
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(
                    base.saturating_add(1),
                    Actor::Assistant,
                    "older answer",
                )],
            )
            .expect("older answer"),
        );
    }
    let latest = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(100_000, Actor::User, "latest intent")],
    )
    .expect("latest intent");
    let latest_answer = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(100_001, Actor::Assistant, "latest answer")],
    )
    .expect("latest answer");
    segments.push(latest.clone());
    segments.push(latest_answer.clone());
    let first = segments[0].clone();
    let selected_count = segments.len().saturating_sub(2);
    let middle = segments[selected_count / 2].clone();
    let source = ConversationView::from_shared(
        CanonicalHistory::new(segments).expect("large history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("large plan");
    let cut = SemanticCompactionCut::seal(&source, &plan).expect("large cut");

    assert_eq!(cut.selected_segments().len(), selected_count);
    assert_eq!(
        cut.summary_input().history().segments().len(),
        selected_count
    );
    assert_eq!(cut.retained_segment_count(), 2);
    assert!(cut.selected_segments()[0].shares_storage_with(&first));
    assert!(cut.selected_segments()[selected_count / 2].shares_storage_with(&middle));
    assert!(cut.retained_segments()[0].shares_storage_with(&latest));
    assert!(cut.retained_segments()[1].shares_storage_with(&latest_answer));

    let summary = compaction_summary(200_000, "large folded prefix");
    let successor = cut.build_successor(&summary).expect("successor");
    assert_eq!(successor.segments().len(), 3);
    assert!(successor.segments()[1].shares_storage_with(&latest));
    assert!(successor.segments()[2].shares_storage_with(&latest_answer));
}

#[test]
fn stale_or_multiple_model_visible_compaction_layouts_fail_closed() {
    let late = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "retained intent")],
            )
            .expect("intent"),
            compaction(2),
        ])
        .expect("late history"),
        WorkingProjection::default(),
    );
    ContextHistoryPlan::new(&late).expect("compaction-last successor");

    let multiple = ConversationView::from_shared(
        CanonicalHistory::new(vec![compaction(3), compaction(4)]).expect("multiple history"),
        WorkingProjection::default(),
    );
    assert_eq!(
        ContextHistoryPlan::new(&multiple),
        Err(HistoryPlanningError::NoncanonicalCompaction)
    );

    let middle = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(5, Actor::User, "before compaction")],
            )
            .expect("leading intent"),
            compaction(6),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(7, Actor::User, "after compaction")],
            )
            .expect("trailing intent"),
        ])
        .expect("middle history"),
        WorkingProjection::default(),
    );
    ContextHistoryPlan::new(&middle)
        .expect("native compact successor plus a later user remains plannable");
}

#[test]
fn latest_working_intent_closes_the_last_committed_older_group() {
    let old_intent = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(1, Actor::User, "old")],
    )
    .expect("old intent");
    let old_tool = complete_exchange(600);
    let latest = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(2, Actor::User, "working latest")],
    )
    .expect("latest intent");
    let latest_answer = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(3, Actor::Assistant, "working tail")],
    )
    .expect("working tail");
    let history = CanonicalHistory::new(vec![old_intent, old_tool]).expect("history");
    let working = WorkingProjection::new(
        vec![latest.clone(), latest_answer.clone()],
        WorkingProjectionLimits::default(),
    )
    .expect("working");
    let source = ConversationView::from_shared(history.clone(), working);
    let plan = ContextHistoryPlan::new(&source).expect("plan");

    assert_eq!(plan.latest_user_intent_position(), Some(2));
    assert_eq!(
        plan.selected_intent_groups()
            .iter()
            .map(|group| (group.start_position(), group.end_position()))
            .collect::<Vec<_>>(),
        vec![(0, 2)]
    );
    assert_eq!(
        plan.retained_tail()
            .map(|tail| (tail.start_position(), tail.segment_count())),
        Some((2, 2))
    );
    assert!(plan.shares_source_history_with(&history));
    let retained = plan
        .summary_replacement_tail()
        .expect("summary replacement tail")
        .segments()
        .collect::<Vec<_>>();
    assert!(retained[0].shares_storage_with(&latest));
    assert!(retained[1].shares_storage_with(&latest_answer));
}

#[test]
fn steering_without_user_intent_anchors_a_protected_tail() {
    let source = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(1, Actor::Assistant, "before")],
            )
            .expect("conversation"),
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![text(2, Actor::User, "steer")],
            )
            .expect("steering"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(3, Actor::Assistant, "after")],
            )
            .expect("conversation"),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    assert_eq!(plan.latest_user_intent_position(), None);
    assert_eq!(plan.selected_segments(), []);
    // Nothing is selected, so the retained set is the whole committed
    // history — the steering anchor does not discard the orphan before it.
    assert_eq!(
        plan.retained_tail()
            .map(|tail| (tail.start_position(), tail.segment_count())),
        Some((0, 3))
    );
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );
}

#[test]
fn only_protected_latest_group_has_no_summary_selection() {
    let source = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            compaction(10),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(11, Actor::User, "latest")],
            )
            .expect("latest intent"),
            complete_exchange(700),
            reasoning(800),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    assert_eq!(plan.selected_intent_groups(), []);
    assert_eq!(plan.selected_segments(), []);
    assert!(plan.summary_replacement_tail().is_none());
    assert_eq!(
        plan.retained_tail()
            .map(|tail| (tail.start_position(), tail.segment_count())),
        Some((1, 3))
    );
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );
}

#[test]
fn user_bearing_ordinary_segment_is_the_latest_semantic_intent() {
    let mixed = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![
            text(3, Actor::User, "ordinary user intent"),
            text(4, Actor::Assistant, "same atomic segment"),
        ],
    )
    .expect("mixed intent");
    let source = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "old")],
            )
            .expect("old intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(2, Actor::Assistant, "old answer")],
            )
            .expect("old answer"),
            mixed,
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(5, Actor::Assistant, "new tail")],
            )
            .expect("tail"),
        ])
        .expect("history"),
        WorkingProjection::default(),
    );
    let plan = ContextHistoryPlan::new(&source).expect("plan");
    assert_eq!(plan.latest_user_intent_position(), Some(2));
    assert_eq!(
        plan.selected_segments()
            .iter()
            .map(|segment| segment.position())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        plan.retained_tail().map(|tail| tail.start_position()),
        Some(2)
    );
}

#[test]
fn incomplete_tool_exchange_cannot_enter_a_request_projection() {
    let call_id = CallId::try_from(Uuid::from_u128(7)).expect("call ID");
    let call = entry(
        1,
        Actor::Assistant,
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                call_id,
                ToolName::new("read").expect("tool"),
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                ToolCorrelation::default(),
                "{}",
            )
            .expect("call"),
        ),
    );
    assert_eq!(
        HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call.clone()]),
        Err(HistoryError::IncompleteToolExchange)
    );
    let inferred = ConversationView::from_items(vec![call])
        .expect("test conversation items form one valid segment");
    assert_eq!(
        inferred.validate_model_visible(),
        Err(HistoryError::IncompleteToolExchange)
    );
    assert_eq!(
        ContextHistoryPlan::new(&inferred),
        Err(HistoryPlanningError::IncompleteGroup { position: 0 })
    );

    let result = entry(
        2,
        Actor::Tool,
        ContentBlock::ToolResult(
            ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(
                    TextBlock::new("ok", None).expect("text"),
                )],
                ToolCorrelation::default(),
            )
            .expect("result"),
        ),
    );
    assert!(
        HistorySegment::new(
            HistorySegmentKind::ToolExchange,
            vec![inferred.items()[0].clone(), result,]
        )
        .is_ok()
    );
}

#[test]
fn semantic_prefix_identity_accepts_inside_tool_exchange_item_boundary() {
    let call_id = CallId::try_from(Uuid::from_u128(70)).expect("call ID");
    let correlation = ToolCorrelation::new(
        Some("remote-70".to_owned()),
        None,
        ToolCallerLineage::Direct,
    )
    .expect("correlation");
    let call = entry(
        71,
        Actor::Assistant,
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                call_id,
                ToolName::new("read").expect("tool"),
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                correlation.clone(),
                "{}",
            )
            .expect("call"),
        ),
    );
    let result = entry(
        72,
        Actor::Tool,
        ContentBlock::ToolResult(
            ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(
                    TextBlock::new("ok", None).expect("text"),
                )],
                correlation,
            )
            .expect("result"),
        ),
    );
    let segment = HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call, result])
        .expect("complete tool exchange");
    let minimum_retained = segment
        .canonical_bytes()
        .len()
        .saturating_mul(2)
        .saturating_add(size_of::<HistorySegmentInner>())
        .saturating_add(2_usize.saturating_mul(size_of::<Digest>()));
    assert!(segment.retained_bytes_upper_bound() >= minimum_retained);

    let view = ConversationView::from_shared(
        CanonicalHistory::new(vec![segment]).expect("history"),
        WorkingProjection::default(),
    );
    let empty = view.semantic_prefix_identity(0).expect("empty prefix");
    let after_output = view
        .semantic_prefix_identity(1)
        .expect("complete output-item boundary");
    let full = view.semantic_prefix_identity(2).expect("full prefix");
    assert_ne!(empty.identity(), after_output.identity());
    assert_ne!(after_output.identity(), full.identity());
    assert_eq!(full.identity(), view.identity());
    assert_eq!(after_output.item_count(), 1);
    assert_eq!(
        view.semantic_prefix_identity(3),
        Err(HistoryError::PrefixOutsideProjection)
    );
}

#[test]
fn canonical_item_prefix_ignores_regrouping_but_full_projection_does_not() {
    let first = text(80, Actor::Assistant, "one");
    let second = text(81, Actor::Assistant, "two");
    let grouped = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![first.clone(), second.clone()],
            )
            .expect("grouped segment"),
        ])
        .expect("grouped history"),
        WorkingProjection::default(),
    );
    let split = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            HistorySegment::new(HistorySegmentKind::Conversation, vec![first])
                .expect("first split segment"),
            HistorySegment::new(HistorySegmentKind::Conversation, vec![second])
                .expect("second split segment"),
        ])
        .expect("split history"),
        WorkingProjection::default(),
    );

    assert_ne!(grouped.identity(), split.identity());
    assert_ne!(
        grouped.semantic_prefix_identity(2).expect("grouped prefix"),
        split.semantic_prefix_identity(2).expect("split prefix")
    );
    let grouped_items = grouped
        .canonical_item_prefix_identity(2)
        .expect("grouped item prefix");
    assert_eq!(
        grouped_items,
        split
            .canonical_item_prefix_identity(2)
            .expect("split item prefix")
    );
    assert_eq!(
        grouped_items.identity().domain(),
        DigestDomain::CanonicalItemPrefix
    );
    assert_eq!(
        grouped_items.identity().version(),
        CANONICAL_ITEM_PREFIX_DIGEST_REVISION
    );
}

#[test]
fn staged_item_prefix_matches_later_complete_tool_exchange() {
    let intent = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(90, Actor::User, "use the tool")],
    )
    .expect("intent");
    let base = ConversationView::from_shared(
        CanonicalHistory::new(vec![intent.clone()]).expect("base history"),
        WorkingProjection::default(),
    );
    let call_id = CallId::try_from(Uuid::from_u128(91)).expect("call ID");
    let correlation = ToolCorrelation::new(
        Some("remote-91".to_owned()),
        None,
        ToolCallerLineage::Direct,
    )
    .expect("correlation");
    let call = entry(
        92,
        Actor::Assistant,
        ContentBlock::ToolCall(
            ToolCallBlock::new(
                call_id,
                ToolName::new("read").expect("tool"),
                ToolExecutionOwner::Local,
                ToolCallState::Requested,
                correlation.clone(),
                "{}",
            )
            .expect("call"),
        ),
    );
    let staged = base
        .staged_canonical_item_prefix_identity(slice::from_ref(&call))
        .expect("staged prefix");
    let result = entry(
        93,
        Actor::Tool,
        ContentBlock::ToolResult(
            ToolResultBlock::new(
                call_id,
                ToolExecutionOwner::Local,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(
                    TextBlock::new("ok", None).expect("text"),
                )],
                correlation,
            )
            .expect("result"),
        ),
    );
    let complete = ConversationView::from_shared(
        CanonicalHistory::new(vec![
            intent,
            HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call.clone(), result])
                .expect("complete exchange"),
        ])
        .expect("complete history"),
        WorkingProjection::default(),
    );

    assert_eq!(
        staged,
        complete
            .canonical_item_prefix_identity(2)
            .expect("committed prefix")
    );
    assert_eq!(
        base.staged_canonical_item_prefix_identity(&[call.clone(), call]),
        Err(HistoryError::DuplicateEntry)
    );
}

#[test]
fn working_projection_limits_are_enforced_without_flattening() {
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(1, Actor::Assistant, "one")],
    )
    .expect("segment");
    let limits = WorkingProjectionLimits::new(1, 1, usize::MAX).expect("limits");
    assert!(WorkingProjection::new(vec![segment.clone()], limits).is_ok());
    assert!(matches!(
        WorkingProjection::new(vec![segment.clone(), segment], limits),
        Err(HistoryError::LimitExceeded {
            field: "working projection segments",
            maximum: 1,
        })
    ));
}

#[test]
fn context_plan_bound_accepts_maximum_and_rejects_one_more_group() {
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(1, Actor::Assistant, "bounded")],
    )
    .expect("segment");
    let limits = WorkingProjectionLimits::new(
        MAX_CONTEXT_PLANNER_SEGMENTS.saturating_add(1),
        MAX_CONTEXT_PLANNER_SEGMENTS.saturating_add(1),
        usize::MAX,
    )
    .expect("limits");
    let maximum = ConversationView::from_shared(
        CanonicalHistory::default(),
        WorkingProjection::new(vec![segment.clone(); MAX_CONTEXT_PLANNER_SEGMENTS], limits)
            .expect("maximum working projection"),
    );
    let plan = ContextHistoryPlan::new(&maximum).expect("maximum plan");
    assert_eq!(
        plan.disposition().retained_segment_count(),
        u32::try_from(MAX_CONTEXT_PLANNER_SEGMENTS).expect("bounded count")
    );
    assert_eq!(
        plan.disposition().kind(),
        ContextPlannerDispositionKind::ProtectedOnly
    );

    let excessive = ConversationView::from_shared(
        CanonicalHistory::default(),
        WorkingProjection::new(
            vec![segment; MAX_CONTEXT_PLANNER_SEGMENTS.saturating_add(1)],
            limits,
        )
        .expect("excessive working projection"),
    );
    assert_eq!(
        ContextHistoryPlan::new(&excessive),
        Err(HistoryPlanningError::LimitExceeded)
    );
}

#[test]
fn long_plan_keeps_source_and_retained_payloads_structurally_shared() {
    let first = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(1, Actor::User, "old intent")],
    )
    .expect("first intent");
    let mut history = CanonicalHistory::new(vec![first]).expect("history");
    for value in 2..=2_049_u128 {
        history = history
            .append(
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(value, Actor::Assistant, "shared payload")],
                )
                .expect("conversation"),
            )
            .expect("append");
    }
    let latest = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(3_000, Actor::User, "latest")],
    )
    .expect("latest intent");
    history = history.append(latest.clone()).expect("append latest");
    let source = ConversationView::from_shared(history.clone(), WorkingProjection::default());
    let left = ContextHistoryPlan::new(&source).expect("left plan");
    let right = ContextHistoryPlan::new(&source).expect("right plan");

    assert!(left.shares_source_history_with(&history));
    assert_eq!(left.identity(), right.identity());
    assert_eq!(
        left.disposition().identity(),
        right.disposition().identity()
    );
    assert_eq!(left.selected_intent_groups().len(), 1);
    assert_eq!(left.selected_segments().len(), 2_049);
    let retained = left
        .summary_replacement_tail()
        .expect("summary replacement tail")
        .segments()
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 1);
    assert!(retained[0].shares_storage_with(&latest));
}

#[test]
fn canonical_decode_preserves_segment_and_item_identities() {
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![
            text(1, Actor::Assistant, "canonical item"),
            text(2, Actor::Assistant, "escaped \"item\"\n"),
        ],
    )
    .expect("canonical segment");
    let decoded =
        HistorySegment::from_canonical(segment.canonical_bytes()).expect("canonical segment");
    assert_eq!(decoded, segment);
    let mut padded = segment.canonical_bytes().to_vec();
    padded.push(b' ');
    assert!(HistorySegment::from_canonical(&padded).is_err());
}

fn media_segment(bytes: Vec<u8>) -> HistorySegment {
    let source = crate::MediaSource::bytes(bytes).expect("media");
    HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![
            entry(
                1,
                Actor::User,
                ContentBlock::Image(
                    crate::ImageBlock::new("image/png", source.clone()).expect("image"),
                ),
            ),
            entry(
                2,
                Actor::User,
                ContentBlock::Document(
                    crate::DocumentBlock::new("application/pdf", source.clone()).expect("document"),
                ),
            ),
            entry(
                3,
                Actor::User,
                ContentBlock::Audio(crate::AudioBlock::new("audio/wav", source).expect("audio")),
            ),
        ],
    )
    .expect("media segment")
}

fn auxiliary_segment(content: &str) -> HistorySegment {
    let replay = crate::ReplayEnvelope::new(
        crate::ProtocolId::new("test.protocol").expect("protocol"),
        None,
        crate::ReplayScope::Protocol,
        crate::ReplayKindId::new("test.replay").expect("kind"),
        1,
        crate::DataClassification::Public,
        serde_json::value::RawValue::from_string(serde_json::to_string(content).expect("payload"))
            .expect("raw payload"),
    )
    .expect("replay");
    let citation = crate::CitationBlock::new(
        crate::CitationTarget::new(EntryId::try_from(Uuid::from_u128(1)).expect("id"), 0),
        crate::CitationSource::new("source", "annotation", None, Some(content.to_owned()))
            .expect("source"),
        Some(
            crate::CitationRange::new(0, content.len() as u64, crate::CitationUnit::Utf8Byte)
                .expect("range"),
        ),
    );
    HistorySegment::new(
        HistorySegmentKind::ReasoningReplay,
        vec![
            entry(
                1,
                Actor::Assistant,
                ContentBlock::Opaque(
                    crate::OpaqueBlock::new(
                        replay,
                        crate::OpaqueFallback::SemanticSummary(content.to_owned()),
                    )
                    .expect("opaque"),
                ),
            ),
            entry(2, Actor::Assistant, ContentBlock::Citation(citation)),
        ],
    )
    .expect("auxiliary segment")
}

fn exchange_with_update() -> HistorySegment {
    let mut items = complete_exchange(1).items().to_vec();
    items.insert(
        1,
        entry(
            4,
            Actor::Tool,
            ContentBlock::ToolResultUpdate(
                crate::ToolResultUpdateBlock::new(
                    CallId::try_from(Uuid::from_u128(1)).expect("call"),
                    ToolExecutionOwner::Local,
                    TextBlock::new("progress", None).expect("text"),
                    ToolCorrelation::default(),
                )
                .expect("update"),
            ),
        ),
    );
    HistorySegment::new(HistorySegmentKind::ToolExchange, items).expect("exchange with update")
}

proptest::proptest! {
    #[test]
    fn canonical_segments_roundtrip_content_and_projection_identities(
        content in ".{1,128}", bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 1..64),
        variant in 0_u8..8,
    ) {
        let segment = match variant {
            0 => HistorySegment::new(HistorySegmentKind::Conversation, vec![text(1, Actor::Assistant, &content)]).expect("text"),
            1 => complete_exchange(1),
            2 => reasoning(1),
            3 => compaction_summary(1, &content),
            4 => media_segment(bytes),
            6 => auxiliary_segment(&content),
            7 => exchange_with_update(),
            _ => HistorySegment::new(HistorySegmentKind::Conversation, vec![entry(1, Actor::Assistant,
                ContentBlock::Refusal(crate::RefusalBlock::new(content).expect("refusal")))]).expect("refusal segment"),
        };
        let decoded = HistorySegment::from_canonical(segment.canonical_bytes()).expect("decode");
        proptest::prop_assert_eq!(&decoded, &segment);
        for (item, identity) in segment.items().iter().zip(decoded.inner.item_identities.iter()) {
            proptest::prop_assert_eq!(projection_item_identity(item).expect("item identity"), *identity);
        }
    }
}

#[test]
fn canonical_segment_refuses_alternate_json_encodings() {
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![text(1, Actor::User, "A/\u{e9}\u{1f680}")],
    )
    .expect("segment");
    let canonical = std::str::from_utf8(segment.canonical_bytes()).expect("utf8");
    let value: serde_json::Value = serde_json::from_str(canonical).expect("value");
    let items = serde_json::to_string(&value["items"]).expect("items");
    let kind = serde_json::to_string(&value["kind"]).expect("kind");
    for alternative in [
        canonical.replace("A/", r"\u0041/"),
        canonical.replace("A/", r"A\/"),
        canonical.replace('\u{1f680}', r"\ud83d\ude80"),
        format!("{{\"kind\":{kind},\"items\":{items}}}"),
        format!("{{\"items\":{items},\"kind\":{kind},\"kind\":{kind}}}"),
        format!("{{\"items\":{items},\"kind\":{kind},\"unknown\":0}}"),
        canonical.replace(
            "\"actor\":\"User\"",
            "\"actor\":\"User\",\"actor\":\"User\"",
        ),
        canonical.replacen("{\"actor\":", "{\"unknown\":0,\"actor\":", 1),
        format!(
            "{{\"items\":[{{\"blocks\":{},\"actor\":{},\"id\":{},\"provenance\":{}}}],\"kind\":{kind}}}",
            value["items"][0]["blocks"],
            value["items"][0]["actor"],
            value["items"][0]["id"],
            value["items"][0]["provenance"]
        ),
        format!("{{\"items\":[],\"kind\":{kind}}}"),
    ] {
        assert_ne!(alternative.as_bytes(), segment.canonical_bytes());
        assert!(
            HistorySegment::from_canonical(alternative.as_bytes()).is_err(),
            "{alternative}"
        );
    }
    let decoded = HistorySegment::from_canonical(segment.canonical_bytes()).expect("unicode");
    assert_eq!(
        decoded.inner.item_identities[0],
        projection_item_identity(&segment.items()[0]).expect("identity")
    );
    // Envelope refusals deliberately use the integrity class, even for garbage.
    assert!(matches!(
        HistorySegment::from_canonical(b"garbage"),
        Err(HistoryError::IntegrityMismatch { .. })
    ));
}

#[test]
fn canonical_media_refuses_noncanonical_base64_text() {
    let segment = media_segment(vec![1, 0, 255]);
    let canonical = std::str::from_utf8(segment.canonical_bytes()).expect("utf8");
    assert!(canonical.contains(r#""Bytes":"AQD/""#), "{canonical}");
    for encoded in [
        "[1,0,255]",
        r#""AQD_""#,
        r#""AQD""#,
        r#""AQD/=""#,
        r#""AQD\/""#,
        r#""AQD\u002f""#,
        r#""AQD/\n""#,
    ] {
        let alternative = canonical.replacen(r#""AQD/""#, encoded, 1);
        assert_ne!(alternative, canonical);
        assert!(
            HistorySegment::from_canonical(alternative.as_bytes()).is_err(),
            "{encoded}"
        );
    }
    let decoded = HistorySegment::from_canonical(segment.canonical_bytes()).expect("media decode");
    for (item, identity) in segment
        .items()
        .iter()
        .zip(decoded.inner.item_identities.iter())
    {
        assert_eq!(projection_item_identity(item).expect("identity"), *identity);
    }
}

#[test]
fn tiny_image_segment_canonical_bytes_and_identity_are_pinned() {
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![entry(
            1,
            Actor::User,
            ContentBlock::Image(
                crate::ImageBlock::new(
                    "image/png",
                    crate::MediaSource::bytes(b"\x89PNG\r\n\x1a\n".to_vec()).expect("media"),
                )
                .expect("image"),
            ),
        )],
    )
    .expect("image segment");
    assert_eq!(
        std::str::from_utf8(segment.canonical_bytes()).expect("utf8"),
        concat!(
            r#"{"items":[{"actor":"User","blocks":[{"Image":{"detail":null,"#,
            r#""media_type":"image/png","source":{"kind":{"Bytes":"iVBORw0KGgo="}}}}],"#,
            r#""id":"00000000-0000-0000-0000-000000000001","provenance":"UserInput"}],"#,
            r#""kind":"Conversation"}"#
        )
    );
    assert_eq!(
        segment.identity().to_string(),
        "62de671de0cf73ddb665577d9edd9a647023eb982eb3e50be92910638354444d"
    );
    assert_eq!(
        HistorySegment::from_canonical(segment.canonical_bytes())
            .expect("decode")
            .identity(),
        segment.identity()
    );
}

#[test]
fn segment_bytes_and_identity_are_deterministic() {
    let left = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(1, Actor::User, "same")],
    )
    .expect("left");
    let right = HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text(1, Actor::User, "same")],
    )
    .expect("right");
    assert_eq!(left.canonical_bytes(), right.canonical_bytes());
    assert_eq!(left.identity(), right.identity());
    assert!(!left.shares_storage_with(&right));
}
