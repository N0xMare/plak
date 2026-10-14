#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::collections::{BTreeMap, BTreeSet};

use plakous::agent::{AbandonedTurnClaim, AbandonmentCause, AgentErrorKind, CommandId, TurnUsage};
use plakous::model::{
    BranchId, CallId, CheckpointId, FinishReason, HistorySegmentKind, SessionId,
    SessionWorkflowRootId, ToolName, TurnId, TurnOrigin,
};
use plakous::sessions::SessionLocator;
use plakous::sessions::feed::{
    FeedBranchTransition, FeedCallerLineage, FeedCancelCause, FeedCommandKind, FeedCompactPass,
    FeedCompactReason, FeedFact, FeedHeader, FeedLocator, FeedOperationKind, FeedOperationState,
    FeedRecord, FeedSeq, FeedTurnOutcome, FeedWorkflowKind, FeedWorkflowOutcome,
};
use proptest::prelude::*;

use super::*;
use crate::sessions::{NoticeScope, ProductCompactReason, SessionWorkflowKind, TranscriptRow};

fn uuid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

fn id<T: std::str::FromStr>(n: u64) -> T
where
    T::Err: std::fmt::Debug,
{
    uuid(n).parse().unwrap()
}

/// A synthetic session feed with every read its records name.
struct Feed {
    session: SessionId,
    branch: BranchId,
    seq: u64,
    next_id: u64,
    head: CheckpointId,
    ordinal: u64,
    records: Vec<FeedRecord>,
    reads: ReadCache,
    intents: ProductIntents,
}

/// What one record's facts need read.
#[derive(Default)]
struct Reads {
    items: Vec<(usize, Option<Vec<ReadItem>>)>,
    calls: Vec<(CallTarget, CallRead)>,
    terminals: Vec<(TurnId, TerminalRead)>,
}

impl Feed {
    fn new() -> Self {
        Self::new_for(id(1))
    }

    fn new_for(session: SessionId) -> Self {
        let mut feed = Self {
            session,
            branch: id(2),
            seq: 0,
            next_id: 100,
            head: id(3),
            ordinal: 0,
            records: Vec::new(),
            reads: ReadCache::default(),
            intents: ProductIntents::default(),
        };
        let genesis = feed.head;
        feed.push(
            feed.branch,
            serde_json::json!({"created": {"genesis": genesis.to_string(), "revision": 1}}),
            genesis,
            vec![FeedFact::SessionCreated {
                operation_id: "plak:create:s1".to_owned(),
                seeded: false,
            }],
            Reads::default(),
        );
        feed
    }

    fn fresh(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn push(
        &mut self,
        branch: BranchId,
        change: serde_json::Value,
        checkpoint: CheckpointId,
        facts: Vec<FeedFact>,
        reads: Reads,
    ) {
        self.seq += 1;
        let header: FeedHeader = serde_json::from_value(serde_json::json!({
            "session_id": self.session.to_string(),
            "seq": self.seq,
            "branch_id": branch.to_string(),
            "committed_at_ms": 1_000 * self.seq,
            "change": change,
        }))
        .unwrap();
        let record = FeedRecord::from_store(header, facts);
        for (fact_index, items) in reads.items {
            let FeedFact::HistoryAppended {
                first_ordinal,
                count,
                first_entry,
                last_entry,
                ..
            } = &record.facts()[fact_index]
            else {
                panic!("items read for a non-history fact");
            };
            let mut rest = items;
            for request in reads::item_reads(
                branch,
                checkpoint,
                *first_ordinal,
                *count,
                *first_entry,
                *last_entry,
            ) {
                let ReadRequest::Items { count, .. } = request else {
                    unreachable!("item reads are items");
                };
                let chunk = rest.as_mut().map(|rest| {
                    let tail = rest.split_off((count as usize).min(rest.len()));
                    std::mem::replace(rest, tail)
                });
                self.reads.insert(request, ReadValue::Items(chunk));
            }
        }
        for (target, call) in reads.calls {
            self.reads.insert(
                ReadRequest::Call {
                    branch,
                    checkpoint,
                    target,
                },
                ReadValue::Call(call),
            );
        }
        for (turn_id, terminal) in reads.terminals {
            self.reads.insert(
                ReadRequest::Terminal {
                    branch,
                    checkpoint,
                    turn_id,
                },
                ReadValue::Terminal(terminal),
            );
        }
        for request in plan_reads(&record) {
            assert!(
                self.reads.read(&request).is_some(),
                "the corpus provides every planned read: {request:?}"
            );
        }
        self.records.push(record);
    }

    /// One commit on `branch` advancing its head.
    fn advance(&mut self, facts: Vec<FeedFact>, reads: Reads) {
        let branch = self.branch;
        self.advance_on(branch, facts, reads);
    }

    fn advance_on(&mut self, branch: BranchId, facts: Vec<FeedFact>, reads: Reads) {
        let parent = self.head;
        let checkpoint = id(self.fresh());
        self.head = checkpoint;
        let revision = self.seq + 1;
        self.push(
            branch,
            serde_json::json!({"advanced": {
                "parent": parent.to_string(),
                "checkpoint": checkpoint.to_string(),
                "revision": revision,
                "head": "quiescent",
            }}),
            checkpoint,
            facts,
            reads,
        );
    }

    fn history(
        &mut self,
        turn_id: Option<TurnId>,
        segment: HistorySegmentKind,
        items: Vec<ReadItem>,
        tool_results: Vec<CallId>,
    ) -> (FeedFact, Option<Vec<ReadItem>>) {
        let count = items.len().max(1) as u64;
        let first_ordinal = self.ordinal;
        self.ordinal += count;
        (
            FeedFact::HistoryAppended {
                turn_id,
                segment,
                first_ordinal,
                count,
                first_entry: id(self.fresh()),
                last_entry: id(self.fresh()),
                tool_results,
            },
            Some(items),
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum End {
    Completed,
    Failed,
    ContextCapacity,
    Interrupted,
    Abandoned,
    RecoveryRequired,
}

#[derive(Clone, Debug)]
enum Step {
    Turn {
        host_wake: bool,
        submitted: bool,
        tools: u8,
        nested_duplicate: bool,
        steer: bool,
        end: End,
    },
    Compact {
        compacted: bool,
        scheduled: bool,
    },
    Rejected,
    Release,
    Process {
        terminal: bool,
    },
}

fn text_item(actor: ReadActor, text: &str) -> ReadItem {
    ReadItem {
        actor,
        harness_authored: false,
        parts: vec![ReadPart::Text(text.to_owned())],
    }
}

fn effect(
    operation_id: u64,
    turn_id: TurnId,
    call_id: CallId,
    lineage: FeedCallerLineage,
    state: FeedOperationState,
) -> FeedFact {
    FeedFact::OperationAdvanced {
        operation_id,
        turn_id: Some(turn_id),
        kind: FeedOperationKind::Effect {
            call_id,
            exposed_name: ToolName::new("bash").unwrap(),
            lineage,
            subject: Some("ls -la".to_owned()),
        },
        state,
    }
}

fn apply_step(feed: &mut Feed, step: &Step) {
    match *step {
        Step::Turn {
            host_wake,
            submitted,
            tools,
            nested_duplicate,
            steer,
            end,
        } => turn(
            feed,
            host_wake,
            submitted,
            tools,
            nested_duplicate,
            steer,
            end,
        ),
        Step::Compact {
            compacted,
            scheduled,
        } => {
            let n = feed.fresh();
            let workflow_id =
                SessionWorkflowRootId::new([u8::try_from(n % 250).unwrap() + 1; 32]).unwrap();
            let operation_id = format!("plak:compact:{n}");
            if scheduled {
                feed.intents.workflow_requests.push(WorkflowRequest {
                    operation_id: operation_id.clone(),
                    kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
                    source_locator: SessionLocator::new(feed.session, feed.branch, feed.head),
                });
            }
            let kind = FeedWorkflowKind::Compact {
                reason: FeedCompactReason::Endpoint,
            };
            feed.advance(
                vec![FeedFact::WorkflowAdmitted {
                    workflow_id,
                    operation_id: Some(operation_id.clone()),
                    kind,
                }],
                Reads::default(),
            );
            let locator = FeedLocator {
                session_id: feed.session,
                branch_id: feed.branch,
                checkpoint_id: feed.head,
            };
            let outcome = if compacted {
                FeedWorkflowOutcome::Compacted {
                    source: locator,
                    target: locator,
                    pass: FeedCompactPass::Summary,
                    selected_segments: 3,
                    tokens_before: 199_700,
                    tokens_after: 12_400,
                }
            } else {
                FeedWorkflowOutcome::NoEligibleRange
            };
            let (summary, _) = feed.history(
                None,
                HistorySegmentKind::Compaction,
                vec![ReadItem {
                    actor: ReadActor::Other,
                    harness_authored: true,
                    parts: vec![ReadPart::Compaction],
                }],
                Vec::new(),
            );
            feed.advance(
                vec![
                    summary,
                    FeedFact::WorkflowSettled {
                        workflow_id,
                        operation_id: Some(operation_id),
                        kind,
                        outcome,
                    },
                ],
                Reads::default(),
            );
        }
        Step::Rejected => {
            let n = feed.fresh();
            feed.advance(
                vec![FeedFact::CommandRejected {
                    command_id: CommandId::new(format!("cmd-{n}")).unwrap(),
                    kind: FeedCommandKind::Root {
                        origin: TurnOrigin::UserPrompt,
                    },
                    error: plakous::agent::ControlErrorKind::StoragePressure,
                }],
                Reads::default(),
            );
        }
        Step::Release => {
            let head = feed.head;
            let branch = feed.branch;
            feed.push(
                branch,
                serde_json::json!({"lease_released": {"head": head.to_string(), "fence": 7}}),
                head,
                Vec::new(),
                Reads::default(),
            );
        }
        Step::Process { terminal } => {
            let operation_id = feed.fresh();
            let fact = |state| FeedFact::OperationAdvanced {
                operation_id,
                turn_id: None,
                kind: FeedOperationKind::ProcessSession {
                    job_number: Some(7),
                },
                state,
            };
            feed.advance(vec![fact(FeedOperationState::Dispatched)], Reads::default());
            if terminal {
                feed.advance(
                    vec![fact(FeedOperationState::Completed)],
                    Reads {
                        calls: vec![(
                            CallTarget::Operation(operation_id),
                            CallRead::Job {
                                number: Some(7),
                                command: Some("echo done".to_owned()),
                                end: Some(JobEnd {
                                    stopped: false,
                                    exit_code: Some(0),
                                }),
                            },
                        )],
                        ..Reads::default()
                    },
                );
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools,
    reason = "one scripted turn"
)]
fn turn(
    feed: &mut Feed,
    host_wake: bool,
    submitted: bool,
    tools: u8,
    nested_duplicate: bool,
    steer: bool,
    end: End,
) {
    let n = feed.fresh();
    let turn_id: TurnId = id(n);
    let command_id = format!("cmd-{n}");
    if submitted && !host_wake {
        feed.intents.submissions.push(Submission {
            command_id: command_id.clone(),
            kind: SubmissionKind::RootStart,
            queue_item_id: format!("item-{n}"),
            queue_item_revision: 1,
            target_turn_id: None,
            canonical_command: format!("prompt {n}"),
            submitted_at_ms: 40 + n,
        });
    }
    let (prompt, prompt_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::Conversation,
        vec![text_item(ReadActor::User, &format!("prompt {n}"))],
        Vec::new(),
    );
    feed.advance(
        vec![
            FeedFact::CommandAccepted {
                command_id: CommandId::new(command_id.clone()).unwrap(),
                kind: FeedCommandKind::Root {
                    origin: if host_wake {
                        TurnOrigin::HostWake
                    } else {
                        TurnOrigin::UserPrompt
                    },
                },
                turn_id: Some(turn_id),
                sequence: n,
                operation_id: None,
            },
            prompt,
        ],
        Reads {
            items: vec![(1, prompt_items)],
            ..Reads::default()
        },
    );
    let calls: Vec<(u64, CallId)> = (0..tools)
        .map(|_| {
            let call = feed.fresh();
            (feed.fresh(), id(call))
        })
        .collect();
    let mut parts = vec![
        ReadPart::Reasoning(format!("thinking {n}")),
        ReadPart::Text(format!("answer {n}")),
    ];
    let mut reads = Reads::default();
    for (operation, call) in &calls {
        parts.push(ReadPart::Call {
            call_id: call.to_string(),
            canonical: "process_session_start".to_owned(),
            exposed: "bash".to_owned(),
            arguments: Some(r#"{"command":"ls -la"}"#.to_owned()),
            parent_call_id: None,
            cell_id: None,
            subject: None,
        });
        reads.calls.push((
            CallTarget::Operation(*operation),
            CallRead::Tool {
                canonical: "process_session_start".to_owned(),
                exposed: "bash".to_owned(),
                arguments: Some(r#"{"command":"ls -la"}"#.to_owned()),
                result: None,
            },
        ));
    }
    let (response, response_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::Conversation,
        vec![ReadItem {
            actor: ReadActor::Assistant,
            harness_authored: false,
            parts,
        }],
        Vec::new(),
    );
    reads.items.push((0, response_items));
    let mut facts = vec![response];
    for (operation, call) in &calls {
        facts.push(effect(
            *operation,
            turn_id,
            *call,
            FeedCallerLineage::Direct,
            FeedOperationState::Dispatched,
        ));
    }
    // A Code Cell nested call shows twice: its edge and its execution
    // effect, under one call id.
    let nested = nested_duplicate.then(|| {
        let call: CallId = id(feed.fresh());
        let cell_operation = feed.fresh();
        let edge = feed.fresh();
        let execution = feed.fresh();
        (call, cell_operation, edge, execution)
    });
    if let Some((call, cell_operation, edge, execution)) = nested {
        let lineage = FeedCallerLineage::LocalProgrammatic {
            evaluation_operation_id: cell_operation,
            parent_call_id: None,
            cell_id: "cell-1".to_owned(),
            request_sequence: 1,
            depth: 1,
        };
        for operation in [edge, execution] {
            facts.push(effect(
                operation,
                turn_id,
                call,
                lineage.clone(),
                FeedOperationState::Dispatched,
            ));
            reads.calls.push((
                CallTarget::Operation(operation),
                CallRead::Nested {
                    name: "read_file".to_owned(),
                    arguments: Some(r#"{"path":"a.rs"}"#.to_owned()),
                },
            ));
        }
    }
    feed.advance(facts, reads);
    if steer {
        let operation = feed.fresh();
        let mut reads = Reads::default();
        reads.calls.push((
            CallTarget::Operation(operation),
            CallRead::Steer(format!("steer {n}")),
        ));
        feed.advance(
            vec![FeedFact::CommandAccepted {
                command_id: CommandId::new(format!("steer-{n}")).unwrap(),
                kind: FeedCommandKind::Steer,
                turn_id: Some(turn_id),
                sequence: n + 1,
                operation_id: Some(operation),
            }],
            reads,
        );
    }
    let finishes_tools = matches!(end, End::Completed | End::Failed);
    let mut reads = Reads::default();
    let mut facts = Vec::new();
    if finishes_tools && !calls.is_empty() {
        let (results, items) = feed.history(
            Some(turn_id),
            HistorySegmentKind::ToolExchange,
            vec![ReadItem {
                actor: ReadActor::Other,
                harness_authored: false,
                parts: Vec::new(),
            }],
            calls.iter().map(|(_, call)| *call).collect(),
        );
        facts.push(results);
        reads.items.push((0, items));
        for (index, (operation, call)) in calls.iter().enumerate() {
            let tool = CallRead::Tool {
                canonical: "process_session_start".to_owned(),
                exposed: "bash".to_owned(),
                arguments: Some(r#"{"command":"ls -la"}"#.to_owned()),
                result: Some(ResultRead {
                    succeeded: index % 2 == 0,
                    text: format!("output {index}"),
                }),
            };
            reads.calls.push((CallTarget::Call(*call), tool.clone()));
            reads.calls.push((CallTarget::Operation(*operation), tool));
            facts.push(effect(
                *operation,
                turn_id,
                *call,
                FeedCallerLineage::Direct,
                FeedOperationState::Completed,
            ));
        }
    }
    if let Some((call, cell_operation, edge, execution)) = nested {
        let lineage = FeedCallerLineage::LocalProgrammatic {
            evaluation_operation_id: cell_operation,
            parent_call_id: None,
            cell_id: "cell-1".to_owned(),
            request_sequence: 1,
            depth: 1,
        };
        for operation in [edge, execution] {
            facts.push(effect(
                operation,
                turn_id,
                call,
                lineage.clone(),
                FeedOperationState::Completed,
            ));
            reads.calls.push((
                CallTarget::Operation(operation),
                CallRead::Nested {
                    name: "read_file".to_owned(),
                    arguments: Some(r#"{"path":"a.rs"}"#.to_owned()),
                },
            ));
        }
    }
    if matches!(end, End::Completed) {
        let (answer, items) = feed.history(
            Some(turn_id),
            HistorySegmentKind::Conversation,
            vec![text_item(ReadActor::Assistant, &format!("done {n}"))],
            Vec::new(),
        );
        reads.items.push((facts.len(), items));
        facts.push(answer);
    }
    let outcome = match end {
        End::Completed => FeedTurnOutcome::Completed {
            finish: FinishReason::Stop,
        },
        End::Failed => {
            reads.terminals.push((
                turn_id,
                TerminalRead::Failed(format!("agent turn failed: boom {n}")),
            ));
            FeedTurnOutcome::Failed {
                kind: AgentErrorKind::Model,
                model_kind: None,
                context: None,
            }
        }
        End::ContextCapacity => {
            reads.terminals.push((
                turn_id,
                TerminalRead::Failed("context capacity exceeded".to_owned()),
            ));
            FeedTurnOutcome::Failed {
                kind: AgentErrorKind::Model,
                model_kind: None,
                context: Some(
                    serde_json::from_value(serde_json::json!({
                        "withheld": false,
                        "remedy": "compact",
                    }))
                    .unwrap(),
                ),
            }
        }
        End::Interrupted => FeedTurnOutcome::Cancelled {
            cause: FeedCancelCause::Requested,
        },
        End::Abandoned => FeedTurnOutcome::Cancelled {
            cause: FeedCancelCause::Abandoned {
                claim: AbandonedTurnClaim::ModelRequest,
                cause: AbandonmentCause::ProcessLost,
                model_round_completed: false,
                tool_round_completed: false,
            },
        },
        End::RecoveryRequired => FeedTurnOutcome::RecoveryRequired,
    };
    facts.push(FeedFact::TurnTerminal {
        turn_id,
        command_id: CommandId::new(command_id).unwrap(),
        outcome,
        closed_exchange: true,
        steering_settled_unapplied: false,
        usage: TurnUsage::default(),
    });
    feed.advance(facts, reads);
}

fn feed_of(steps: &[Step]) -> Feed {
    let mut feed = Feed::new();
    for step in steps {
        apply_step(&mut feed, step);
    }
    feed
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (any::<bool>(), any::<bool>(), 0_u8..4, any::<bool>(), any::<bool>(), 0_u8..5).prop_map(
            |(host_wake, submitted, tools, nested_duplicate, steer, end)| Step::Turn {
                host_wake,
                submitted,
                tools,
                nested_duplicate,
                steer,
                end: match end {
                    0 => End::Completed,
                    1 => End::Failed,
                    2 => End::Interrupted,
                    3 => End::Abandoned,
                    _ => End::RecoveryRequired,
                },
            }
        ),
        2 => (any::<bool>(), any::<bool>())
            .prop_map(|(compacted, scheduled)| Step::Compact { compacted, scheduled }),
        1 => Just(Step::Rejected),
        1 => Just(Step::Release),
        1 => any::<bool>().prop_map(|terminal| Step::Process { terminal }),
    ]
}

fn project_all(feed: &Feed) -> Projection {
    project(
        &SessionViews::empty(),
        &feed.intents,
        &feed.records,
        &feed.reads,
    )
    .unwrap()
}

#[test]
fn terminal_usage_is_fact_derived_and_identical_on_rebuild_not_replay() {
    let mut feed = Feed::new();
    let usage = TurnUsage::new(plakous::model::Usage::new(123, 45, 6, 7, 8), 2, 1, 0).unwrap();
    feed.advance(
        vec![FeedFact::TurnTerminal {
            turn_id: id(91),
            command_id: CommandId::new("usage-root").unwrap(),
            outcome: FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Abandoned {
                    claim: AbandonedTurnClaim::ModelRequest,
                    cause: AbandonmentCause::ProcessLost,
                    model_round_completed: false,
                    tool_round_completed: false,
                },
            },
            closed_exchange: false,
            steering_settled_unapplied: false,
            usage,
        }],
        Reads::default(),
    );
    let live = project_all(&feed);
    assert_eq!(live.terminal_usages.len(), 1);
    assert_eq!(live.terminal_usages[0].usage, usage);
    assert_eq!(live.terminal_usages[0].turn_id, id(91));
    assert_eq!(live.terminal_usages[0].committed_at_ms, 2_000);
    assert_eq!(project_all(&feed).terminal_usages, live.terminal_usages);
    let replay = project(&live.views, &feed.intents, &feed.records, &feed.reads).unwrap();
    assert_eq!(replay.terminal_usages, [] as [ProjectedTurnUsage; 0]);
}

/// Applies `records` page by page, each page replaying up to `replay`
/// records before it, and folds every page's writes into a stored row map
/// as the store would.
fn project_paged(
    feed: &Feed,
    pages: &[usize],
    replay: usize,
) -> (SessionViews, BTreeMap<RowKey, TranscriptRow>) {
    let mut views = SessionViews::empty();
    let mut stored = BTreeMap::new();
    let mut stored_calls = BTreeMap::new();
    let mut stored_turns = BTreeMap::new();
    let mut stored_processes = BTreeMap::new();
    let mut start = 0;
    let mut sizes = pages.iter().copied().cycle();
    while start < feed.records.len() {
        let size = sizes.next().unwrap_or(1).max(1);
        let end = (start + size).min(feed.records.len());
        let from = start.saturating_sub(replay);
        let projection =
            project(&views, &feed.intents, &feed.records[from..end], &feed.reads).unwrap();
        assert_eq!(projection.writes.expected_cursor, views.cursor);
        for key in &projection.writes.deletes {
            stored.remove(key);
        }
        for (key, row) in &projection.writes.upserts {
            stored.insert(*key, row.clone());
        }
        apply_keyed(&mut stored_calls, &projection.writes.calls);
        apply_keyed(&mut stored_turns, &projection.writes.open_turns);
        apply_keyed(&mut stored_processes, &projection.writes.processes);
        assert_eq!(stored_calls, projection.views.state.calls);
        assert_eq!(stored_turns, projection.views.state.open_turns);
        assert_eq!(stored_processes, projection.views.state.processes);
        views = projection.views;
        start = end;
    }
    (views, stored)
}

fn apply_keyed<K: Clone + Ord, V: Clone>(stored: &mut BTreeMap<K, V>, changes: &[(K, Option<V>)]) {
    for (key, value) in changes {
        if let Some(value) = value {
            stored.insert(key.clone(), value.clone());
        } else {
            stored.remove(key);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rebuild_from_empty_equals_every_paging_and_replay(
        steps in prop::collection::vec(step(), 1..7),
        pages in prop::collection::vec(1_usize..6, 1..4),
        replay in 0_usize..4,
    ) {
        let feed = feed_of(&steps);
        let all = project_all(&feed);
        let (paged, stored) = project_paged(&feed, &pages, replay);
        prop_assert_eq!(&paged, &all.views);
        prop_assert_eq!(&stored, &all.views.rows);
        // Every record boundary, one record per page.
        let (single, single_stored) = project_paged(&feed, &[1], 0);
        prop_assert_eq!(&single, &all.views);
        prop_assert_eq!(&single_stored, &all.views.rows);
    }

    #[test]
    fn a_replayed_feed_changes_nothing(steps in prop::collection::vec(step(), 1..7)) {
        let feed = feed_of(&steps);
        let all = project_all(&feed);
        let again = project(&all.views, &feed.intents, &feed.records, &feed.reads).unwrap();
        prop_assert_eq!(&again.views, &all.views);
        prop_assert!(again.writes.is_empty());
        prop_assert!(again.writes.successors.is_empty());
    }

    #[test]
    fn deleting_the_views_and_rebuilding_is_identical(steps in prop::collection::vec(step(), 1..7)) {
        let feed = feed_of(&steps);
        let first = project_all(&feed);
        let rebuilt = project(&SessionViews::empty(), &feed.intents, &feed.records, &feed.reads).unwrap();
        prop_assert_eq!(&rebuilt.views, &first.views);
        prop_assert_eq!(
            rebuilt.views.transcript(&feed.intents),
            first.views.transcript(&feed.intents)
        );
    }

    #[test]
    fn every_call_id_has_one_row_and_counts_once(steps in prop::collection::vec(step(), 1..7)) {
        let feed = feed_of(&steps);
        let views = project_all(&feed).views;
        let mut ids = BTreeSet::new();
        for row in views.rows.values() {
            if let TranscriptRow::Tool(tool) = row && tool.name != "compact" {
                prop_assert!(ids.insert(tool.call_id.clone()), "duplicate row for {}", tool.call_id);
            }
        }
        prop_assert_eq!(views.state.counters.tool_calls, ids.len() as u64);
    }

    #[test]
    fn a_successor_rebuild_equals_its_independently_projected_prefix_and_branch(
        before in prop::collection::vec(step(), 1..5),
        after in prop::collection::vec(step(), 0..4),
        rebind in any::<bool>(),
    ) {
        let mut feed = feed_of(&before);
        let source = SessionLocator::new(feed.session, feed.branch, feed.head);
        let target_branch: BranchId = id(feed.fresh());
        let target: CheckpointId = id(feed.fresh());
        let workflow_id = SessionWorkflowRootId::new([9; 32]).unwrap();
        let operation_id = if rebind { "plak:rebind:1" } else { "plak:fork:1" }.to_owned();
        let transition = if rebind { FeedBranchTransition::Rebind } else { FeedBranchTransition::Fork };
        let kind = if rebind { FeedWorkflowKind::Rebind } else { FeedWorkflowKind::Fork };
        let target_locator = FeedLocator {
            session_id: feed.session,
            branch_id: target_branch,
            checkpoint_id: target,
        };
        feed.push(
            target_branch,
            serde_json::json!({"branched": {
                "source": {
                    "session_id": source.session_id().to_string(),
                    "branch_id": source.branch_id().to_string(),
                    "checkpoint_id": source.checkpoint_id().to_string(),
                },
                "target": target.to_string(),
                "revision": 4,
                "transition": if rebind { "rebind" } else { "fork" },
            }}),
            target,
            vec![
                FeedFact::WorkflowAdmitted {
                    workflow_id,
                    operation_id: Some(operation_id.clone()),
                    kind,
                },
                FeedFact::WorkflowSettled {
                    workflow_id,
                    operation_id: Some(operation_id.clone()),
                    kind,
                    outcome: FeedWorkflowOutcome::Published { target: target_locator },
                },
            ],
            Reads::default(),
        );
        let branched_at = feed.seq;
        // Work continues on the successor's branch only.
        feed.branch = target_branch;
        feed.head = target;
        for step in &after {
            apply_step(&mut feed, step);
        }
        let source_run = project_all(&feed);
        prop_assert_eq!(source_run.writes.successors.len(), 1);
        let successor = &source_run.writes.successors[0];
        prop_assert_eq!(successor.operation_id.as_deref(), Some(operation_id.as_str()));
        prop_assert_eq!(successor.transition, transition);
        prop_assert_eq!(successor.published_seq, FeedSeq::new(branched_at));
        prop_assert_eq!(successor.target, target_locator);
        prop_assert!(source_run.views.state.workflows[&operation_id].settled.is_some());
        let initial = SessionViews::empty_with_lineage(vec![
            LineageStep { branch_id: source.branch_id(), from_seq: 0 },
            LineageStep { branch_id: target_branch, from_seq: branched_at },
        ]);
        let prefix = project(
            &initial,
            &feed.intents,
            &feed.records.iter().filter(|record| record.header().seq.get() <= branched_at).cloned().collect::<Vec<_>>(),
            &feed.reads,
        ).unwrap().views;
        prop_assert_eq!(prefix.cursor, FeedSeq::new(branched_at));
        prop_assert_eq!(prefix.state.head, Some(target_locator));
        let continued = project(
            &prefix,
            &feed.intents,
            &feed.records,
            &feed.reads,
        )
        .unwrap()
        .views;
        let rebuilt = project(
            &initial,
            &feed.intents,
            &feed.records,
            &feed.reads,
        )
        .unwrap()
        .views;
        prop_assert_eq!(&rebuilt, &continued);
        // The source applies nothing of the successor's branch.
        let source_rows = project_all(&feed_of(&before)).views.rows;
        prop_assert_eq!(&source_run.views.rows, &source_rows);
    }
}

fn single_turn(end: End, submitted: bool, steer: bool) -> (Feed, SessionViews) {
    let feed = feed_of(&[Step::Turn {
        host_wake: false,
        submitted,
        tools: 1,
        nested_duplicate: false,
        steer,
        end,
    }]);
    let views = project_all(&feed).views;
    (feed, views)
}

fn rows(views: &SessionViews) -> Vec<&TranscriptRow> {
    views.rows.values().collect()
}

#[test]
fn the_user_row_takes_its_send_time_from_the_submission_only() {
    let (feed, views) = single_turn(End::Completed, true, false);
    let submitted = feed.intents.submissions[0].submitted_at_ms;
    assert!(rows(&views).iter().any(|row| matches!(
        row,
        TranscriptRow::User { sent_at_ms: Some(at), .. } if *at == submitted
    )));
    let (_, views) = single_turn(End::Completed, false, false);
    assert!(rows(&views).iter().any(|row| matches!(
        row,
        TranscriptRow::User {
            sent_at_ms: None,
            ..
        }
    )));
}

#[test]
fn the_steer_row_reads_its_text_and_stamps_its_acceptance() {
    let (feed, views) = single_turn(End::Completed, true, true);
    let accepted = feed
        .records
        .iter()
        .find(|record| {
            record.facts().iter().any(|fact| {
                matches!(
                    fact,
                    FeedFact::CommandAccepted {
                        kind: FeedCommandKind::Steer,
                        ..
                    }
                )
            })
        })
        .unwrap()
        .header()
        .committed_at_ms;
    let steer = rows(&views)
        .into_iter()
        .find_map(|row| match row {
            TranscriptRow::Steer { text, sent_at_ms } => Some((text.clone(), *sent_at_ms)),
            _ => None,
        })
        .unwrap();
    assert!(steer.0.starts_with("steer "));
    assert_eq!(steer.1, Some(accepted));
}

#[test]
fn a_failed_turn_shows_the_terminal_record_error_once() {
    let (_, views) = single_turn(End::Failed, true, false);
    let errors: Vec<_> = rows(&views)
        .into_iter()
        .filter(|row| row.is_error())
        .collect();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].text().unwrap().contains("boom"));
}

#[test]
fn a_capacity_terminal_rebuilds_once_and_pauses_a_host_wake_from_facts() {
    for host_wake in [false, true] {
        let feed = feed_of(&[Step::Turn {
            host_wake,
            submitted: true,
            tools: 1,
            nested_duplicate: false,
            steer: false,
            end: End::ContextCapacity,
        }]);
        let views = project_all(&feed).views;
        assert_eq!(
            rows(&views)
                .into_iter()
                .filter(|row| row.is_error())
                .count(),
            1
        );
        assert!(matches!(
            views.pause_cause(&feed.intents),
            Some(PauseCause::ContextCapacity { .. })
        ));
        assert_eq!(views.wake_suppressed(&feed.intents), host_wake);
        let (replayed, _) = project_paged(&feed, &[1], 2);
        assert_eq!(replayed, views);
        assert_eq!(project_all(&feed).views, views);
    }
}

#[test]
fn tool_rows_take_identity_from_the_effect_and_stamps_from_its_records() {
    let (_, views) = single_turn(End::Completed, true, false);
    let tool = rows(&views)
        .into_iter()
        .find_map(TranscriptRow::as_tool)
        .unwrap();
    assert_eq!(tool.name, "process_session_start");
    assert_eq!(tool.exposed_name.as_deref(), Some("bash"));
    assert_eq!(tool.subject.as_deref(), Some("ls -la"));
    assert!(tool.started_at_ms.is_some());
    assert!(tool.finished_at_ms > tool.started_at_ms);
    assert_eq!(views.state.counters.tool_calls, 1);
    assert_eq!(views.state.counters.root_turns, 1);
}

#[test]
fn a_bound_dropped_call_is_a_settled_tombstone_and_a_late_result_stays_above_the_floor() {
    let (mut feed, _) = single_turn(End::Completed, true, false);
    let dispatched = feed
        .records
        .iter()
        .position(|record| {
            record.facts().iter().any(|fact| {
                matches!(
                    fact,
                    FeedFact::OperationAdvanced {
                        state: FeedOperationState::Dispatched,
                        kind: FeedOperationKind::Effect { .. },
                        ..
                    }
                )
            })
        })
        .unwrap()
        + 1;
    feed.records.truncate(dispatched);
    feed.seq = dispatched as u64;
    feed.head = record_checkpoint(feed.records.last().unwrap()).unwrap();
    let mut views = project_all(&feed).views;
    let (call_id, call) = views.state.calls.first_key_value().unwrap();
    let call_id = call_id.clone();
    let mut call = call.clone();
    call.settled = false;
    views.state.calls = BTreeMap::from([(call_id.clone(), call.clone())]);
    views.rows.retain(|key, _| *key == call.key);
    for ordinal in 0..crate::sessions::MAX_TRANSCRIPT_LINES {
        views.rows.insert(
            RowKey::new(
                call.key.anchor + 1,
                Lane::Runtime,
                u32::try_from(ordinal).unwrap(),
            ),
            TranscriptRow::assistant("retained output"),
        );
    }
    views.state.row_bytes = views
        .rows
        .values()
        .map(|row| plakous_host_protocol::views::presentation::row_bytes(row) as u64)
        .sum();
    feed.advance(Vec::new(), Reads::default());
    let bounded = project(
        &views,
        &feed.intents,
        &feed.records[feed.records.len() - 1..],
        &feed.reads,
    )
    .unwrap()
    .views;
    assert!(!bounded.rows.contains_key(&call.key));
    assert!(
        bounded.state.calls[&call_id].settled,
        "the dropped row has no unfinished presentation left"
    );
    let (fact, items) = feed.history(
        call.turn_id.as_deref().map(|turn| turn.parse().unwrap()),
        HistorySegmentKind::Conversation,
        vec![text_item(ReadActor::Assistant, "late result committed")],
        vec![call_id.parse().unwrap()],
    );
    feed.advance(
        vec![fact],
        Reads {
            items: vec![(0, items)],
            calls: vec![(
                CallTarget::Call(call_id.parse().unwrap()),
                CallRead::Tool {
                    canonical: "process_session_start".into(),
                    exposed: "bash".into(),
                    arguments: None,
                    result: Some(ResultRead {
                        succeeded: true,
                        text: "late result".into(),
                    }),
                },
            )],
            ..Reads::default()
        },
    );
    let late = project(
        &bounded,
        &feed.intents,
        &feed.records[feed.records.len() - 1..],
        &feed.reads,
    )
    .unwrap()
    .views;
    assert!(
        !late.rows.contains_key(&call.key),
        "a late result cannot resurrect the dropped key"
    );
    assert_eq!(
        late.state.counters.tool_calls,
        bounded.state.counters.tool_calls
    );
}

#[test]
fn terminal_turns_prune_call_bookkeeping_but_keep_their_transcript_rows() {
    for end in [
        End::Completed,
        End::Failed,
        End::Interrupted,
        End::Abandoned,
        End::RecoveryRequired,
    ] {
        let (_, views) = single_turn(end, true, false);
        assert!(
            views.state.calls.is_empty(),
            "terminal calls need no live bookkeeping"
        );
        assert!(views.rows.values().any(|row| row.as_tool().is_some()));
        assert_eq!(views.state.counters.tool_calls, 1);
    }
}

#[test]
fn an_interrupted_turn_derives_the_marker_and_cancels_its_open_tools() {
    let (_, views) = single_turn(End::Interrupted, true, false);
    assert!(rows(&views).contains(&&TranscriptRow::Interrupted));
    let tool = rows(&views)
        .into_iter()
        .find_map(TranscriptRow::as_tool)
        .unwrap();
    assert_eq!(tool.state, "cancelled");
}

#[test]
fn an_abandoned_turn_derives_the_live_recovery_line() {
    let (_, views) = single_turn(End::Abandoned, true, false);
    assert!(rows(&views).iter().any(|row| matches!(
        row,
        TranscriptRow::Notice(notice)
            if notice.scope == NoticeScope::Recovery && notice.text.starts_with("plak: recovered")
    )));
}

#[test]
fn a_recovery_required_terminal_pauses_until_the_next_user_prompt() {
    let (feed, views) = single_turn(End::RecoveryRequired, true, false);
    assert!(matches!(
        views.pause_cause(&feed.intents),
        Some(PauseCause::RecoveryRequired { .. })
    ));
    let mut feed = feed;
    apply_step(
        &mut feed,
        &Step::Turn {
            host_wake: false,
            submitted: true,
            tools: 0,
            nested_duplicate: false,
            steer: false,
            end: End::Completed,
        },
    );
    let views = project_all(&feed).views;
    assert_eq!(views.pause_cause(&feed.intents), None);
}

#[test]
fn projected_compaction_refusal_and_recovery_notices_use_live_wording() {
    for (outcome, note) in [
        (
            FeedWorkflowOutcome::NoEligibleRange,
            crate::runtime::COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
        ),
        (
            FeedWorkflowOutcome::Abandoned {
                summary_unsent: true,
            },
            crate::runtime::COMPACT_ABANDONED_UNSENT_NOTE,
        ),
        (
            FeedWorkflowOutcome::Abandoned {
                summary_unsent: false,
            },
            crate::runtime::COMPACT_ABANDONED_BILLED_NOTE,
        ),
    ] {
        let mut feed = Feed::new();
        let workflow_id = SessionWorkflowRootId::new([1; 32]).unwrap();
        let kind = FeedWorkflowKind::Compact {
            reason: FeedCompactReason::Endpoint,
        };
        feed.advance(
            vec![FeedFact::WorkflowAdmitted {
                workflow_id,
                operation_id: Some("compact-notice".into()),
                kind,
            }],
            Reads::default(),
        );
        feed.advance(
            vec![FeedFact::WorkflowSettled {
                workflow_id,
                operation_id: Some("compact-notice".into()),
                kind,
                outcome,
            }],
            Reads::default(),
        );
        let views = project_all(&feed).views;
        assert_eq!(
            rows(&views)
                .iter()
                .filter(|row| matches!(row,
                    TranscriptRow::Notice(notice) if notice.text == note,
                ))
                .count(),
            1
        );
    }
    let (_, views) = single_turn(End::RecoveryRequired, false, false);
    assert!(rows(&views).iter().any(|row| matches!(row,
        TranscriptRow::Notice(notice) if notice.text == crate::runtime::RECOVERY_PENDING_NOTE,
    )));
    let views = project_all(&feed_of(&[Step::Rejected])).views;
    assert!(rows(&views).iter().any(|row| matches!(row,
        TranscriptRow::Notice(notice) if notice.text == format!("{}: storage pressure", crate::runtime::PROMPT_NOT_ADMITTED),
    )));
}

#[test]
fn compact_rows_carry_the_settled_figures_and_count_once() {
    let feed = feed_of(&[Step::Compact {
        compacted: true,
        scheduled: false,
    }]);
    let views = project_all(&feed).views;
    let compact = rows(&views)
        .into_iter()
        .find_map(TranscriptRow::as_tool)
        .unwrap();
    assert_eq!(compact.name, "compact");
    assert_eq!(compact.state, "succeeded");
    assert_eq!(compact.subject.as_deref(), Some("199.7k → 12.4k"));
    assert!(compact.started_at_ms < compact.finished_at_ms);
    assert_eq!(views.state.counters.compactions, 1);
}

#[test]
fn a_scheduled_pass_that_did_not_compact_pauses_by_its_request() {
    let feed = feed_of(&[Step::Compact {
        compacted: false,
        scheduled: true,
    }]);
    let views = project_all(&feed).views;
    assert!(matches!(
        views.pause_cause(&feed.intents),
        Some(PauseCause::CompactionNotApplied { .. })
    ));
    let manual = feed_of(&[Step::Compact {
        compacted: false,
        scheduled: false,
    }]);
    assert_eq!(
        project_all(&manual).views.pause_cause(&manual.intents),
        None
    );
}

#[test]
fn deliveries_and_pending_commands_follow_the_facts() {
    let mut feed = feed_of(&[Step::Turn {
        host_wake: false,
        submitted: true,
        tools: 0,
        nested_duplicate: false,
        steer: false,
        end: End::Completed,
    }]);
    feed.intents.submissions.push(Submission {
        command_id: "cmd-unrecorded".to_owned(),
        kind: SubmissionKind::RootStart,
        queue_item_id: "item-x".to_owned(),
        queue_item_revision: 1,
        target_turn_id: None,
        canonical_command: "later".to_owned(),
        submitted_at_ms: 7,
    });
    apply_step(&mut feed, &Step::Rejected);
    let views = project_all(&feed).views;
    assert_eq!(
        pending_command_ids(&views, &feed.intents),
        vec!["cmd-unrecorded".to_owned()]
    );
    assert!(views.state.deliveries.values().any(
        |delivery| matches!(delivery, Delivery::Rejected { error } if error == "storage_pressure")
    ));
    assert!(matches!(
        views.pause_cause(&feed.intents),
        Some(PauseCause::SessionRefusal { .. })
    ));
}

#[test]
fn notices_and_imported_rows_merge_by_anchor() {
    let mut feed = feed_of(&[Step::Turn {
        host_wake: false,
        submitted: true,
        tools: 0,
        nested_duplicate: false,
        steer: false,
        end: End::Completed,
    }]);
    feed.intents.imported = vec![TranscriptRow::user("imported")];
    feed.intents.notices.push(OperatorNotice {
        notice_id: "n1".to_owned(),
        anchor: 1,
        ordinal: 0,
        row: TranscriptRow::info(NoticeScope::Settings, "model changed"),
    });
    let views = project_all(&feed).views;
    let transcript = views.transcript(&feed.intents);
    assert_eq!(transcript[0].1, TranscriptRow::user("imported"));
    assert_eq!(transcript[1].0, RowKey::new(1, Lane::Product, 0));
    assert!(transcript[2].0.anchor > 1);
}

/// K4 open issue (g): the transcript bound counts the product-lane rows
/// (notices and imported rows) anchored before each record, so a session
/// projected record by record, seeing each notice once it was written,
/// truncates exactly where a rebuild with every notice does.
#[test]
fn notices_count_in_the_transcript_bound_alike_incrementally_and_rebuilt() {
    let turn = Step::Turn {
        host_wake: false,
        submitted: true,
        tools: 1,
        nested_duplicate: false,
        steer: false,
        end: End::Completed,
    };
    let feed = feed_of(&vec![turn; 6]);
    assert!(feed.records.len() > 9);
    let large = "n".repeat(crate::sessions::MAX_TRANSCRIPT_BYTES / 4 + 1);
    let notices: Vec<OperatorNotice> = (0..4_u64)
        .map(|index| OperatorNotice {
            notice_id: format!("n{index}"),
            anchor: 2 + index * 2,
            ordinal: 0,
            row: TranscriptRow::info(NoticeScope::Settings, large.clone()),
        })
        .collect();
    let mut every = feed.intents.clone();
    every.notices.clone_from(&notices);
    let prefixes: Vec<ProductIntents> = (0..=notices.len())
        .map(|count| {
            let mut written = feed.intents.clone();
            written.notices = notices[..count].to_vec();
            written
        })
        .collect();

    let mut incremental = SessionViews::empty();
    for record in &feed.records {
        let count = notices
            .iter()
            .filter(|notice| notice.anchor <= incremental.cursor.get())
            .count();
        incremental = project(
            &incremental,
            &prefixes[count],
            std::slice::from_ref(record),
            &feed.reads,
        )
        .unwrap()
        .views;
    }
    let rebuilt = project(&SessionViews::empty(), &every, &feed.records, &feed.reads)
        .unwrap()
        .views;
    assert_eq!(incremental, rebuilt);
    assert!(rebuilt.state.truncated, "the notices reached the bound");
    let bytes: usize = rebuilt
        .transcript(&every)
        .iter()
        .map(|(_, row)| plakous_host_protocol::views::presentation::row_bytes(row))
        .sum();
    assert!(bytes <= crate::sessions::MAX_TRANSCRIPT_BYTES);
}

/// K4 open issue (b): a tool round streams its prose before its tools run,
/// and its response reaches history only with the results. The round's
/// prose is keyed at the record of its provider decision, so it precedes
/// the round's tool rows as it did live.
#[test]
fn a_tool_round_keys_its_prose_at_its_provider_decision() {
    let mut feed = Feed::new();
    let turn_id: TurnId = id(feed.fresh());
    let (prompt, prompt_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::Conversation,
        vec![text_item(ReadActor::User, "list the files")],
        Vec::new(),
    );
    feed.advance(
        vec![
            FeedFact::CommandAccepted {
                command_id: CommandId::new("cmd-round").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(turn_id),
                sequence: 1,
                operation_id: None,
            },
            prompt,
        ],
        Reads {
            items: vec![(1, prompt_items)],
            ..Reads::default()
        },
    );
    let attempt = feed.fresh();
    let attempt_id = id(feed.fresh());
    feed.advance(
        vec![FeedFact::OperationAdvanced {
            operation_id: attempt,
            turn_id: Some(turn_id),
            kind: FeedOperationKind::ProviderAttempt { attempt_id },
            state: FeedOperationState::Completed,
        }],
        Reads::default(),
    );
    let decision = feed.seq;
    let (operation, call): (u64, CallId) = (feed.fresh(), id(feed.fresh()));
    let read = CallRead::Tool {
        canonical: "process_session_start".to_owned(),
        exposed: "bash".to_owned(),
        arguments: Some(r#"{"command":"ls -la"}"#.to_owned()),
        result: None,
    };
    feed.advance(
        vec![effect(
            operation,
            turn_id,
            call,
            FeedCallerLineage::Direct,
            FeedOperationState::Dispatched,
        )],
        Reads {
            calls: vec![(CallTarget::Operation(operation), read)],
            ..Reads::default()
        },
    );
    let (response, response_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::ToolExchange,
        vec![ReadItem {
            actor: ReadActor::Assistant,
            harness_authored: false,
            parts: vec![
                ReadPart::Reasoning("look first".to_owned()),
                ReadPart::Text("I will list them.".to_owned()),
                ReadPart::Call {
                    call_id: call.to_string(),
                    canonical: "process_session_start".to_owned(),
                    exposed: "bash".to_owned(),
                    arguments: Some(r#"{"command":"ls -la"}"#.to_owned()),
                    parent_call_id: None,
                    cell_id: None,
                    subject: None,
                },
            ],
        }],
        vec![call],
    );
    feed.advance(
        vec![response],
        Reads {
            items: vec![(0, response_items)],
            calls: vec![(
                CallTarget::Call(call),
                CallRead::Tool {
                    canonical: "process_session_start".to_owned(),
                    exposed: "bash".to_owned(),
                    arguments: Some(r#"{"command":"ls -la"}"#.to_owned()),
                    result: Some(ResultRead {
                        succeeded: true,
                        text: "a.rs".to_owned(),
                    }),
                },
            )],
            ..Reads::default()
        },
    );
    let views = project_all(&feed).views;
    let order: Vec<(u64, &str)> = views
        .rows
        .iter()
        .map(|(key, row)| {
            (
                key.anchor,
                match row {
                    TranscriptRow::User { .. } => "user",
                    TranscriptRow::Thinking(_) => "thinking",
                    TranscriptRow::Assistant { .. } => "assistant",
                    TranscriptRow::Tool(_) => "tool",
                    _ => "other",
                },
            )
        })
        .collect();
    assert_eq!(
        order.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
        ["user", "thinking", "assistant", "tool"],
        "{order:?}"
    );
    assert_eq!(order[1].0, decision);
    assert_eq!(order[2].0, decision);
    // A rebuild one record at a time keys them identically.
    let (paged, stored) = project_paged(&feed, &[1], 0);
    assert_eq!(paged.rows, views.rows);
    assert_eq!(stored, views.rows);
}

/// An operator cancel of a turn with work in flight may be settled by an
/// abandonment closure; the turn an accepted cancel named still shows the
/// interrupted marker the live face showed, not a recovery line.
#[test]
fn a_cancel_settled_by_an_abandonment_is_the_operators_interrupt() {
    let mut feed = Feed::new();
    let turn_id: TurnId = id(feed.fresh());
    let (prompt, prompt_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::Conversation,
        vec![text_item(ReadActor::User, "run it")],
        Vec::new(),
    );
    feed.advance(
        vec![
            FeedFact::CommandAccepted {
                command_id: CommandId::new("cmd-root").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(turn_id),
                sequence: 1,
                operation_id: None,
            },
            prompt,
        ],
        Reads {
            items: vec![(1, prompt_items)],
            ..Reads::default()
        },
    );
    feed.advance(
        vec![FeedFact::CommandAccepted {
            command_id: CommandId::new("cmd-cancel").unwrap(),
            kind: FeedCommandKind::Cancel,
            turn_id: Some(turn_id),
            sequence: 2,
            operation_id: None,
        }],
        Reads::default(),
    );
    feed.advance(
        vec![FeedFact::TurnTerminal {
            turn_id,
            command_id: CommandId::new("cmd-root").unwrap(),
            outcome: FeedTurnOutcome::Cancelled {
                cause: FeedCancelCause::Abandoned {
                    claim: AbandonedTurnClaim::ToolRoundCompleted,
                    cause: AbandonmentCause::Interrupted,
                    model_round_completed: false,
                    tool_round_completed: true,
                },
            },
            closed_exchange: true,
            steering_settled_unapplied: false,
            usage: TurnUsage::default(),
        }],
        Reads::default(),
    );
    let views = project_all(&feed).views;
    let rows: Vec<&TranscriptRow> = views.rows.values().collect();
    assert_eq!(rows.last(), Some(&&TranscriptRow::Interrupted), "{rows:?}");
    assert!(
        !rows
            .iter()
            .any(|row| row.text().is_some_and(|text| text.contains("recovered"))),
        "{rows:?}"
    );
}

#[test]
fn wake_suppression_merges_intents_and_facts_by_anchor() {
    let mut feed = Feed::new();
    feed.intents.wake_suppressions.push(AnchoredIntent {
        anchor: 1,
        ordinal: 0,
        value: (),
    });
    let views = project_all(&feed).views;
    assert!(views.wake_suppressed(&feed.intents));
    apply_step(
        &mut feed,
        &Step::Turn {
            host_wake: false,
            submitted: true,
            tools: 0,
            nested_duplicate: false,
            steer: false,
            end: End::Completed,
        },
    );
    let views = project_all(&feed).views;
    assert!(!views.wake_suppressed(&feed.intents));
}

#[test]
fn pruned_items_derive_one_not_retained_marker() {
    let mut feed = Feed::new();
    let (fact, _) = feed.history(
        None,
        HistorySegmentKind::Conversation,
        Vec::new(),
        Vec::new(),
    );
    feed.advance(
        vec![fact],
        Reads {
            items: vec![(0, None)],
            ..Reads::default()
        },
    );
    let views = project_all(&feed).views;
    assert_eq!(
        rows(&views),
        vec![&TranscriptRow::info(NoticeScope::Session, NOT_RETAINED_ROW)]
    );
}

#[test]
fn a_record_whose_read_was_not_loaded_is_refused_whole() {
    let feed = feed_of(&[Step::Turn {
        host_wake: false,
        submitted: true,
        tools: 1,
        nested_duplicate: false,
        steer: false,
        end: End::Completed,
    }]);
    let error = project(
        &SessionViews::empty(),
        &feed.intents,
        &feed.records,
        &ReadCache::default(),
    )
    .unwrap_err();
    assert!(matches!(error, ProjectError::MissingRead(_)));
}

#[test]
fn old_submission_anchor_preserves_retained_command_facts() {
    for delivery in [
        Delivery::Accepted,
        Delivery::Rejected {
            error: "busy".into(),
        },
    ] {
        let mut views = SessionViews::empty();
        views.state.retained_from = Some(10);
        let submission = Submission {
            command_id: "old-command".into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: "old-item".into(),
            queue_item_revision: 1,
            target_turn_id: None,
            canonical_command: "prompt".into(),
            submitted_at_ms: 1,
        };
        let mut intents = ProductIntents::default();
        intents
            .submission_anchors
            .insert(submission.command_id.clone(), 1);
        intents.submissions.push(submission.clone());
        views
            .state
            .deliveries
            .insert(submission.command_id.clone(), delivery.clone());

        views.consume_before_retention(&intents);
        views.consume_before_retention(&intents);
        assert_eq!(views.state.deliveries[&submission.command_id], delivery);

        views.state.deliveries.clear();
        views.consume_before_retention(&intents);
        assert_eq!(
            views.state.deliveries[&submission.command_id],
            Delivery::Consumed
        );
    }
}

#[test]
fn old_workflow_anchor_preserves_retained_workflow_facts() {
    for settled in [None, Some(WorkflowSettlement::NoEligibleRange)] {
        let mut views = SessionViews::empty();
        views.state.retained_from = Some(10);
        let request = WorkflowRequest {
            operation_id: "old-workflow".into(),
            kind: SessionWorkflowKind::Compact(ProductCompactReason::Scheduled),
            source_locator: SessionLocator::new(id(1), id(2), id(3)),
        };
        let mut intents = ProductIntents::default();
        intents
            .workflow_anchors
            .insert(request.operation_id.clone(), 1);
        intents.workflow_requests.push(request.clone());
        let completion = WorkflowCompletion {
            admitted_at: Some(10),
            settled,
        };
        views
            .state
            .workflows
            .insert(request.operation_id.clone(), completion.clone());

        views.consume_before_retention(&intents);
        views.consume_before_retention(&intents);
        assert_eq!(views.state.workflows[&request.operation_id], completion);

        views.state.workflows.clear();
        views.consume_before_retention(&intents);
        assert_eq!(
            views.state.workflows[&request.operation_id],
            WorkflowCompletion {
                admitted_at: None,
                settled: Some(WorkflowSettlement::Consumed),
            }
        );
    }
}

#[test]
fn behind_retention_restarts_counters_behind_one_marker() {
    let feed = feed_of(&[]);
    let head = FeedLocator {
        session_id: feed.session,
        branch_id: feed.branch,
        checkpoint_id: feed.head,
    };
    let views = rebuild_behind_retention(
        vec![LineageStep {
            branch_id: feed.branch,
            from_seq: 0,
        }],
        feed.session,
        FeedSeq::new(40),
        FeedSeq::new(90),
        head,
        &[text_item(ReadActor::User, "tail prompt")],
    );
    assert_eq!(views.cursor, FeedSeq::new(39));
    assert_eq!(views.state.counters, Counters::default());
    let transcript = views.transcript(&feed.intents);
    assert_eq!(transcript[0].1, TranscriptRow::Truncated);
    assert_eq!(transcript[1].1, TranscriptRow::user("tail prompt"));
}

#[test]
fn retained_window_rows_cannot_precede_or_evict_the_seeded_canonical_tail() {
    let feed = feed_of(&[
        Step::Turn {
            host_wake: false,
            submitted: true,
            tools: 1,
            nested_duplicate: false,
            steer: false,
            end: End::Interrupted,
        },
        Step::Compact {
            compacted: true,
            scheduled: false,
        },
        Step::Process { terminal: true },
    ]);
    let live = project_all(&feed).views;
    // Fill the normal transcript bound so misplaced retained rows cannot hide
    // behind a small fixture that never trims.
    let history = (0..crate::sessions::MAX_TRANSCRIPT_LINES)
        .map(|n| text_item(ReadActor::User, &format!("canonical tail {n}")))
        .collect::<Vec<_>>();
    let seed = rebuild_behind_retention(
        live.state.lineage.clone(),
        feed.session,
        FeedSeq::new(2),
        live.cursor,
        live.state.head.unwrap(),
        &history,
    );
    let rebuilt = project(&seed, &feed.intents, &feed.records[1..], &feed.reads).unwrap();
    assert_eq!(rebuilt.views.rows, seed.rows);
    let rows = rebuilt.views.transcript(&ProductIntents::default());
    assert_eq!(rows.first().unwrap().1, TranscriptRow::Truncated);
    assert!(rows.iter().skip(1).all(|(key, row)|
        key.anchor == live.cursor.get() && matches!(row, TranscriptRow::User { text, .. } if text.starts_with("canonical tail "))
    ));
    assert_eq!(rebuilt.views.state.counters, live.state.counters);
    assert_eq!(rebuilt.views.state.calls.len(), live.state.calls.len());
    assert_eq!(
        rebuilt.views.state.processes.len(),
        live.state.processes.len()
    );
    assert_eq!(rebuilt.terminal_usages.len(), 1);
    for end in 2..=feed.records.len() {
        let retained = project(&seed, &feed.intents, &feed.records[1..end], &feed.reads).unwrap();
        let full = project(
            &SessionViews::empty(),
            &feed.intents,
            &feed.records[..end],
            &feed.reads,
        )
        .unwrap();
        assert_eq!(retained.views.state.calls, full.views.state.calls);
        assert_eq!(retained.views.state.processes, full.views.state.processes);
    }
}

#[test]
fn retained_history_frontier_survives_an_interrupted_replay() {
    let mut feed = feed_of(&[Step::Turn {
        host_wake: false,
        submitted: true,
        tools: 1,
        nested_duplicate: false,
        steer: false,
        end: End::Completed,
    }]);
    let live = project_all(&feed).views;
    let history: Vec<_> = live
        .rows
        .values()
        .filter_map(|row| match row {
            TranscriptRow::User { text, .. } => Some(text_item(ReadActor::User, text)),
            TranscriptRow::Assistant { text } => Some(text_item(ReadActor::Assistant, text)),
            _ => None,
        })
        .collect();
    let tail = live.cursor;
    let seed = rebuild_behind_retention(
        live.state.lineage.clone(),
        feed.session,
        FeedSeq::new(2),
        tail,
        live.state.head.unwrap(),
        &history,
    );
    let mut partial = project(&seed, &feed.intents, &feed.records[1..2], &feed.reads)
        .unwrap()
        .views;
    assert_eq!(partial.state.history_through, Some(tail.get()));
    let mut restored: SessionViewState =
        serde_json::from_value(serde_json::to_value(&partial.state).unwrap()).unwrap();
    // The store hydrates these separately keyed maps beside the JSON state.
    restored.open_turns.clone_from(&partial.state.open_turns);
    restored.calls.clone_from(&partial.state.calls);
    restored.processes.clone_from(&partial.state.processes);
    partial.state = restored;
    let resumed = project(&partial, &feed.intents, &feed.records[2..], &feed.reads).unwrap();
    assert_eq!(resumed.views.state.history_through, None);
    assert_eq!(resumed.terminal_usages.len(), 1);
    assert_eq!(resumed.views.state.counters, live.state.counters);
    assert_eq!(
        resumed
            .views
            .rows
            .values()
            .filter(|row| matches!(row, TranscriptRow::User { .. }))
            .count(),
        1
    );
    let start = feed.records.len();
    apply_step(
        &mut feed,
        &Step::Turn {
            host_wake: false,
            submitted: true,
            tools: 0,
            nested_duplicate: false,
            steer: false,
            end: End::Completed,
        },
    );
    let later = project(
        &resumed.views,
        &feed.intents,
        &feed.records[start..],
        &feed.reads,
    )
    .unwrap();
    assert_eq!(
        later
            .views
            .rows
            .values()
            .filter(|row| matches!(row, TranscriptRow::User { .. }))
            .count(),
        2
    );
}

#[test]
fn records_of_another_runtime_session_are_refused() {
    let feed = feed_of(&[]);
    let applied = project_all(&feed).views;
    let mut foreign = Feed::new_for(id(78));
    apply_step(&mut foreign, &Step::Release);
    assert_eq!(
        project(&applied, &feed.intents, &foreign.records, &foreign.reads).unwrap_err(),
        ProjectError::ForeignSession
    );
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Reads a pilot session's whole feed through the facade, page by page.
async fn pilot_records(
    sessions: &AgentSessions,
    session: SessionId,
) -> TestResult<Vec<FeedRecord>> {
    use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
    let mut cursor = FeedCursor::start(session);
    let mut records = Vec::new();
    loop {
        let FeedPage::Records { records: page, .. } =
            sessions.feed_page(FeedPageRequest::after(cursor)).await?
        else {
            return Err("pilot feed is behind retention".into());
        };
        let Some(last) = page.last() else {
            return Ok(records);
        };
        cursor = cursor.advanced_past(last);
        records.extend(page);
    }
}

/// A pilot over a real `AgentSessions`: two scripted turns (one with a tool
/// round) and a manual compaction. The facade-read driver projects the same
/// views at every page size and over replayed pages, and the rows carry what
/// the live session did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pilot_feed_projects_identically_at_every_page_boundary() -> TestResult {
    use plakous::sessions::{
        CompactReason, CompactSessionOutcome, CompactSessionRequest, CreateSessionOutcome,
        CreateSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
    use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    let sessions = AgentSessions::new(SessionBackend::sqlite(workspace.join("runtime.sqlite"))?)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("evidence.jsonl"),
    )?;
    let plan: Arc<ScriptPlan> = Arc::new(serde_json::from_value(serde_json::json!({
        "version": SCRIPT_VERSION,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "child done"}},
            {"output": {"text": "second answer"}}
        ]
    }))?);
    let binding = plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
        workspace,
        controller.clone(),
        Arc::clone(&plan),
        Arc::new(AtomicU64::new(1)),
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("projector-pilot:create")?,
            binding,
        ))
        .await?
    else {
        return Err("not created".into());
    };
    for prompt in ["child intent", "second intent"] {
        session.agent().prompt(prompt).await?.result().await?;
    }
    let compaction = plakous_offline_crash_harness::scripted_compaction_runtime_binding(
        workspace, controller, &plan, None, None,
    )?;
    let CompactSessionOutcome::Compacted { .. } = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new("projector-pilot:compact")?,
            session.locator(),
            CompactReason::Manual,
            compaction,
        ))
        .await?
    else {
        return Err("not compacted".into());
    };
    let records = pilot_records(&sessions, session.locator().session_id()).await?;
    let intents = ProductIntents::default();
    let all = run(&sessions, &SessionViews::empty(), &intents, &records).await?;
    let sizes: Vec<usize> = if cfg!(debug_assertions) {
        vec![1, 2, records.len()]
    } else {
        (1..=records.len()).collect()
    };
    for size in sizes {
        for replay in [0, 1, 3] {
            let mut views = SessionViews::empty();
            let mut start = 0;
            while start < records.len() {
                let end = (start + size).min(records.len());
                let from = start.saturating_sub(replay);
                views = run(&sessions, &views, &intents, &records[from..end])
                    .await?
                    .views;
                start = end;
            }
            assert_eq!(views, all.views, "page size {size}, replay {replay}");
        }
    }
    let again = run(&sessions, &all.views, &intents, &records).await?;
    assert!(again.writes.is_empty());
    let rows: Vec<&TranscriptRow> = all.views.rows.values().collect();
    let position = |wanted: &dyn Fn(&TranscriptRow) -> bool| {
        rows.iter()
            .position(|row| wanted(row))
            .unwrap_or_else(|| panic!("missing row in {rows:?}"))
    };
    let user = position(&|row| *row == TranscriptRow::user("child intent"));
    let tool = position(&|row| {
        row.as_tool()
            .is_some_and(|tool| tool.name == "loopback_effect" && tool.state == "succeeded")
    });
    let answer = position(&|row| *row == TranscriptRow::assistant("child done"));
    let compact = position(&|row| {
        row.as_tool()
            .is_some_and(|tool| tool.name == "compact" && tool.finished_at_ms.is_some())
    });
    assert!(user < tool && tool < answer && answer < compact, "{rows:?}");
    let call = rows[tool].as_tool().unwrap();
    assert!(
        call.started_at_ms.is_some() && call.finished_at_ms.is_some(),
        "{call:?}"
    );
    assert_eq!(
        all.views.state.counters,
        Counters {
            root_turns: 2,
            tool_calls: 1,
            compactions: 1,
        }
    );
    session.release().await?;
    Ok(())
}

#[test]
fn a_ten_thousand_item_append_projects_through_chunked_reads() {
    let mut feed = Feed::new();
    let items: Vec<ReadItem> = (0..10_000)
        .map(|index| text_item(ReadActor::User, &format!("item {index}")))
        .collect();
    let (fact, read) = feed.history(None, HistorySegmentKind::Conversation, items, Vec::new());
    feed.advance(
        vec![fact],
        Reads {
            items: vec![(0, read)],
            ..Reads::default()
        },
    );
    let record = feed.records.last().unwrap();
    let chunks: Vec<(u64, u64, bool, bool)> = plan_reads(record)
        .into_iter()
        .filter_map(|request| match request {
            ReadRequest::Items {
                first_ordinal,
                count,
                first_entry,
                last_entry,
                ..
            } => Some((
                first_ordinal,
                count,
                first_entry.is_some(),
                last_entry.is_some(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        chunks,
        vec![
            (0, 4_096, true, false),
            (4_096, 4_096, false, false),
            (8_192, 1_808, false, true),
        ]
    );
    let views = project_all(&feed).views;
    let users: Vec<&str> = rows(&views)
        .into_iter()
        .filter_map(|row| match row {
            TranscriptRow::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let expected: Vec<String> = (10_000 - users.len()..10_000)
        .map(|index| format!("item {index}"))
        .collect();
    assert!(users.len() > 5_000, "the bound keeps most rows");
    assert_eq!(users, expected, "every chunk lands in ordinal order");
    for boundary in ["item 8191", "item 8192", "item 9999"] {
        assert!(users.contains(&boundary));
    }
}

fn call_part(call: CallId, canonical: &str, subject: Option<&str>) -> ReadPart {
    ReadPart::Call {
        call_id: call.to_string(),
        canonical: canonical.to_owned(),
        exposed: canonical.to_owned(),
        arguments: Some(r#"{"query":"plak"}"#.to_owned()),
        parent_call_id: None,
        cell_id: None,
        subject: subject.map(str::to_owned),
    }
}

fn only_tool(views: &SessionViews) -> &crate::sessions::ToolItem {
    let tools: Vec<_> = views
        .rows
        .values()
        .filter_map(TranscriptRow::as_tool)
        .collect();
    assert_eq!(tools.len(), 1, "{tools:?}");
    tools[0]
}

/// (h) A provider-hosted call has no operation: its call and result blocks
/// commit together in the response segment, read through `feed_items`, and
/// that commit settles its row. It is not a local tool call.
#[test]
fn a_provider_hosted_call_settles_from_its_response_segment() {
    let mut feed = Feed::new();
    let turn_id: TurnId = id(feed.fresh());
    let call: CallId = id(feed.fresh());
    let (response, items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::Conversation,
        vec![ReadItem {
            actor: ReadActor::Assistant,
            harness_authored: false,
            parts: vec![
                call_part(call, "web_search", None),
                ReadPart::Result {
                    call_id: call.to_string(),
                    result: ResultRead {
                        succeeded: true,
                        text: "3 results".to_owned(),
                    },
                },
                ReadPart::Text("found it".to_owned()),
            ],
        }],
        Vec::new(),
    );
    feed.advance(
        vec![response],
        Reads {
            items: vec![(0, items)],
            ..Reads::default()
        },
    );
    let views = project_all(&feed).views;
    let tool = only_tool(&views);
    assert_eq!(tool.name, "web_search");
    assert_ne!(tool.state, "running", "{tool:?}");
    assert!(!tool.state.starts_with("failed"), "{tool:?}");
    let state = &views.state.calls[&call.to_string()];
    assert!(state.settled && !state.operated && !state.counted);
    assert_eq!(views.state.counters.tool_calls, 0);
    let (paged, stored) = project_paged(&feed, &[1], 0);
    assert_eq!(paged.rows, views.rows);
    assert_eq!(stored, views.rows);
}

/// (h) A call rejected before preparation has no operation: its subject
/// comes from its call block and its failure from its committed result; a
/// result in another item never settles a call through the hosted path.
#[test]
fn a_rejected_call_takes_its_subject_from_its_call_block() {
    let mut feed = Feed::new();
    let turn_id: TurnId = id(feed.fresh());
    let call: CallId = id(feed.fresh());
    let (response, response_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::Conversation,
        vec![ReadItem {
            actor: ReadActor::Assistant,
            harness_authored: false,
            parts: vec![call_part(call, "read_file", Some("src/"))],
        }],
        Vec::new(),
    );
    feed.advance(
        vec![response],
        Reads {
            items: vec![(0, response_items)],
            ..Reads::default()
        },
    );
    let failed = ResultRead {
        succeeded: false,
        text: "invalid arguments: path is required".to_owned(),
    };
    let (results, result_items) = feed.history(
        Some(turn_id),
        HistorySegmentKind::ToolExchange,
        vec![ReadItem {
            actor: ReadActor::Other,
            harness_authored: false,
            parts: vec![ReadPart::Result {
                call_id: call.to_string(),
                result: ResultRead {
                    succeeded: true,
                    text: "a preview that must not settle it".to_owned(),
                },
            }],
        }],
        vec![call],
    );
    feed.advance(
        vec![results],
        Reads {
            items: vec![(0, result_items)],
            calls: vec![(
                CallTarget::Call(call),
                CallRead::Tool {
                    canonical: "read_file".to_owned(),
                    exposed: "read_file".to_owned(),
                    arguments: Some("{}".to_owned()),
                    result: Some(failed),
                },
            )],
            ..Reads::default()
        },
    );
    let views = project_all(&feed).views;
    let tool = only_tool(&views);
    assert_eq!(tool.subject.as_deref(), Some("src/"));
    assert!(tool.state.starts_with("failed"), "{tool:?}");
    assert!(!views.state.calls[&call.to_string()].operated);
    let (paged, stored) = project_paged(&feed, &[1], 0);
    assert_eq!(paged.rows, views.rows);
    assert_eq!(stored, views.rows);
}

#[test]
fn child_mail_riding_a_prompt_projects_as_rows_attributed_to_each_child() {
    let feed = feed_of(&[]);
    let head = FeedLocator {
        session_id: feed.session,
        branch_id: feed.branch,
        checkpoint_id: feed.head,
    };
    let frame = crate::runtime::result_mail::CHILD_MAIL_FRAME;
    let command = format!(
        "fix the build\n\n{frame}\n\n<child_message message_id=\"m1\" from=\"tree-c7\" purpose=\"result\">\nall green\nmore\n</child_message>\n\n<child_message message_id=\"m2\" from=\"tree-c9\" purpose=\"question\">\nwhich &lt;branch&gt;?\n</child_message>"
    );
    let views = rebuild_behind_retention(
        vec![LineageStep {
            branch_id: feed.branch,
            from_seq: 0,
        }],
        feed.session,
        FeedSeq::new(40),
        FeedSeq::new(90),
        head,
        &[text_item(ReadActor::User, &command)],
    );
    let transcript = views.transcript(&feed.intents);
    let rows: Vec<_> = transcript
        .iter()
        .skip(1)
        .map(|(_, row)| row.clone())
        .collect();
    assert_eq!(
        rows,
        vec![
            TranscriptRow::user("fix the build"),
            TranscriptRow::info(NoticeScope::Tree, "result · #c7 · all green"),
            TranscriptRow::info(NoticeScope::Tree, "question · #c9 · which <branch>?"),
        ]
    );
}
