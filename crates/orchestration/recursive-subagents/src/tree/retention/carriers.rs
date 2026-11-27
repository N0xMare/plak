//! One-time proof for result carriers accepted before terminal witnesses existed.

use super::{CommandFact, ProductAgentTree};
use plakous::sessions::feed::{
    FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedRecord, FeedSeq, FeedTurnOutcome,
};
use plakous::sessions::{AgentSessions, SessionLocator};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct HistoricalCarrierBatch {
    pub(crate) recipient: String,
    pub(crate) branch: SessionLocator,
    pub(crate) cursor: u64,
    commands: Vec<HistoricalCarrier>,
}

struct HistoricalCarrier {
    id: String,
    turn: Option<String>,
    accepted: u64,
}

impl HistoricalCarrierBatch {
    pub(crate) async fn certify(mut self, sessions: &AgentSessions) -> (bool, Option<Self>) {
        if sessions
            .quiescent_head_at_feed_cursor(self.branch, self.cursor)
            .await
            .is_ok_and(|proved| proved)
        {
            return (true, Some(self));
        }
        let total = self.commands.len();
        let session = self.branch.session_id();
        let mut proved = Vec::new();
        for command in std::mem::take(&mut self.commands) {
            if terminal_was_projected(sessions, session, &command, self.cursor).await {
                proved.push(command);
            }
        }
        let all = proved.len() == total;
        self.commands = proved;
        (all, (!self.commands.is_empty()).then_some(self))
    }
}

/// The feed reader authenticates the committed record, including its sequence
/// and terminal outcome. No terminal payload or checkpoint history is needed.
async fn terminal_was_projected(
    sessions: &AgentSessions,
    session: plakous::model::SessionId,
    command: &HistoricalCarrier,
    cursor: u64,
) -> bool {
    let Some(turn) = command.turn.as_deref().and_then(|id| id.parse().ok()) else {
        return false;
    };
    let mut after = command.accepted - 1;
    let mut scan = TerminalScan {
        command,
        turn,
        cursor,
        accepted: false,
    };
    for _ in 0..4 {
        let Ok(FeedPage::Records { records, .. }) = sessions
            .feed_page(FeedPageRequest::after(FeedCursor::new(
                session,
                FeedSeq::new(after),
            )))
            .await
        else {
            return false;
        };
        if records.is_empty() {
            break;
        }
        for record in records {
            after = record.header().seq.get();
            if let Some(proved) = scan.observe(&record) {
                return proved;
            }
        }
        if after == cursor {
            break;
        }
    }
    false
}

struct TerminalScan<'a> {
    command: &'a HistoricalCarrier,
    turn: plakous::model::TurnId,
    cursor: u64,
    accepted: bool,
}

impl TerminalScan<'_> {
    fn observe(&mut self, record: &FeedRecord) -> Option<bool> {
        let seq = record.header().seq.get();
        if seq > self.cursor {
            return Some(false);
        }
        for fact in record.facts() {
            match *fact {
                FeedFact::CommandAccepted {
                    ref command_id,
                    turn_id,
                    ..
                } if seq == self.command.accepted
                    && command_id.as_str() == self.command.id
                    && turn_id == Some(self.turn) =>
                {
                    self.accepted = true;
                }
                FeedFact::TurnTerminal {
                    ref command_id,
                    turn_id,
                    outcome,
                    ..
                } if self.accepted
                    && command_id.as_str() == self.command.id
                    && turn_id == self.turn =>
                {
                    match outcome {
                        FeedTurnOutcome::RecoveryRequired => {}
                        FeedTurnOutcome::Failed {
                            context: Some(context),
                            ..
                        } if context.withheld => return Some(false),
                        _ => return Some(true),
                    }
                }
                _ => {}
            }
        }
        (!self.accepted).then_some(false)
    }
}

impl ProductAgentTree {
    pub(crate) fn historical_carriers(
        &self,
        now: u64,
        max_age: Option<std::time::Duration>,
        root: Option<SessionLocator>,
        occupied: &BTreeSet<String>,
    ) -> Vec<HistoricalCarrierBatch> {
        let mut due: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let due_children = self.due_children(now, max_age);
        for agent in &due_children {
            if !occupied.contains(agent)
                && let Some(source) = self.retirement_source(agent)
            {
                due.entry(source.parent)
                    .or_default()
                    .insert(&source.mail.message_id);
            }
        }

        let mut batches = BTreeMap::new();
        for command in self.commands.values() {
            if command.fact != Some(CommandFact::Accepted)
                || self.carrier_has_terminal(command)
                || !due.get(command.recipient.as_str()).is_some_and(|mail| {
                    command
                        .message_ids
                        .iter()
                        .any(|id| mail.contains(id.as_str()))
                })
            {
                continue;
            }
            let Some(accepted) = command.accepted_seq.filter(|seq| *seq != 0) else {
                continue;
            };
            let Some((branch, cursor)) = self.carrier_position(&command.recipient, root) else {
                continue;
            };
            if accepted <= cursor {
                batches
                    .entry(&command.recipient)
                    .or_insert_with(|| HistoricalCarrierBatch {
                        recipient: command.recipient.clone(),
                        branch,
                        cursor,
                        commands: Vec::new(),
                    })
                    .commands
                    .push(HistoricalCarrier {
                        id: command.command_id.clone(),
                        turn: command.turn_id.clone(),
                        accepted,
                    });
            }
        }
        batches.into_values().collect()
    }

    fn carrier_position(
        &self,
        recipient: &str,
        root: Option<SessionLocator>,
    ) -> Option<(SessionLocator, u64)> {
        let node = self.nodes.get(recipient)?;
        let branch = if recipient == self.root_agent_id {
            root
        } else {
            node.session_locator.as_deref()?.parse().ok()
        }?;
        Some((branch, self.feed_cursor(&branch.session_id().to_string())))
    }

    /// Called under the tree writer only after the runtime proved this head at the
    /// sampled cursor. A replay or new binding between the read and save wins.
    pub(crate) fn apply_historical_carriers(
        &mut self,
        batch: &HistoricalCarrierBatch,
        root: Option<SessionLocator>,
    ) {
        if self.carrier_position(&batch.recipient, root) != Some((batch.branch, batch.cursor)) {
            return;
        }
        for proof in &batch.commands {
            if self.commands.get(&proof.id).is_some_and(|command| {
                command.recipient == batch.recipient
                    && command.fact == Some(CommandFact::Accepted)
                    && command.accepted_seq == Some(proof.accepted)
                    && command.turn_id == proof.turn
                    && !self.carrier_has_terminal(command)
            }) {
                self.record_carrier_terminal(
                    &batch.recipient,
                    &proof.id,
                    proof.turn.as_deref().unwrap_or_default(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::agent::{AgentErrorKind, CommandId, TurnUsage};
    use plakous::model::{FinishReason, TurnOrigin, Usage};
    use plakous::sessions::feed::{FeedChange, FeedCommandKind, FeedHeader};

    fn record(seq: u64, fact: FeedFact) -> FeedRecord {
        let locator: SessionLocator = concat!(
            "00000000-0000-4000-8000-000000000001/",
            "00000000-0000-4000-8000-000000000002/",
            "00000000-0000-4000-8000-000000000003"
        )
        .parse()
        .unwrap();
        FeedRecord::from_store(
            FeedHeader {
                session_id: locator.session_id(),
                branch_id: locator.branch_id(),
                seq: FeedSeq::new(seq),
                committed_at_ms: seq,
                change: FeedChange::LeaseReleased {
                    head: locator.checkpoint_id(),
                    fence: 1,
                },
            },
            vec![fact],
        )
    }

    #[test]
    fn historical_child_retirement_rejects_unprojected_or_returning_terminals() {
        let turn: plakous::model::TurnId = "00000000-0000-4000-8000-000000000004".parse().unwrap();
        let command = HistoricalCarrier {
            id: "carrier".to_owned(),
            turn: Some(turn.to_string()),
            accepted: 10,
        };
        let accepted = record(
            10,
            FeedFact::CommandAccepted {
                command_id: CommandId::new("carrier").unwrap(),
                kind: FeedCommandKind::Root {
                    origin: TurnOrigin::UserPrompt,
                },
                turn_id: Some(turn),
                sequence: 1,
                operation_id: None,
            },
        );
        let terminal = |seq, id, outcome| {
            record(
                seq,
                FeedFact::TurnTerminal {
                    turn_id: turn,
                    command_id: CommandId::new(id).unwrap(),
                    outcome,
                    closed_exchange: true,
                    steering_settled_unapplied: false,
                    usage: TurnUsage::new(Usage::new(0, 0, 0, 0, 0), 0, 0, 0).unwrap(),
                },
            )
        };
        let completed = FeedTurnOutcome::Completed {
            finish: FinishReason::Stop,
        };
        let withheld = FeedTurnOutcome::Failed {
            kind: AgentErrorKind::Model,
            model_kind: None,
            context: Some(
                serde_json::from_str(r#"{"withheld":true,"remedy":"resend_shorter"}"#).unwrap(),
            ),
        };
        for (seq, id, outcome, expected) in [
            (12, "carrier", completed, Some(true)),
            (13, "carrier", completed, Some(false)),
            (12, "other", completed, None),
            (12, "carrier", FeedTurnOutcome::RecoveryRequired, None),
            (12, "carrier", withheld, Some(false)),
        ] {
            let mut scan = TerminalScan {
                command: &command,
                turn,
                cursor: 12,
                accepted: false,
            };
            assert_eq!(scan.observe(&accepted), None);
            assert_eq!(scan.observe(&terminal(seq, id, outcome)), expected);
        }
    }
}
