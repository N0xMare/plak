//! Runtime command intents and the tree projector's feed cursors (spec 03
//! §14, §14.3).
//!
//! Every runtime command the tree prepares is written here before it is
//! submitted: a root admission's result-mail batch, or a child Start, wake,
//! mail Start, followup or corrective round. The intent fields never change.
//! What became of the command is a view the tree projector derives from the
//! recipient's change feed (`CommandAccepted`, `CommandRejected`, a withheld
//! `TurnTerminal`); withdrawal is the tree's own decision for a command that
//! has no fact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{ProductAgentTree, TopologyError, TopologyErrorCode};
use crate::protocol::AttemptId;

/// The most command intents one tree holds; exhaustion refuses admission
/// rather than evicting an unresolved command (spec 03 §8).
pub const MAX_TREE_COMMANDS: usize = 4096;

/// Which runtime command an intent prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCommandKind {
    /// A root admission's result-mail batch, keyed by the product's command id.
    RootBatch,
    /// A child's first Start.
    Start,
    /// A parked or reopened child's wake.
    Wake,
    /// A Start a mail delivery admitted.
    MailStart,
    /// A followup's Start.
    Followup,
    /// A submit-harness corrective round.
    Corrective,
    /// A new turn continuing after a committed transient model failure.
    Resume,
}

/// What the recipient's feed recorded for a command: a view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandFact {
    /// `CommandAccepted`: its batch was acknowledged.
    Accepted,
    /// `CommandRejected`: nothing was consumed.
    Rejected,
    /// Accepted, then its turn withheld it before dispatch: the batch went
    /// back to unread.
    Returned,
}

/// One `agent_commands` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCommand {
    /// The runtime command id.
    pub command_id: String,
    /// The agent whose session the command was submitted to.
    pub recipient: String,
    /// The attempt a child command serves; `None` for a root batch.
    pub attempt_id: Option<AttemptId>,
    /// Which command it is.
    pub kind: AgentCommandKind,
    /// The exact canonical text submitted under the command id.
    pub text: String,
    /// The mail the command's batch carries, in mailbox order.
    pub message_ids: Vec<String>,
    /// The tree withdrew the command before any fact named it (intent).
    pub withdrawn: bool,
    /// What the recipient's feed recorded for it (view).
    pub fact: Option<CommandFact>,
    /// The turn a `CommandAccepted` bound it to (view).
    pub turn_id: Option<String>,
    /// Exact feed sequence of the acceptance, retained in the fact view.
    pub accepted_seq: Option<u64>,
}

impl AgentCommand {
    /// A fresh intent with no fact.
    #[must_use]
    pub fn intent(
        command_id: impl Into<String>,
        recipient: impl Into<String>,
        attempt_id: Option<AttemptId>,
        kind: AgentCommandKind,
        text: impl Into<String>,
        message_ids: Vec<String>,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            recipient: recipient.into(),
            attempt_id,
            kind,
            text: text.into(),
            message_ids,
            withdrawn: false,
            fact: None,
            turn_id: None,
            accepted_seq: None,
        }
    }

    /// The intent fields alone: what a resubmission must match exactly.
    fn same_intent(&self, other: &Self) -> bool {
        self.command_id == other.command_id
            && self.recipient == other.recipient
            && self.attempt_id == other.attempt_id
            && self.kind == other.kind
            && self.text == other.text
            && self.message_ids == other.message_ids
    }

    /// Neither a fact nor a withdrawal has settled it.
    #[must_use]
    pub const fn unresolved(&self) -> bool {
        self.fact.is_none() && !self.withdrawn
    }
}

impl ProductAgentTree {
    /// Remembers which unresolved commands a behind-retention pass cannot
    /// prove unrecorded. Catching the cursor up must not erase that knowledge.
    ///
    /// # Errors
    ///
    /// Returns an intent encoding refusal.
    pub fn mark_commands_behind_retention(&mut self, recipient: &str) -> Result<(), TopologyError> {
        let commands: Vec<_> = self
            .commands
            .values()
            .filter(|command| command.recipient == recipient && command.unresolved())
            .map(|command| command.command_id.clone())
            .collect();
        for command_id in commands {
            let id = format!("plak:behind-retention:{command_id}");
            if self.operations.contains_key(&id) {
                continue;
            }
            let canonical = super::canonical_json(&command_id)?;
            self.operations.insert(
                id.clone(),
                super::OperationRecord {
                    fold: None,
                    operation_id: id,
                    caller_agent_id: recipient.to_owned(),
                    tool_name: "behind_retention_command".to_owned(),
                    request_digest: super::digest_text(&canonical),
                    canonical_request: canonical,
                    terminal_code: None,
                    terminal_response: Some(command_id),
                },
            );
            self.bump();
        }
        Ok(())
    }

    /// Whether the command has to be resubmitted under its identity because
    /// its original feed record may already have left retention.
    #[must_use]
    pub fn command_behind_retention(&self, command_id: &str) -> bool {
        self.operations
            .contains_key(&format!("plak:behind-retention:{command_id}"))
    }

    pub(crate) fn remove_command_retention_marker(&mut self, command_id: &str) {
        self.operations
            .remove(&format!("plak:behind-retention:{command_id}"));
    }

    /// Writes a command intent before its submission. Writing the same intent
    /// again is a no-op, so a resubmission reuses the stored batch and bytes.
    ///
    /// # Errors
    ///
    /// Refuses a different intent under an existing command id
    /// (`operation_recovery_required`), mail addressed to another agent, and
    /// a tree already holding [`MAX_TREE_COMMANDS`] intents (`capacity`).
    pub fn record_command(&mut self, command: AgentCommand) -> Result<(), TopologyError> {
        if let Some(stored) = self.commands.get(&command.command_id) {
            if stored.same_intent(&command) {
                return Ok(());
            }
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "command id conflicts with a different stored command",
            ));
        }
        if self.commands.len() >= MAX_TREE_COMMANDS {
            return Err(TopologyError::new(
                TopologyErrorCode::Capacity,
                "the tree holds its maximum number of runtime commands",
            ));
        }
        if command
            .message_ids
            .iter()
            .filter_map(|id| self.messages.get(id))
            .any(|mail| mail.to != command.recipient)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "command batch mail is addressed to another agent",
            ));
        }
        let fresh = AgentCommand {
            withdrawn: false,
            fact: None,
            turn_id: None,
            accepted_seq: None,
            ..command
        };
        self.commands.insert(fresh.command_id.clone(), fresh);
        self.bump();
        Ok(())
    }

    /// One command intent.
    #[must_use]
    pub fn command(&self, command_id: &str) -> Option<&AgentCommand> {
        self.commands.get(command_id)
    }

    /// Every command intent, by command id.
    #[must_use]
    pub const fn commands(&self) -> &BTreeMap<String, AgentCommand> {
        &self.commands
    }

    /// Withdraws a command no fact has settled: its mail stays unread and it
    /// is never submitted again. Returns whether this call withdrew it.
    pub fn withdraw_command(&mut self, command_id: &str) -> bool {
        let Some(command) = self.commands.get_mut(command_id) else {
            return false;
        };
        if !command.unresolved() {
            return false;
        }
        command.withdrawn = true;
        self.remove_command_retention_marker(command_id);
        self.bump();
        true
    }

    /// The tree side of the product-to-tree join at open (spec 03 §14.3):
    /// after open applied the root's feed to its tail, the product names the
    /// command ids of its pending root submissions, and every root batch that
    /// is neither named nor recorded is withdrawn with its mail unread. A
    /// discarded host-wake submission is never named, so its batch goes.
    /// Returns the withdrawn command ids.
    pub fn join_pending_root_commands(&mut self, pending_command_ids: &[String]) -> Vec<String> {
        let root = self.root_agent_id.clone();
        let stale: Vec<String> = self
            .commands
            .values()
            .filter(|command| {
                command.kind == AgentCommandKind::RootBatch
                    && command.recipient == root
                    && command.unresolved()
                    && !pending_command_ids.contains(&command.command_id)
            })
            .map(|command| command.command_id.clone())
            .collect();
        for id in &stale {
            self.withdraw_command(id);
        }
        stale
    }

    /// The last feed sequence the tree projector applied for a runtime
    /// session; 0 before any.
    #[must_use]
    pub fn feed_cursor(&self, session_id: &str) -> u64 {
        self.feed_cursors.get(session_id).copied().unwrap_or(0)
    }

    /// Every feed cursor, by runtime session id.
    #[must_use]
    pub const fn feed_cursors(&self) -> &BTreeMap<String, u64> {
        &self.feed_cursors
    }

    /// Places a session's cursor just below its retained feed, for a tree
    /// that must rebuild from a feed behind its retention (spec 03 §14.1).
    /// Never moves a cursor backwards.
    pub fn seed_feed_cursor(&mut self, session_id: &str, below_retained: u64) {
        let cursor = self.feed_cursors.entry(session_id.to_owned()).or_default();
        if below_retained > *cursor {
            *cursor = below_retained;
            self.bump();
        }
    }

    /// Deletes the command views and every feed cursor, so the next
    /// projector pass re-derives them from each session's feed (spec 03
    /// §14.3). The mail an accepted batch acknowledged returns to
    /// `accepted`, and the `returned_admission` receipts of command batches
    /// go; replaying the feed acknowledges and returns them again.
    ///
    /// Command-bound attempts lose their binding, active flag and turn count;
    /// the feed re-derives each distinct turn once, in order. Harness rounds
    /// are cleared and rebuilt from accepted corrective commands. The tool calls
    /// and tokens each child `TurnTerminal` adds are zeroed and summed again by the
    /// replay; the elapsed time only grows, so it stays.
    /// An allocated child's locator is cleared and rebuilt from its immutable
    /// session association and that branch's feed headers.
    pub fn clear_feed_views(&mut self) {
        self.clear_scoped_feed_views(None);
    }

    pub(in crate::tree) fn clear_agent_feed_views(&mut self, agent: &str, session: &str) {
        self.clear_scoped_feed_views(Some((agent, session)));
    }

    fn clear_scoped_feed_views(&mut self, scope: Option<(&str, &str)>) {
        self.compaction_views.retain(|id, _| {
            scope.is_some_and(|(agent, _)| {
                self.attempts
                    .get(id)
                    .is_none_or(|attempt| attempt.agent_id != agent)
            })
        });
        if let Some((_, session)) = scope {
            self.compaction_cursors.remove(session);
        } else {
            self.compaction_cursors.clear();
        }
        let retired: std::collections::BTreeSet<_> = self
            .nodes
            .keys()
            .filter(|agent| self.conversation_retired(agent))
            .cloned()
            .collect();
        let selected =
            |agent: &str| !retired.contains(agent) && scope.is_none_or(|(id, _)| id == agent);
        for operation in self.operations.values() {
            if operation.tool_name == super::projector::CHILD_SESSION_KIND
                && selected(&operation.caller_agent_id)
                && let Some(node) = self.nodes.get_mut(&operation.caller_agent_id)
            {
                node.session_locator = None;
            }
        }
        let mut reset = Vec::new();
        reset.extend(
            self.failed_wake_mail_ids()
                .into_iter()
                .filter(|id| self.messages.get(id).is_some_and(|mail| selected(&mail.to))),
        );
        for command in self
            .commands
            .values_mut()
            .filter(|command| selected(&command.recipient))
        {
            if command.fact == Some(CommandFact::Accepted) {
                reset.extend(command.message_ids.iter().cloned());
            }
            command.fact = None;
            command.turn_id = None;
            command.accepted_seq = None;
        }
        for id in reset {
            if let Some(mail) = self.messages.get_mut(&id)
                && mail.durable == super::MailDurableState::Acknowledged
            {
                mail.durable = super::MailDurableState::Accepted;
                let ids = self.inbound.entry(mail.to.clone()).or_default();
                if !ids.contains(&id) {
                    ids.push(id);
                    ids.sort_by(|left, right| {
                        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                    });
                }
            }
        }
        // A return is recorded against its command id by the record that
        // derived it, so replaying that record records it again.
        let commands = &self.commands;
        self.operations.retain(|id, operation| {
            operation.tool_name != super::RETURNED_ADMISSION_KIND
                || !commands
                    .get(id)
                    .is_some_and(|command| selected(&command.recipient))
        });
        for attempt in self
            .attempts
            .values_mut()
            .filter(|attempt| selected(&attempt.agent_id))
        {
            attempt.status = super::AttemptStatus::Running;
            attempt.output_json = None;
            attempt.candidate_id = None;
            attempt.ended_at = None;
            attempt.failure = None;
            attempt.blocked = None;
            attempt.turn_id = None;
            attempt.applied_terminal_read_seqs.clear();
            attempt.usage.turns = 0;
            attempt.submit_harness_nudge = super::SubmitNudgeBudget::NONE;
            attempt.usage.tool_calls = 0;
            attempt.usage.input_tokens = 0;
            attempt.usage.output_tokens = 0;
        }
        for node in self
            .nodes
            .values_mut()
            .filter(|node| selected(&node.agent_id))
        {
            node.fold_order = None;
            if node.agent_id != self.root_agent_id {
                if let Some(first) = self
                    .attempts
                    .values()
                    .find(|attempt| attempt.agent_id == node.agent_id && attempt.retry_of.is_none())
                {
                    node.attempt_id = Some(first.attempt_id.clone());
                    node.output_schema_json
                        .clone_from(&first.output_schema_json);
                    node.schema_digest.clone_from(&first.schema_digest);
                }
                node.active_turn = false;
                node.session_locator = None;
                node.status = super::NodeStatus::Running;
            }
        }
        if let Some((_, session)) = scope {
            self.feed_cursors.remove(session);
        } else {
            self.feed_cursors.clear();
        }
        self.bump();
    }
}
