//! Parked children: an assignee whose turn ended without `submit_result`
//! while it still owned live work (spec 03 §5.2).
//!
//! A parked child keeps its attempt `running` and its live session, has no
//! bound turn, and is woken by the host when tracked work changes. The parked
//! map is process-local, like `parked_waits`: on reopen the parked shape is
//! derived from durable rows instead ([`ProductAgentTree::parked_reopen_targets`]).

use serde::Serialize;

use super::{AttemptStatus, MailRecord, NodeStatus, ProductAgentTree, TopologyError};
use crate::protocol::TopologyErrorCode;

/// What a parked child waits on, as `wait_agent` and `list_agents` rows show it.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct WaitingOn {
    /// Content-free route readiness for a pending transient continuation.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub network: bool,
    /// The child's own running processes, as `p<N>` ids.
    pub processes: Vec<String>,
    /// Its own children that are still live work.
    pub agents: Vec<String>,
}

/// One parked child, process-local.
#[derive(Clone, Debug)]
pub(crate) struct ParkedChild {
    waiting_on: WaitingOn,
    state: ParkState,
    continuation: bool,
}

/// Where a parked child is between its ended turn and its next one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParkState {
    /// Waiting on its tracked work.
    Parked,
    /// The host is admitting the wake turn: mail still waits for the next
    /// boundary rather than racing the Start.
    Waking,
    /// The wake was not admitted and the host is ending the attempt: new mail
    /// is refused as terminal and an interrupt no longer applies.
    Ending,
}

impl ProductAgentTree {
    /// Current node-local admission, read from its retained port rather than
    /// picker cache or the root's placement. No runner call is made here.
    #[must_use]
    pub fn placement_ready(&self, agent_id: &str) -> bool {
        self.nodes
            .get(agent_id)
            .is_some_and(|node| self.vcs.0.placement_ready(&node.path).unwrap_or(true))
    }

    /// The caller's children that will still report on their own: open
    /// attempts on a node that runs (or will once resumed from a reopen), and
    /// nodes sealing a result they have not pushed yet. Interrupted and
    /// recovery-required children wait on someone else and are not counted.
    #[must_use]
    pub fn live_child_work(&self, agent_id: &str) -> Vec<String> {
        self.nodes
            .values()
            .filter(|node| node.parent_agent_id.as_deref() == Some(agent_id))
            .filter(|node| match node.status {
                NodeStatus::Sealing => true,
                NodeStatus::Running | NodeStatus::Pending => self
                    .current_attempt(&node.agent_id)
                    .is_some_and(|attempt| !attempt.status.is_stopped()),
                _ => false,
            })
            .map(|node| node.agent_id.clone())
            .collect()
    }

    /// Unread mail for `agent_id` that no live delivery has claimed: what a
    /// parked child's wake carries.
    #[must_use]
    pub fn wake_mail(&self, agent_id: &str) -> Vec<MailRecord> {
        self.unread_mail(agent_id)
            .filter(|mail| !self.delivery_claims.contains(&mail.message_id))
            .cloned()
            .collect()
    }

    /// Unread mail for `agent_id` that a live delivery still holds. Its
    /// delivery may yet find the turn ended and leave it unread, so a turn
    /// end treats it as an arrival that has not landed yet.
    #[must_use]
    pub fn claimed_mail(&self, agent_id: &str) -> Vec<String> {
        self.unread_mail(agent_id)
            .filter(|mail| self.delivery_claims.contains(&mail.message_id))
            .map(|mail| mail.message_id.clone())
            .collect()
    }

    /// Holds or releases a live delivery claim on `message_id`, as the
    /// delivery of `send_agent_message` does around its deliverer.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_delivery_claim_for_test(&mut self, message_id: &str, claimed: bool) {
        if claimed {
            self.delivery_claims.insert(message_id.to_owned());
        } else {
            self.delivery_claims.remove(message_id);
        }
        self.waiters().notify_waiters();
    }

    /// Parks `agent_id`: releases live ownership and records what it waits on.
    /// The preceding terminal feed pass already released its binding and wait
    /// receipts. The attempt stays `running` and its nudge count is untouched.
    ///
    /// # Errors
    /// Refuses a node that is not running an open, unsubmitted attempt.
    pub fn park_child(
        &mut self,
        agent_id: &str,
        processes: Vec<String>,
    ) -> Result<WaitingOn, TopologyError> {
        let parkable = self
            .nodes
            .get(agent_id)
            .is_some_and(|node| node.status == NodeStatus::Running)
            && self.current_attempt(agent_id).is_some_and(|attempt| {
                attempt.status == AttemptStatus::Running && attempt.output_json.is_none()
            });
        if !parkable {
            return Err(TopologyError::new(
                TopologyErrorCode::RecipientAttemptTerminal,
                "only a running, unsubmitted attempt can park",
            ));
        }
        self.release_turn_owner(agent_id);
        let waiting_on = WaitingOn {
            network: false,
            processes,
            agents: self.live_child_work(agent_id),
        };
        self.parked.insert(
            agent_id.to_owned(),
            ParkedChild {
                waiting_on: waiting_on.clone(),
                state: ParkState::Parked,
                continuation: false,
            },
        );
        self.bump();
        Ok(waiting_on)
    }

    /// Refreshes what a parked child waits on. Rows only; nothing is woken.
    pub fn refresh_parked(&mut self, agent_id: &str, processes: Vec<String>) {
        let agents = self.live_child_work(agent_id);
        if let Some(parked) = self.parked.get_mut(agent_id) {
            parked.waiting_on = WaitingOn {
                network: false,
                processes,
                agents,
            };
        }
    }

    /// Marks the wake turn as being admitted, so mail keeps waiting. Returns
    /// whether the child is still parked on a running node: an interrupt or
    /// close that won the race ended the park, and no wake may start.
    pub fn begin_wake(&mut self, agent_id: &str) -> bool {
        if !self.placement_ready(agent_id) {
            return false;
        }
        let running = self
            .nodes
            .get(agent_id)
            .is_some_and(|node| node.status == NodeStatus::Running);
        match self.parked.get_mut(agent_id) {
            Some(parked) if running && parked.state == ParkState::Parked => {
                parked.state = ParkState::Waking;
                true
            }
            _ => false,
        }
    }

    /// Returns an unadmitted wake to its park after a placement race. An
    /// interrupt or an ending attempt is never undone.
    pub fn defer_wake(&mut self, agent_id: &str) {
        if let Some(parked) = self.parked.get_mut(agent_id)
            && parked.state == ParkState::Waking
        {
            parked.state = ParkState::Parked;
        }
    }

    /// Whether a wake begun by [`Self::begin_wake`] may still start: nothing
    /// ended the park since.
    #[must_use]
    pub fn wake_admissible(&self, agent_id: &str) -> bool {
        self.placement_ready(agent_id)
            && self
                .parked
                .get(agent_id)
                .is_some_and(|parked| parked.state == ParkState::Waking)
            && self
                .nodes
                .get(agent_id)
                .is_some_and(|node| node.status == NodeStatus::Running)
    }

    /// Marks a parked child whose wake was not admitted as ending its
    /// attempt. Returns `false`, changing nothing, when the park already
    /// ended (an interrupt won the race and keeps the attempt).
    pub fn mark_park_ending(&mut self, agent_id: &str) -> bool {
        let running = self
            .nodes
            .get(agent_id)
            .is_some_and(|node| node.status == NodeStatus::Running);
        match self.parked.get_mut(agent_id) {
            Some(parked) if running => {
                parked.state = ParkState::Ending;
                self.bump();
                true
            }
            _ => false,
        }
    }

    /// Whether `agent_id`'s attempt is ending after a wake that was not
    /// admitted: mail to it can no longer be delivered.
    #[must_use]
    pub fn is_park_ending(&self, agent_id: &str) -> bool {
        self.parked
            .get(agent_id)
            .is_some_and(|parked| parked.state == ParkState::Ending)
    }

    /// Ends the park for an interrupt. Returns whether the child was parked,
    /// held or waking; an ending child is past its last turn and stays as it
    /// is.
    pub(super) fn interrupt_park(&mut self, agent_id: &str) -> bool {
        match self.parked.get(agent_id).map(|parked| parked.state) {
            Some(ParkState::Ending) | None => false,
            Some(_) => {
                self.parked.remove(agent_id);
                true
            }
        }
    }

    /// Forgets a parked child once its wake turn is bound or the park ended.
    pub fn unpark_child(&mut self, agent_id: &str) {
        self.parked.remove(agent_id);
    }

    /// Labels a live continuation gate without adding persistence authority.
    pub fn mark_transient_wait(&mut self, agent_id: &str, network: bool) {
        if let Some(parked) = self.parked.get_mut(agent_id) {
            parked.waiting_on.network = network;
            parked.continuation = true;
            self.bump();
        }
    }

    /// Whether the host owns this child's next turn: a parked, held,
    /// reopening, waking or ending child's mail is not delivered live.
    #[must_use]
    pub fn is_parked(&self, agent_id: &str) -> bool {
        self.parked.contains_key(agent_id)
    }

    /// What a parked child waits on, while it still runs parked or is held
    /// for its reopen wake (then empty: what it waits on is not yet known).
    /// Rows render it as the explicit parked marker; a wake being admitted
    /// or an ending attempt has none.
    #[must_use]
    pub fn waiting_on(&self, agent_id: &str) -> Option<&WaitingOn> {
        let parked = self.parked.get(agent_id)?;
        let running = self
            .nodes
            .get(agent_id)
            .is_some_and(|node| node.status == NodeStatus::Running);
        (running && (parked.state == ParkState::Parked || parked.continuation))
            .then_some(&parked.waiting_on)
    }

    /// Children a reopen finds parked, derived from durable rows alone:
    /// running node, open unsubmitted attempt with no bound or active turn, a
    /// session, and at least one turn of that attempt already spent (an
    /// unstarted spawn or a fresh followup, even one whose start was
    /// prepared, has none; an interrupted node is `interrupted`).
    /// Returns `(agent_id, session_locator, path)`.
    #[must_use]
    pub fn parked_reopen_targets(&self) -> Vec<(String, String, String)> {
        self.nodes
            .values()
            .filter(|node| node.agent_id != self.root_agent_id)
            .filter(|node| node.status == NodeStatus::Running)
            .filter_map(|node| {
                let attempt = self.current_attempt(&node.agent_id)?;
                let resume = self
                    .pending_transient_resume(&node.agent_id)
                    .ok()
                    .flatten()
                    .is_some();
                let parked = attempt.status == AttemptStatus::Running
                    && attempt.output_json.is_none()
                    && ((!node.active_turn && attempt.turn_id.is_none()) || resume)
                    && (attempt.usage.turns >= 1
                        || resume
                        || self.commands.values().any(|command| {
                            command.recipient == node.agent_id
                                && command.kind != super::AgentCommandKind::RootBatch
                                && command.attempt_id.as_ref() == Some(&attempt.attempt_id)
                                && command.fact.is_some()
                        }));
                parked.then(|| {
                    Some((
                        node.agent_id.clone(),
                        node.session_locator.clone()?,
                        node.path.display().to_string(),
                    ))
                })?
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
