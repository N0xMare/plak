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
    /// Held by a resume for the host's reopen wake, not parked by a turn
    /// waiter; only [`ProductAgentTree::release_reopen_hold`] removes it.
    Held,
    /// The host is admitting or rejoining the reopen wake of a held child:
    /// mail still waits, and an interrupt ends it like a hold.
    Reopening,
    /// The wake was not admitted and the host is ending the attempt: new mail
    /// is refused as terminal and an interrupt no longer applies.
    Ending,
}

impl ProductAgentTree {
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

    /// Whether a wake begun by [`Self::begin_wake`] may still start: nothing
    /// ended the park since.
    #[must_use]
    pub fn wake_admissible(&self, agent_id: &str) -> bool {
        self.parked
            .get(agent_id)
            .is_some_and(|parked| parked.state == ParkState::Waking)
            && self
                .nodes
                .get(agent_id)
                .is_some_and(|node| node.status == NodeStatus::Running)
    }

    /// Moves a held child to its reopen wake, before any Start or rejoin
    /// binds a turn for it. Returns whether the child is still held on a
    /// running node with an open, unsubmitted attempt: an interrupt or close
    /// that landed since the resume ended the hold, and nothing may start.
    pub fn begin_reopen_wake(&mut self, agent_id: &str) -> bool {
        let open = self.reopen_target_open(agent_id);
        match self.parked.get_mut(agent_id) {
            Some(parked) if open && parked.state == ParkState::Held => {
                parked.state = ParkState::Reopening;
                true
            }
            _ => false,
        }
    }

    /// Whether a reopen wake begun by [`Self::begin_reopen_wake`] may still
    /// start or bind its turn: nothing ended the hold since.
    #[must_use]
    pub fn reopen_wake_admissible(&self, agent_id: &str) -> bool {
        self.parked
            .get(agent_id)
            .is_some_and(|parked| parked.state == ParkState::Reopening)
            && self.reopen_target_open(agent_id)
    }

    fn reopen_target_open(&self, agent_id: &str) -> bool {
        self.nodes
            .get(agent_id)
            .is_some_and(|node| node.status == NodeStatus::Running)
            && self.current_attempt(agent_id).is_some_and(|attempt| {
                attempt.status == AttemptStatus::Running && attempt.output_json.is_none()
            })
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
        (running
            && (matches!(parked.state, ParkState::Parked | ParkState::Held) || parked.continuation))
            .then_some(&parked.waiting_on)
    }

    /// Holds every child a reopen finds parked until the host's reopen wake
    /// has run for it, so mail sent in between waits for that wake (a second
    /// turn is never started beside it) and an interrupt still ends the
    /// park. Called when the tree resumes.
    pub(super) fn hold_parked_reopen_targets(&mut self) {
        for (agent_id, ..) in self.parked_reopen_targets() {
            self.parked.entry(agent_id).or_insert(ParkedChild {
                waiting_on: WaitingOn::default(),
                state: ParkState::Held,
                continuation: false,
            });
        }
    }

    /// Ends a resume's hold once the reopen wake for `agent_id` ran or was
    /// abandoned. A park its turn waiter recorded since is left alone.
    pub fn release_reopen_hold(&mut self, agent_id: &str) {
        if self.parked.get(agent_id).is_some_and(|parked| {
            !parked.continuation && matches!(parked.state, ParkState::Held | ParkState::Reopening)
        }) {
            self.parked.remove(agent_id);
        }
    }

    /// Children a reopen finds parked, derived from durable rows alone:
    /// running node, open unsubmitted attempt with no bound or active turn, a
    /// session, and at least one turn already spent (an unstarted spawn or a
    /// fresh followup has none; an interrupted node is `interrupted`).
    /// Returns `(agent_id, session_locator, path)`.
    #[must_use]
    pub fn parked_reopen_targets(&self) -> Vec<(String, String, String)> {
        if self.state != super::TreeAdmissionState::Live {
            return Vec::new();
        }
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
                        || self.commands.values().any(|command| {
                            command.recipient == node.agent_id
                                && command.kind != super::AgentCommandKind::RootBatch
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
mod tests {
    use crate::protocol::{ListAgentsRequest, MessagePriority, MessagePurpose, TopologyErrorCode};
    use crate::tree::test_support::*;
    use crate::tree::*;

    /// A spawned child that already ran one turn, as the turn waiter leaves
    /// it when that turn ended without `submit_result`.
    fn after_one_turn(tree: &mut ProductAgentTree) -> String {
        let child = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&child, "locator".to_owned())
            .unwrap();
        tree.set_active_turn(&child, true).unwrap();
        tree.bind_attempt_turn(&child, "00000000-0000-0000-0000-000000000001")
            .unwrap();
        child
    }

    #[test]
    fn a_parked_child_runs_waits_on_its_work_and_settles_no_wait() {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        tree.release_attempt_turn(&child).unwrap();
        let waiting = tree.park_child(&child, vec!["p3".to_owned()]).unwrap();
        assert_eq!(waiting.processes, ["p3"]);
        let attempt = tree.attempt(&child).unwrap();
        assert_eq!(attempt.status, AttemptStatus::Running);
        assert!(attempt.turn_id.is_none());
        assert_eq!(attempt.submit_harness_nudge, SubmitNudgeBudget::NONE);
        assert!(!tree.node(&child).unwrap().active_turn);
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Running);

        // The parent's wait does not settle on a park, and its rows say why.
        assert!(tree.poll_wait("root", &wait_for(&child)).unwrap().is_none());
        let row = tree.wait_row(&child).unwrap();
        assert_eq!(row.waiting_on.as_ref().unwrap().processes, ["p3"]);
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: None,
                },
            )
            .unwrap();
        assert_eq!(
            listed.agents[0]["waiting_on"],
            serde_json::json!({"processes": ["p3"], "agents": []})
        );

        // A wake in flight hides the row; its bound turn ends the park.
        tree.begin_wake(&child);
        assert!(tree.wait_row(&child).unwrap().waiting_on.is_none());
        assert!(tree.is_parked(&child));
        tree.unpark_child(&child);
        assert!(!tree.is_parked(&child));
    }

    #[test]
    fn mail_to_a_parked_child_waits_for_its_wake_instead_of_starting_it() {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        tree.release_attempt_turn(&child).unwrap();
        tree.park_child(&child, Vec::new()).unwrap();
        let sent = tree
            .send_agent_message("root", &mail(&child, "also check docs"), None)
            .unwrap();
        assert_eq!(sent.disposition, MailDisposition::Queued);
        let mut urgent = mail(&child, "stop and report");
        urgent.priority = Some(MessagePriority::Urgent);
        let steered = tree.send_agent_message("root", &urgent, None).unwrap();
        assert_eq!(steered.disposition, MailDisposition::Steered);
        assert_eq!(tree.wake_mail(&child).len(), 2);
        // Unparked with no bound turn, the same mail would start a turn.
        tree.unpark_child(&child);
        let started = tree
            .send_agent_message("root", &mail(&child, "start"), None)
            .unwrap();
        assert_eq!(started.disposition, MailDisposition::Started);
    }

    #[test]
    fn interrupt_and_close_end_a_park() {
        let (_root, mut tree) = tree();
        let interrupted = after_one_turn(&mut tree);
        tree.release_attempt_turn(&interrupted).unwrap();
        tree.park_child(&interrupted, Vec::new()).unwrap();
        let status = tree
            .interrupt_agent("root", &close_req(&interrupted), None)
            .unwrap();
        assert_eq!(status, NodeStatus::Interrupted);
        assert!(!tree.is_parked(&interrupted));

        let by_root = after_one_turn(&mut tree);
        tree.release_attempt_turn(&by_root).unwrap();
        tree.park_child(&by_root, Vec::new()).unwrap();
        assert_eq!(
            tree.interrupt_running_descendants().unwrap(),
            std::slice::from_ref(&by_root)
        );
        assert_eq!(tree.node(&by_root).unwrap().status, NodeStatus::Interrupted);
        assert!(!tree.is_parked(&by_root));

        let closed = after_one_turn(&mut tree);
        tree.release_attempt_turn(&closed).unwrap();
        tree.park_child(&closed, Vec::new()).unwrap();
        tree.close_agent("root", &close_req(&closed), None).unwrap();
        assert!(!tree.is_parked(&closed));
        assert!(tree.waiting_on(&closed).is_none());
    }

    /// An interrupt that wins the race against a wake's admission ends the
    /// park: the wake may not begin, or may no longer start once begun.
    #[test]
    fn an_interrupt_refuses_a_wake_begun_or_not() {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        tree.release_attempt_turn(&child).unwrap();
        tree.park_child(&child, Vec::new()).unwrap();
        assert!(tree.begin_wake(&child));
        assert!(tree.wake_admissible(&child));
        assert!(!tree.begin_wake(&child), "one wake at a time");
        tree.interrupt_agent("root", &close_req(&child), None)
            .unwrap();
        assert!(!tree.wake_admissible(&child));
        assert!(!tree.is_parked(&child));

        let parked = after_one_turn(&mut tree);
        tree.release_attempt_turn(&parked).unwrap();
        tree.park_child(&parked, Vec::new()).unwrap();
        tree.interrupt_agent("root", &close_req(&parked), None)
            .unwrap();
        assert!(!tree.begin_wake(&parked));
    }

    /// A child whose wake was not admitted is ending its attempt: mail is
    /// refused as terminal instead of starting a turn, an interrupt no
    /// longer applies, and rows carry no parked marker.
    #[test]
    fn an_ending_park_refuses_mail_and_ignores_an_interrupt() {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        tree.release_attempt_turn(&child).unwrap();
        tree.park_child(&child, Vec::new()).unwrap();
        tree.begin_wake(&child);
        assert!(tree.mark_park_ending(&child));
        assert!(tree.waiting_on(&child).is_none());
        let refused = tree
            .send_agent_message("root", &mail(&child, "status?"), None)
            .unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::RecipientAttemptTerminal);
        assert_eq!(
            tree.interrupt_agent("root", &close_req(&child), None)
                .unwrap(),
            NodeStatus::Running
        );
        assert!(tree.interrupt_running_descendants().unwrap().is_empty());
        assert!(tree.is_park_ending(&child));
        tree.unpark_child(&child);
        assert!(!tree.mark_park_ending(&child), "only a parked child ends");
    }

    #[test]
    fn claimed_mail_is_not_a_wake_arrival_until_its_delivery_returns() {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        let sent = tree
            .send_agent_message("root", &mail(&child, "also check docs"), None)
            .unwrap();
        tree.set_delivery_claim_for_test(&sent.message_id, true);
        assert_eq!(
            tree.claimed_mail(&child),
            std::slice::from_ref(&sent.message_id)
        );
        assert!(tree.wake_mail(&child).is_empty());
        tree.set_delivery_claim_for_test(&sent.message_id, false);
        assert!(tree.claimed_mail(&child).is_empty());
        assert_eq!(tree.wake_mail(&child).len(), 1);
    }

    #[test]
    fn live_child_work_counts_children_that_will_still_report() {
        let (_root, mut tree) = tree();
        let mut snapshot = tree.snapshot.clone();
        snapshot.max_depth = 2;
        snapshot.digest = snapshot.compute_digest();
        tree.install_snapshot(snapshot);
        let hub = tree
            .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
            .unwrap()
            .agent_id;
        let running = tree.spawn_agent(&hub, &spawn_req(), None).unwrap().agent_id;
        let interrupted = tree.spawn_agent(&hub, &spawn_req(), None).unwrap().agent_id;
        tree.set_active_turn(&interrupted, true).unwrap();
        tree.interrupt_agent(&hub, &close_req(&interrupted), None)
            .unwrap();
        assert_eq!(tree.live_child_work(&hub), std::slice::from_ref(&running));
        tree.submit_result(
            &running,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.to_owned(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&running).unwrap();
        assert!(tree.live_child_work(&hub).is_empty());
    }

    #[test]
    fn a_reopen_derives_the_parked_shape_from_durable_rows() {
        let (_root, mut tree) = tree();
        let parked = after_one_turn(&mut tree);
        tree.release_attempt_turn(&parked).unwrap();
        tree.park_child(&parked, Vec::new()).unwrap();
        // Spawned and bound but never started: no turn spent, not parked.
        let unstarted = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&unstarted, "unstarted".to_owned())
            .unwrap();
        // Mid-turn: a bound turn is an in-flight join target instead.
        let running = after_one_turn(&mut tree);
        let interrupted = after_one_turn(&mut tree);
        tree.interrupt_agent("root", &close_req(&interrupted), None)
            .unwrap();
        tree.release_attempt_turn(&interrupted).unwrap();
        // An admitted followup is a fresh attempt with no turn spent: its own
        // Start runs it, never a reopen wake beside it.
        let retasked = after_one_turn(&mut tree);
        tree.interrupt_agent("root", &close_req(&retasked), None)
            .unwrap();
        tree.release_attempt_turn(&retasked).unwrap();
        let mut followup = mail(&retasked, "now the docs");
        followup.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &followup, None).unwrap();
        assert_eq!(tree.node(&retasked).unwrap().status, NodeStatus::Running);

        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert!(
            !reopened.is_parked(&parked),
            "the parked map is process-local"
        );
        // A restored tree wakes nothing until the operator resumes it.
        assert!(reopened.parked_reopen_targets().is_empty());
        reopened.resume_tree().unwrap();
        let targets = reopened
            .parked_reopen_targets()
            .into_iter()
            .map(|(agent_id, locator, _)| (agent_id, locator))
            .collect::<Vec<_>>();
        assert_eq!(targets, [(parked, "locator".to_owned())]);
        assert!(
            reopened
                .inflight_join_targets()
                .iter()
                .any(|(agent_id, ..)| agent_id == &running)
        );
    }

    /// Between `/tree resume` and the host's reopen wake, mail to a derived
    /// parked child waits for that wake instead of starting a second turn,
    /// and an interrupt still ends the park.
    #[test]
    fn a_resume_holds_derived_parked_children_for_the_reopen_wake() {
        let (_root, mut tree) = tree();
        let held = after_one_turn(&mut tree);
        tree.release_attempt_turn(&held).unwrap();
        tree.park_child(&held, Vec::new()).unwrap();
        let interrupted = after_one_turn(&mut tree);
        tree.release_attempt_turn(&interrupted).unwrap();
        tree.park_child(&interrupted, Vec::new()).unwrap();
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert!(!reopened.is_parked(&held));
        reopened.resume_tree().unwrap();
        assert!(reopened.is_parked(&held));
        assert_eq!(
            reopened.waiting_on(&held),
            Some(&WaitingOn::default()),
            "a held child reads as parked before its wake names what it waits on"
        );
        let sent = reopened
            .send_agent_message("root", &mail(&held, "status?"), None)
            .unwrap();
        assert_eq!(sent.disposition, MailDisposition::Queued);
        assert_eq!(reopened.wake_mail(&held).len(), 1);
        assert_eq!(
            reopened
                .interrupt_agent("root", &close_req(&interrupted), None)
                .unwrap(),
            NodeStatus::Interrupted
        );

        // The wake ran and its turn waiter parked the child again: releasing
        // the hold leaves that park alone.
        reopened
            .bind_attempt_turn(&held, "00000000-0000-0000-0000-000000000002")
            .unwrap();
        reopened.set_active_turn(&held, true).unwrap();
        reopened.release_attempt_turn(&held).unwrap();
        reopened.park_child(&held, vec!["p1".to_owned()]).unwrap();
        reopened.release_reopen_hold(&held);
        assert_eq!(reopened.waiting_on(&held).unwrap().processes, ["p1"]);
        reopened.unpark_child(&held);

        // A wake that never ran releases the hold outright.
        let mut again = ProductAgentTree::reopen(reopened.to_persisted());
        again.resume_tree().unwrap();
        assert!(again.is_parked(&held));
        again.release_reopen_hold(&held);
        assert!(!again.is_parked(&held));
    }

    /// The reopen wake of a held child starts only from the hold: an
    /// interrupt before it began, or while it is being admitted, ends it,
    /// and a released hold is not woken.
    #[test]
    fn a_reopen_wake_begins_only_from_a_hold_an_interrupt_has_not_ended() {
        let (_root, mut tree) = tree();
        let woken = after_one_turn(&mut tree);
        tree.release_attempt_turn(&woken).unwrap();
        tree.park_child(&woken, Vec::new()).unwrap();
        let interrupted_early = after_one_turn(&mut tree);
        tree.release_attempt_turn(&interrupted_early).unwrap();
        tree.park_child(&interrupted_early, Vec::new()).unwrap();
        let interrupted_late = after_one_turn(&mut tree);
        tree.release_attempt_turn(&interrupted_late).unwrap();
        tree.park_child(&interrupted_late, Vec::new()).unwrap();
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert!(!reopened.begin_reopen_wake(&woken), "nothing is held yet");
        reopened.resume_tree().unwrap();

        assert!(reopened.begin_reopen_wake(&woken));
        assert!(reopened.reopen_wake_admissible(&woken));
        assert!(
            !reopened.begin_reopen_wake(&woken),
            "one reopen wake at a time"
        );
        assert!(reopened.waiting_on(&woken).is_none());
        let sent = reopened
            .send_agent_message("root", &mail(&woken, "status?"), None)
            .unwrap();
        assert_eq!(sent.disposition, MailDisposition::Queued);
        reopened.release_reopen_hold(&woken);
        assert!(!reopened.is_parked(&woken));

        reopened
            .interrupt_agent("root", &close_req(&interrupted_early), None)
            .unwrap();
        assert!(!reopened.begin_reopen_wake(&interrupted_early));

        assert!(reopened.begin_reopen_wake(&interrupted_late));
        assert_eq!(
            reopened
                .interrupt_agent("root", &close_req(&interrupted_late), None)
                .unwrap(),
            NodeStatus::Interrupted
        );
        assert!(!reopened.reopen_wake_admissible(&interrupted_late));
        assert!(!reopened.is_parked(&interrupted_late));
    }

    #[test]
    fn await_receipts_stay_bounded_across_many_wakes() {
        let (_root, mut tree) = tree();
        let child = after_one_turn(&mut tree);
        for wake in 0..20_u32 {
            let operation = format!("wait-{wake}");
            tree.remember_success(
                &operation,
                &child,
                AWAIT_RECEIPT_KIND,
                &serde_json::json!({"wake": wake}),
                &serde_json::json!({}),
            )
            .unwrap();
            assert_eq!(tree.retained_wait_receipts(&child), [operation]);
            tree.release_attempt_turn(&child).unwrap();
            tree.park_child(&child, Vec::new()).unwrap();
            assert!(tree.retained_wait_receipts(&child).is_empty());
            tree.begin_wake(&child);
            tree.bind_attempt_turn(&child, format!("00000000-0000-0000-0000-{:012}", wake + 2))
                .unwrap();
            tree.set_active_turn(&child, true).unwrap();
            tree.unpark_child(&child);
        }
        assert_eq!(tree.attempt(&child).unwrap().usage.turns, 21);
    }
}
