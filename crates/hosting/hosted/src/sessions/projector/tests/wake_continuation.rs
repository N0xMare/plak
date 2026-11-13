//! Only an admitted, recorded loss continuation rearms suppressed wakes.

use super::*;

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
