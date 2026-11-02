#![allow(
    clippy::too_many_lines,
    reason = "Transcript projection scenarios reconstruct complete canonical histories and compare persisted row identity and presentation."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::limits::OBSERVATION_QUEUE_CAPACITY;
use plakous_host_protocol::views::transcript::{Lane, NoticeScope, RowKey, TranscriptRow};
use plakous_host_protocol::{Core, observation_channel};

fn snapshot(scope: TranscriptScope, rows: &[(u64, &str)]) -> PresentationSnapshot {
    PresentationSnapshot {
        scope,
        items: rows
            .iter()
            .map(|(anchor, text)| TranscriptItem {
                key: RowKey::new(*anchor, Lane::Runtime, 0),
                row: TranscriptRow::assistant(*text),
            })
            .collect(),
        ..PresentationSnapshot::default()
    }
}

#[test]
fn explicit_replacement_installs_a_keyed_baseline() {
    let (sink, receiver) = observation_channel::<Core>();
    let sender = crate::runtime::Sender::over(sink);
    assert!(receiver.try_recv().is_err());
    assert!(sender.replace_transcript(snapshot(TranscriptScope::Root, &[(1, "row")])));
    let HostEvent::PresentationSnapshot(baseline) = receiver.try_recv().unwrap() else {
        panic!("baseline");
    };
    assert!(baseline.revision > 0);
}

#[test]
fn root_waiting_to_running_publishes_the_transition_exactly_once() {
    let (sink, receiver) = observation_channel::<Core>();
    let sender = crate::runtime::Sender::over(sink);
    sender.replace_transcript(snapshot(TranscriptScope::Root, &[]));
    receiver.try_iter().for_each(drop);
    let line = |state: &str| ContentEvent::ToolLine {
        call_id: Some("root-call".into()),
        parent_call_id: None,
        cell_id: None,
        kind: plakous_host_protocol::views::ActivityKind::Tool,
        name: "read_file".into(),
        state: state.into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: Some(100),
        finished_at_ms: None,
        wait: None,
    };
    sender.send(line("waiting"));
    let waiting: Vec<_> = receiver
        .try_iter()
        .filter_map(|event| {
            if let HostEvent::TranscriptPatch(patch) = event {
                Some(patch)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(waiting.len(), 1);
    let key = waiting[0].upserts[0].key;
    sender.send(line("running"));
    let running: Vec<_> = receiver
        .try_iter()
        .filter_map(|event| {
            if let HostEvent::TranscriptPatch(patch) = event {
                Some(patch)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        running.len(),
        1,
        "one root transition, not a suppressed or duplicate patch"
    );
    assert_eq!(running[0].scope, TranscriptScope::Root);
    assert_eq!(running[0].upserts.len(), 1);
    assert_eq!(running[0].upserts[0].key, key);
    assert_eq!(
        running[0].upserts[0].row.as_tool().unwrap().state,
        "running"
    );
    sender.send(line("running"));
    assert!(
        !receiver
            .try_iter()
            .any(|event| matches!(event, HostEvent::TranscriptPatch(_))),
        "a duplicate running observation must not publish another transition"
    );
}

#[test]
fn streaming_cost_is_constant_in_history_rows_and_does_not_copy_accumulated_text() {
    for scope in [
        TranscriptScope::Root,
        TranscriptScope::Agent("child".into()),
    ] {
        for count in [0, 2_000, 8_000] {
            let (sink, receiver) = observation_channel::<Core>();
            let sender = crate::runtime::Sender::over(sink);
            let mut initial = snapshot(scope.clone(), &[]);
            initial.items = (1..=count)
                .map(|anchor| TranscriptItem {
                    key: RowKey::new(anchor, Lane::Runtime, 0),
                    row: TranscriptRow::assistant("historical row"),
                })
                .collect();
            assert!(sender.replace_transcript(initial));
            let _ = receiver.try_iter().count();
            let revision = sender.presentations.lock().unwrap().states[&scope].revision();
            for _ in 0..2_000 {
                let delta = "x".repeat(40);
                assert!(match &scope {
                    TranscriptScope::Root => sender.send(ContentEvent::AssistantDelta(delta)),
                    TranscriptScope::Agent(id) =>
                        sender.observe_agent(id, &HostEvent::<Core>::AssistantDelta(delta)),
                });
            }
            let publisher = sender.presentations.lock().unwrap();
            assert_eq!(publisher.states[&scope].revision(), revision);
            assert_eq!(
                publisher.live[&scope].copied_rows, 0,
                "no old row or accumulated prose was cloned"
            );
            drop(publisher);
            let mut bytes = 0;
            let mut slots = 0;
            for event in receiver.try_iter() {
                slots += 1;
                bytes += match event {
                    HostEvent::AssistantDelta(text) => text.len(),
                    HostEvent::TranscriptStream {
                        delta:
                            plakous_host_protocol::views::transcript::TranscriptStreamDelta::Assistant(
                                text,
                            ),
                        ..
                    } => text.len(),
                    _ => panic!("deltas must not publish keyed rows or rebuild the TUI"),
                };
            }
            assert_eq!(bytes, 80_000, "only incoming bytes cross the mailbox");
            assert_eq!(
                slots, 1,
                "raw deltas coalesce exactly as before keyed publication"
            );
        }
    }
}

#[test]
fn live_tool_duration_uses_the_retained_start_stamp_and_explicit_finish_wins() {
    let (sink, receiver) = observation_channel::<Core>();
    let scope = TranscriptScope::Root;
    let mut publisher = Presentations::default();
    publisher.snapshot(snapshot(scope.clone(), &[]), &sink);
    let _ = receiver.try_recv().unwrap();
    for (started_at_ms, duration_ms, finished_at_ms, expected) in [
        (Some(100), None, None, None),
        (None, Some(35), None, Some(135)),
        (None, Some(50), Some(170), Some(170)),
    ] {
        assert!(
            publisher.observe(
                scope.clone(),
                &HostEvent::<Core>::ToolLine {
                    call_id: Some("call".into()),
                    parent_call_id: None,
                    cell_id: None,
                    kind: plakous_host_protocol::views::ActivityKind::Tool,
                    name: "read_file".into(),
                    state: if duration_ms.is_some() {
                        "done"
                    } else {
                        "running"
                    }
                    .into(),
                    subject: None,
                    detail: None,
                    duration_ms,
                    started_at_ms,
                    finished_at_ms,
                    wait: None,
                },
                &sink
            )
        );
        let HostEvent::TranscriptPatch(patch) = receiver.try_recv().unwrap() else {
            panic!("tool boundary");
        };
        let tool = patch.upserts[0].row.as_tool().unwrap();
        assert_eq!(tool.started_at_ms, Some(100));
        assert_eq!(tool.finished_at_ms, expected);
    }
}

#[test]
fn closing_an_agent_retires_its_live_resync_source_but_not_root() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    publisher.snapshot(snapshot(TranscriptScope::Root, &[(1, "root")]), &sink);
    let child = TranscriptScope::Agent("child".into());
    publisher.snapshot(snapshot(child.clone(), &[(1, "child")]), &sink);
    publisher.close_agent();
    let _ = receiver.try_iter().count();
    assert!(!publisher.resync(&child, &sink));
    assert!(publisher.resync(&TranscriptScope::Root, &sink));
    assert!(
        publisher
            .live
            .keys()
            .all(|scope| *scope == TranscriptScope::Root)
    );
}

#[test]
fn attach_is_full_and_ordinary_publication_is_only_the_changed_keys() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    assert!(publisher.publish(
        snapshot(TranscriptScope::Root, &[(1, "old"), (2, "kept")]),
        &sink
    ));
    let HostEvent::PresentationSnapshot(first) = receiver.try_recv().unwrap() else {
        panic!("attach snapshot");
    };
    let mut folded = TranscriptState::default();
    folded.replace(first).unwrap();
    assert!(publisher.publish(
        snapshot(TranscriptScope::Root, &[(2, "kept"), (7, "new")]),
        &sink
    ));
    let HostEvent::TranscriptPatch(patch) = receiver.try_recv().unwrap() else {
        panic!("ordinary patch");
    };
    assert_eq!(patch.upserts.len(), 1);
    assert_eq!(patch.upserts[0].key.anchor, 7);
    assert_eq!(patch.deletes, [RowKey::new(1, Lane::Runtime, 0)]);
    folded.apply(patch).unwrap();
    assert!(publisher.publish(
        snapshot(TranscriptScope::Root, &[(2, "kept"), (7, "new")]),
        &sink
    ));
    let HostEvent::TranscriptPatch(empty) = receiver.try_recv().unwrap() else {
        panic!("reconciliation patch");
    };
    assert!(empty.upserts.is_empty() && empty.deletes.is_empty());
    folded.apply(empty).unwrap();
    assert!(publisher.resync(&TranscriptScope::Root, &sink));
    let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
        panic!("resync snapshot");
    };
    assert_eq!(
        resync
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect::<BTreeMap<_, _>>(),
        *folded.items()
    );
}

#[test]
fn a_dropped_patch_is_recovered_by_a_full_overflow_snapshot() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    let scope = TranscriptScope::Root;
    assert!(publisher.publish(snapshot(scope.clone(), &[(1, "before")]), &sink));
    let _ = receiver.try_recv().unwrap();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sink.send(ContentEvent::Row(TranscriptRow::error(
            NoticeScope::Host,
            format!("error {index}"),
        )))
        .unwrap();
    }
    assert!(publisher.publish(snapshot(scope, &[(1, "after")]), &sink));
    let resync = receiver
        .try_iter()
        .find_map(|event| match event {
            HostEvent::PresentationSnapshot(snapshot) => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert_eq!(resync.items[0].row, TranscriptRow::assistant("after"));
    assert!(!sink.transcript_resnapshot_required());
}

#[test]
fn scopes_share_the_publisher_but_never_each_others_baseline() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    let root = TranscriptScope::Root;
    let child = TranscriptScope::Agent("c1".into());
    assert!(publisher.publish(snapshot(root.clone(), &[(1, "root")]), &sink));
    assert!(publisher.publish(snapshot(child.clone(), &[(1, "child")]), &sink));
    assert!(publisher.publish(snapshot(child.clone(), &[(1, "child changed")]), &sink));
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(events.len(), 3);
    let HostEvent::TranscriptPatch(patch) = &events[2] else {
        panic!("child patch");
    };
    assert_eq!(patch.scope, child);
    assert!(publisher.resync(&root, &sink));
    let HostEvent::PresentationSnapshot(root_snapshot) = receiver.try_recv().unwrap() else {
        panic!("root snapshot");
    };
    assert_eq!(root_snapshot.scope, root);
    assert_eq!(root_snapshot.items[0].row, TranscriptRow::assistant("root"));
    assert!(publisher.publish(snapshot(TranscriptScope::Agent("c2".into()), &[]), &sink));
    assert!(
        !publisher.resync(&child, &sink),
        "only one agent view is retained"
    );
}

#[test]
fn overflowing_an_already_truncated_scope_keeps_one_marker_and_the_root_header() {
    use plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS;
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let (sink, receiver) = observation_channel::<Core>();
        let mut publisher = Presentations::default();
        let mut initial = snapshot(scope.clone(), &[]);
        if scope == TranscriptScope::Root {
            initial.items.push(TranscriptItem {
                key: RowKey::TRUNCATION,
                row: TranscriptRow::SessionHeader {
                    session_id: "root".into(),
                    carried_from: None,
                },
            });
        }
        initial.items.push(TranscriptItem {
            key: truncation_key(&scope),
            row: TranscriptRow::Truncated,
        });
        let count = MAX_SNAPSHOT_ROWS - initial.items.len();
        initial
            .items
            .extend((1..=count).map(|index| TranscriptItem {
                key: RowKey::new(index as u64, Lane::Runtime, 0),
                row: TranscriptRow::assistant(format!("kept {index}")),
            }));
        assert!(publisher.snapshot(initial, &sink));
        let HostEvent::PresentationSnapshot(first) = receiver.try_recv().unwrap() else {
            panic!("attachment");
        };
        let mut folded = TranscriptState::default();
        folded.replace(first).unwrap();
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::AssistantDelta("new tip".into()),
            &sink
        ));
        assert!(receiver.try_recv().is_err());
        assert!(publisher.observe(scope.clone(), &HostEvent::<Core>::AssistantDone, &sink));
        let HostEvent::TranscriptPatch(patch) = receiver.try_recv().unwrap() else {
            panic!("bounded live patch");
        };
        folded.apply(patch).unwrap();
        assert_eq!(folded.items().len(), MAX_SNAPSHOT_ROWS);
        assert_eq!(
            folded
                .items()
                .values()
                .filter(|row| matches!(row, TranscriptRow::Truncated))
                .count(),
            1
        );
        assert_eq!(
            folded.items().get(&truncation_key(&scope)),
            Some(&TranscriptRow::Truncated)
        );
        assert!(
            !folded
                .items()
                .contains_key(&RowKey::new(1, Lane::Runtime, 0))
        );
        assert_eq!(
            folded
                .items()
                .values()
                .any(|row| matches!(row, TranscriptRow::SessionHeader { .. })),
            scope == TranscriptScope::Root
        );
        assert!(publisher.resync(&scope, &sink));
        let HostEvent::PresentationSnapshot(full) = receiver.try_recv().unwrap() else {
            panic!("resync");
        };
        let restored: BTreeMap<_, _> = full
            .items
            .into_iter()
            .map(|item| (item.key, item.row))
            .collect();
        assert_eq!(restored, *folded.items());
    }
}

#[test]
fn a_quiet_settlement_emits_an_empty_patch_not_a_snapshot() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    publisher.snapshot(snapshot(TranscriptScope::Root, &[(1, "unchanged")]), &sink);
    let _ = receiver.try_recv().unwrap();
    assert!(publisher.publish(snapshot(TranscriptScope::Root, &[(1, "unchanged")]), &sink));
    let HostEvent::TranscriptPatch(patch) = receiver.try_recv().unwrap() else {
        panic!("quiet settlement must patch");
    };
    assert_eq!(patch.upserts, Vec::new());
    assert_eq!(patch.deletes, Vec::new());
    assert!(patch.to_revision > patch.from_revision);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn opening_notices_are_carried_once_into_the_first_baseline() {
    let (sink, receiver) = observation_channel::<Core>();
    let mut publisher = Presentations::default();
    for scope in [NoticeScope::Recovery, NoticeScope::Session] {
        publisher.observe(
            TranscriptScope::Root,
            &HostEvent::<Core>::Row(TranscriptRow::info(scope, "opening note")),
            &sink,
        );
    }
    assert!(
        receiver.try_recv().is_err(),
        "no patch before the attachment baseline"
    );
    publisher.snapshot(snapshot(TranscriptScope::Root, &[(1, "history")]), &sink);
    let HostEvent::PresentationSnapshot(view) = receiver.try_recv().unwrap() else {
        panic!("attachment");
    };
    assert_eq!(view.items.iter().filter(|item| matches!(&item.row, TranscriptRow::Notice(notice) if notice.text == "opening note")).count(), 1);
}
#[test]
fn busy_resync_preserves_live_text_and_continues_the_same_key_in_both_scopes() {
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c1".into())] {
        let (sink, receiver) = observation_channel::<Core>();
        let mut publisher = Presentations::default();
        assert!(publisher.snapshot(snapshot(scope.clone(), &[(1, "committed")]), &sink));
        let HostEvent::PresentationSnapshot(initial) = receiver.try_recv().unwrap() else {
            panic!("initial");
        };
        let mut folded = TranscriptState::default();
        folded.replace(initial).unwrap();
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::AssistantDelta("in flight".into()),
            &sink
        ));
        assert!(receiver.try_recv().is_err(), "no per-delta patch");
        assert!(publisher.resync(&scope, &sink));
        let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
            panic!("busy resync");
        };
        assert_eq!(
            resync.items.last().unwrap().row,
            TranscriptRow::assistant("in flight")
        );
        let key = resync.items.last().unwrap().key;
        folded.replace(resync).unwrap();
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::AssistantDelta(" continues".into()),
            &sink
        ));
        assert!(publisher.observe(scope, &HostEvent::<Core>::SetBusy(false), &sink));
        let HostEvent::TranscriptPatch(delta) = receiver.try_recv().unwrap() else {
            panic!("continued patch");
        };
        assert_eq!(delta.upserts[0].key, key);
        folded.apply(delta).unwrap();
        assert_eq!(
            folded.items().get(&key),
            Some(&TranscriptRow::assistant("in flight continues"))
        );
    }
}

#[test]
fn busy_resync_preserves_running_tool_and_reasoning_identity() {
    for scope in [TranscriptScope::Root, TranscriptScope::Agent("c2".into())] {
        let (sink, receiver) = observation_channel::<Core>();
        let mut publisher = Presentations::default();
        publisher.snapshot(snapshot(scope.clone(), &[]), &sink);
        let _ = receiver.try_recv().unwrap();
        let tool = HostEvent::<Core>::Row(TranscriptRow::Tool(crate::sessions::ToolItem {
            call_id: "call".into(),
            name: "read_file".into(),
            exposed_name: None,
            kind: plakous_host_protocol::views::ActivityKind::Tool,
            state: "running".into(),
            subject: Some("file.rs".into()),
            detail: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
            started_at_ms: Some(10),
            finished_at_ms: None,
        }));
        assert!(publisher.observe(scope.clone(), &tool, &sink));
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::ReasoningDelta {
                location_key: "thought".into(),
                text: "first".into()
            },
            &sink
        ));
        let _ = receiver.try_iter().count();
        assert!(publisher.resync(&scope, &sink));
        let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
            panic!("resync");
        };
        let keys: Vec<_> = resync.items.iter().map(|item| item.key).collect();
        assert_eq!(resync.items[0].row.as_tool().unwrap().state, "running");
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::ReasoningDelta {
                location_key: "thought".into(),
                text: " second".into()
            },
            &sink
        ));
        assert!(publisher.observe(scope.clone(), &HostEvent::<Core>::SetBusy(false), &sink));
        let HostEvent::TranscriptPatch(continued) = receiver.try_recv().unwrap() else {
            panic!("thought patch");
        };
        assert_eq!(continued.upserts[0].key, keys[1]);
        assert_eq!(
            continued.upserts[0].row.as_thinking().unwrap().text,
            "first second"
        );
        assert!(publisher.observe(
            scope,
            &HostEvent::<Core>::ReasoningSealed {
                location_key: "thought".into()
            },
            &sink
        ));
        let HostEvent::TranscriptPatch(sealed) = receiver.try_recv().unwrap() else {
            panic!("seal patch");
        };
        assert_eq!(sealed.upserts[0].key, keys[1]);
        assert!(
            sealed.upserts[0]
                .row
                .as_thinking()
                .unwrap()
                .elapsed_ms
                .is_some()
        );
    }
}

#[test]
fn retry_discards_only_the_uncommitted_response_tail() {
    let (sink, receiver) = observation_channel::<Core>();
    let scope = TranscriptScope::Root;
    let mut publisher = Presentations::default();
    publisher.snapshot(snapshot(scope.clone(), &[(1, "committed")]), &sink);
    publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::AssistantDelta("partial".into()),
        &sink,
    );
    let _ = receiver.try_iter().count();
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::ModelRetry(plakous_host_protocol::ModelRetryNotice {
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 0,
            cause_kind: "stream_interrupted".into(),
            cause: "fixture".into(),
            output_discarded: true,
        }),
        &sink
    ));
    let HostEvent::TranscriptPatch(discard) = receiver.try_recv().unwrap() else {
        panic!("discard patch");
    };
    assert_eq!(discard.upserts, Vec::new());
    assert_eq!(discard.deletes.len(), 1);
    publisher.resync(&scope, &sink);
    let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
        panic!("resync");
    };
    assert_eq!(resync.items.len(), 1);
    assert_eq!(resync.items[0].row, TranscriptRow::assistant("committed"));
}

#[test]
fn output_discarded_keeps_text_of_an_earlier_model_call_in_the_turn() {
    let (sink, receiver) = observation_channel::<Core>();
    let scope = TranscriptScope::Root;
    let mut publisher = Presentations::default();
    publisher.snapshot(snapshot(scope.clone(), &[(1, "committed")]), &sink);
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::AssistantDelta("round one".into()),
        &sink,
    ));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::ModelRequestStarted,
        &sink
    ));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::AssistantDelta("round two".into()),
        &sink,
    ));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::OutputDiscarded { dim: true },
        &sink,
    ));
    let _ = receiver.try_iter().count();
    assert!(publisher.resync(&scope, &sink));
    let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
        panic!("resync");
    };
    let texts: Vec<String> = resync
        .items
        .iter()
        .map(|item| format!("{:?}", item.row))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("round one")),
        "{texts:?}"
    );
    assert!(
        texts.iter().all(|text| !text.contains("round two")),
        "{texts:?}"
    );
}

#[test]
fn e2probe_retry_keeps_text_of_an_earlier_model_call_in_the_turn() {
    let (sink, receiver) = observation_channel::<Core>();
    let scope = TranscriptScope::Root;
    let mut publisher = Presentations::default();
    publisher.snapshot(snapshot(scope.clone(), &[(1, "committed")]), &sink);
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::ModelRequestStarted,
        &sink
    ));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::AssistantDelta("round one".into()),
        &sink,
    ));
    let tool = HostEvent::<Core>::Row(TranscriptRow::Tool(crate::sessions::ToolItem {
        call_id: "call".into(),
        name: "read_file".into(),
        exposed_name: None,
        kind: plakous_host_protocol::views::ActivityKind::Tool,
        state: "running".into(),
        subject: Some("file.rs".into()),
        detail: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
        started_at_ms: Some(10),
        finished_at_ms: None,
    }));
    assert!(publisher.observe(scope.clone(), &tool, &sink));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::ModelRequestStarted,
        &sink
    ));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::AssistantDelta("round two".into()),
        &sink,
    ));
    assert!(publisher.observe(
        scope.clone(),
        &HostEvent::<Core>::ModelRetry(plakous_host_protocol::ModelRetryNotice {
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 0,
            cause_kind: "stream_interrupted".into(),
            cause: "fixture".into(),
            output_discarded: true,
        }),
        &sink,
    ));
    let _ = receiver.try_iter().count();
    assert!(publisher.resync(&scope, &sink));
    let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
        panic!("resync");
    };
    let texts: Vec<String> = resync
        .items
        .iter()
        .map(|item| format!("{:?}", item.row))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("round one")),
        "{texts:?}"
    );
    assert!(
        texts.iter().all(|text| !text.contains("round two")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("read_file")),
        "{texts:?}"
    );
}

#[test]
fn retry_and_refusal_drop_assistant_text_that_preceded_the_tool() {
    for refusal in [false, true] {
        let (sink, receiver) = observation_channel::<Core>();
        let scope = TranscriptScope::Root;
        let mut publisher = Presentations::default();
        publisher.snapshot(snapshot(scope.clone(), &[(1, "committed")]), &sink);
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::AssistantDelta("before the tool".into()),
            &sink,
        ));
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::ReasoningDelta {
                location_key: "thought".into(),
                text: "early thought".into(),
            },
            &sink,
        ));
        let tool = HostEvent::<Core>::Row(TranscriptRow::Tool(crate::sessions::ToolItem {
            call_id: "call".into(),
            name: "read_file".into(),
            exposed_name: None,
            kind: plakous_host_protocol::views::ActivityKind::Tool,
            state: "running".into(),
            subject: Some("file.rs".into()),
            detail: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
            started_at_ms: Some(10),
            finished_at_ms: None,
        }));
        assert!(publisher.observe(scope.clone(), &tool, &sink));
        assert!(publisher.observe(
            scope.clone(),
            &HostEvent::<Core>::AssistantDelta("after the tool".into()),
            &sink,
        ));
        let boundary = if refusal {
            HostEvent::<Core>::ModelRefusal { detail: None }
        } else {
            HostEvent::<Core>::ModelRetry(plakous_host_protocol::ModelRetryNotice {
                next_attempt: 2,
                max_attempts: 3,
                delay_ms: 0,
                cause_kind: "stream_interrupted".into(),
                cause: "fixture".into(),
                output_discarded: true,
            })
        };
        assert!(publisher.observe(scope.clone(), &boundary, &sink));
        let _ = receiver.try_iter().count();
        assert!(publisher.resync(&scope, &sink));
        let HostEvent::PresentationSnapshot(resync) = receiver.try_recv().unwrap() else {
            panic!("resync");
        };
        let texts: Vec<String> = resync
            .items
            .iter()
            .map(|item| format!("{:?}", item.row))
            .collect();
        assert!(
            texts.iter().any(|text| text.contains("committed")),
            "refusal={refusal} {texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.contains("read_file")),
            "refusal={refusal} {texts:?}"
        );
        assert!(
            texts.iter().all(|text| !text.contains("before the tool")
                && !text.contains("after the tool")
                && !text.contains("early thought")),
            "refusal={refusal} {texts:?}"
        );
    }
}

#[test]
fn invalid_view_repair_restores_key_order_uniqueness_and_bounds() {
    let mut invalid = snapshot(
        TranscriptScope::Root,
        &[(2, "second"), (1, "first"), (2, "last value")],
    );
    invalid.schema = u32::MAX;
    for index in 3..=plakous_host_protocol::views::presentation::MAX_SNAPSHOT_ROWS as u64 + 10 {
        invalid.items.push(TranscriptItem {
            key: RowKey::new(index, Lane::Runtime, 0),
            row: TranscriptRow::assistant("row"),
        });
    }
    let repaired = repair::snapshot(invalid);
    assert_eq!(repaired.validate(), Ok(()));
    assert_eq!(repaired.items[0].row, TranscriptRow::Truncated);
    assert!(
        repaired
            .items
            .windows(2)
            .all(|items| items[0].key < items[1].key)
    );
}

#[test]
fn randomized_publisher_mailbox_consumer_converges_across_scopes_and_clear() {
    for seed in 1_u64..=32 {
        let (sink, receiver) = observation_channel::<Core>();
        let mut publisher = Presentations::default();
        let mut consumer: BTreeMap<TranscriptScope, TranscriptState> = BTreeMap::new();
        let mut random = seed;
        for step in 0..160 {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let scope = if random % 3 == 0 {
                TranscriptScope::Root
            } else {
                TranscriptScope::Agent(format!("c{}", (random >> 32) % 3))
            };
            match random % 12 {
                0 => {
                    publisher.clear();
                    let _ = sink.send(ContentEvent::ClearView);
                }
                1 => {
                    publisher.snapshot(snapshot(scope.clone(), &[(1, "attached")]), &sink);
                }
                2 => {
                    publisher.publish(
                        snapshot(scope.clone(), &[(1, "committed"), (2, "tail")]),
                        &sink,
                    );
                }
                3 => {
                    publisher.resync(&scope, &sink);
                }
                4..=6 => {
                    if matches!(scope, TranscriptScope::Agent(_)) {
                        let _ = sink.send(ContentEvent::TranscriptStream { scope: scope.clone(), delta: plakous_host_protocol::views::transcript::TranscriptStreamDelta::Assistant(format!("{step}")) });
                    }
                    publisher.observe(
                        scope.clone(),
                        &HostEvent::<Core>::AssistantDelta(format!("{step}")),
                        &sink,
                    );
                }
                7 => {
                    publisher.observe(scope.clone(), &HostEvent::<Core>::AssistantDone, &sink);
                }
                8 => {
                    publisher.observe(
                        scope.clone(),
                        &HostEvent::<Core>::Row(TranscriptRow::info(
                            NoticeScope::Host,
                            format!("row {step}"),
                        )),
                        &sink,
                    );
                }
                9..=10 => {
                    publisher.observe(
                        scope.clone(),
                        &HostEvent::<Core>::ToolLine {
                            call_id: Some("tool".into()),
                            parent_call_id: None,
                            cell_id: None,
                            kind: plakous_host_protocol::views::ActivityKind::Tool,
                            name: "read_file".into(),
                            state: if random % 12 == 9 { "running" } else { "done" }.into(),
                            subject: None,
                            detail: Some(format!("detail {step}")),
                            duration_ms: Some(5),
                            started_at_ms: Some(1),
                            finished_at_ms: None,
                            wait: None,
                        },
                        &sink,
                    );
                }
                _ => {
                    for index in 0..=OBSERVATION_QUEUE_CAPACITY {
                        let _ = sink.send(ContentEvent::JobStarted {
                            job_id: format!("p{index}"),
                            owner: "root".into(),
                            command: "fixture".into(),
                        });
                    }
                }
            }
            if step % 7 != 0 {
                continue;
            }
            fold_delivery(&receiver, &mut consumer);
            let scopes: Vec<_> = publisher
                .states
                .iter()
                .filter(|(_, state)| state.revision() != 0)
                .map(|(scope, _)| scope.clone())
                .collect();
            for scope in scopes {
                if sink.transcript_resnapshot_required_for(&scope) {
                    assert!(
                        publisher.resync(&scope, &sink),
                        "seed={seed} step={step} scope={scope:?}"
                    );
                } else {
                    assert_eq!(
                        consumer.get(&scope),
                        publisher.states.get(&scope),
                        "convergence without resync: seed={seed} step={step} scope={scope:?}"
                    );
                }
            }
            fold_delivery(&receiver, &mut consumer);
            for (scope, state) in &publisher.states {
                if state.revision() == 0 {
                    continue;
                }
                assert_eq!(
                    consumer.get(scope),
                    Some(state),
                    "seed={seed} step={step} scope={scope:?}"
                );
            }
        }
    }
}

fn fold_delivery(
    receiver: &plakous_host_protocol::ObservationReceiver<Core>,
    consumer: &mut BTreeMap<TranscriptScope, TranscriptState>,
) {
    for event in receiver.try_iter() {
        match event {
            HostEvent::ClearView => consumer.clear(),
            HostEvent::PresentationSnapshot(snapshot) => {
                if matches!(snapshot.scope, TranscriptScope::Agent(_)) {
                    consumer.retain(|scope, _| {
                        *scope == TranscriptScope::Root || *scope == snapshot.scope
                    });
                }
                consumer
                    .entry(snapshot.scope.clone())
                    .or_default()
                    .replace(snapshot)
                    .unwrap();
            }
            HostEvent::TranscriptPatch(patch) => {
                // A scope with a missing baseline/revision waits for resync.
                let _ = consumer
                    .entry(patch.scope.clone())
                    .or_default()
                    .apply(patch);
            }
            _ => {}
        }
    }
}
