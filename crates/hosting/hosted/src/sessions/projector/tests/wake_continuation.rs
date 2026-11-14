//! Only an admitted, recorded loss continuation rearms suppressed wakes.

use super::*;

fn record_handoff(feed: &mut Feed, anchor: u64) {
    feed.intents.root_continuations.push(AnchoredIntent {
        anchor,
        ordinal: 1,
        value: RootContinuation {
            anchor: RootStopAnchor::Turn { turn_id: uuid(99) },
            host_generation: 2,
            cause: RootContinuationCause::Handoff,
            round: 1,
            decided_at_ms: 1,
            decision: RootContinuationDecision::NewTurn {
                command_id: "continued".into(),
                withdrawn: Vec::new(),
            },
        },
    });
}

fn accept_continuation(feed: &mut Feed) {
    feed.advance(
        vec![FeedFact::CommandAccepted {
            command_id: CommandId::new("continued").unwrap(),
            kind: FeedCommandKind::Root {
                origin: TurnOrigin::HostWake,
            },
            turn_id: Some(id(100)),
            sequence: 1,
            operation_id: None,
        }],
        Reads::default(),
    );
}

#[test]
fn an_interrupt_after_the_loss_decision_keeps_wakes_suppressed() {
    for (offset, ordinal) in [(0, 0), (0, 2), (1, 0), (2, 0)] {
        let mut feed = Feed::new();
        let anchor = feed.seq;
        record_handoff(&mut feed, anchor);
        let suppression = AnchoredIntent {
            anchor: anchor + offset,
            ordinal,
            value: (),
        };
        if offset < 2 {
            feed.intents.wake_suppressions.push(suppression.clone());
        }
        // The last case is ordered after acceptance, not just after the
        // decision: it must not change the earlier fact when rebuilding.
        feed.advance(Vec::new(), Reads::default());
        let before = project_all(&feed).views;
        accept_continuation(&mut feed);
        let incremental = project(before, &feed.intents, &feed.records, &feed.reads)
            .unwrap()
            .views;
        if offset == 2 {
            feed.intents.wake_suppressions.push(suppression);
        }
        let rebuilt = project_all(&feed).views;
        assert_eq!(
            rebuilt.wake_suppressed(&feed.intents),
            (offset, ordinal) != (0, 0),
            "suppression offset={offset}, ordinal={ordinal}"
        );
        assert_eq!(
            project(incremental, &feed.intents, &feed.records, &feed.reads)
                .unwrap()
                .views,
            rebuilt
        );
    }
}

#[test]
fn a_loss_decision_at_or_after_acceptance_cannot_rearm_wakes() {
    for offset in [0, 1] {
        let mut feed = Feed::new();
        feed.intents.wake_suppressions.push(AnchoredIntent {
            anchor: feed.seq,
            ordinal: 0,
            value: (),
        });
        let decision_anchor = feed.seq + 1 + offset;
        record_handoff(&mut feed, decision_anchor);
        let before = project_all(&feed).views;
        accept_continuation(&mut feed);
        let rebuilt = project_all(&feed).views;
        assert!(rebuilt.wake_suppressed(&feed.intents), "offset={offset}");
        assert_eq!(
            project(before, &feed.intents, &feed.records, &feed.reads)
                .unwrap()
                .views,
            rebuilt
        );
    }
}

#[test]
fn loss_continuation_acceptance_rearms_wakes_but_a_later_stop_still_wins() {
    for cause in [
        None,
        Some(RootContinuationCause::Handoff),
        Some(RootContinuationCause::OwnerLost),
        Some(RootContinuationCause::Transient),
    ] {
        for matches in [false, true] {
            let mut feed = Feed::new();
            let anchor = feed.seq;
            feed.intents.wake_suppressions.push(AnchoredIntent {
                anchor,
                ordinal: 0,
                value: (),
            });
            if let Some(cause) = cause {
                feed.intents.root_continuations.push(AnchoredIntent {
                    anchor,
                    ordinal: 1,
                    value: RootContinuation {
                        anchor: RootStopAnchor::Turn { turn_id: uuid(99) },
                        host_generation: 2,
                        cause,
                        round: 1,
                        decided_at_ms: 1,
                        decision: RootContinuationDecision::NewTurn {
                            command_id: if matches { "continued" } else { "another" }.into(),
                            withdrawn: Vec::new(),
                        },
                    },
                });
            }
            let before = project_all(&feed).views;
            assert!(before.wake_suppressed(&feed.intents));
            feed.advance(
                vec![FeedFact::CommandAccepted {
                    command_id: CommandId::new("continued").unwrap(),
                    kind: FeedCommandKind::Root {
                        origin: TurnOrigin::HostWake,
                    },
                    turn_id: Some(id(100)),
                    sequence: 1,
                    operation_id: None,
                }],
                Reads::default(),
            );
            let rearmed = matches
                && matches!(
                    cause,
                    Some(RootContinuationCause::Handoff | RootContinuationCause::OwnerLost)
                );
            let projected = project_all(&feed).views;
            assert_eq!(
                project(before, &feed.intents, &feed.records, &feed.reads)
                    .unwrap()
                    .views,
                projected,
                "incremental projection matches rebuild"
            );
            assert_eq!(
                projected.wake_suppressed(&feed.intents),
                !rearmed,
                "cause={cause:?} matches={matches}"
            );
            feed.intents.wake_suppressions.push(AnchoredIntent {
                anchor: feed.seq,
                ordinal: 0,
                value: (),
            });
            assert!(projected.wake_suppressed(&feed.intents), "later Stop wins");
            assert!(project_all(&feed).views.wake_suppressed(&feed.intents));
        }
    }
}
