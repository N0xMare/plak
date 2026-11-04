//! Mail: the delivery request and `send_agent_message` reaching, following up or steering a turn.

use super::*;

#[derive(Debug)]
pub(super) enum MailConsumer {
    Wait {
        caller: String,
        operation: String,
    },
    Delivery(String),
    /// The Start `message` reserved for `agent`: a send's only exclusive
    /// right to deliver a Start to that child.
    Start {
        agent: String,
        message: String,
    },
}

/// A process-local mail claim, released as one writer: by
/// [`Self::release`] where the claimant returns, or by a writer the drop
/// spawns when the claimant's future is dropped first.
pub(super) struct MailConsumerGuard {
    driver: TreeDriver,
    consumer: Option<MailConsumer>,
}

impl MailConsumerGuard {
    pub(super) fn new(driver: TreeDriver, consumer: MailConsumer) -> Self {
        Self {
            driver,
            consumer: Some(consumer),
        }
    }

    /// Releases the claim before the claimant goes on. The release is owed:
    /// it waits for its turn however long a storage outage holds the tree.
    pub(super) async fn release(mut self) {
        if let Some(consumer) = self.consumer.take() {
            let _ = self
                .driver
                .write_owed(move |tree| Self::release_in(tree, &consumer))
                .await;
        }
    }

    fn release_in(tree: &mut ProductAgentTree, consumer: &MailConsumer) {
        match consumer {
            MailConsumer::Wait { caller, operation } => {
                tree.parked_waits
                    .remove(&(caller.clone(), operation.clone()));
            }
            MailConsumer::Delivery(id) => {
                tree.delivery_claims.remove(id);
            }
            MailConsumer::Start { agent, message } => {
                if tree.start_reservations.get(agent) == Some(message) {
                    tree.start_reservations.remove(agent);
                }
            }
        }
        tree.waiters().notify_waiters();
    }
}

impl Drop for MailConsumerGuard {
    fn drop(&mut self) {
        let Some(consumer) = self.consumer.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            // Nothing can release it here; the claim stays until the tree
            // is reopened.
            tracing::error!(target: "plak.topology", consumer = ?consumer,
                "a dropped mail claim could not be released: no runtime");
            return;
        }
        let driver = self.driver.clone();
        let release = async move {
            let _ = driver
                .write_owed(move |tree| Self::release_in(tree, &consumer))
                .await;
        };
        // Joins the tree's release owners: no root's release aborts it.
        drop(self.driver.spawn_release_owner(release));
    }
}

/// Delivers one persisted mail record onto the bound child session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MailDeliveryRequest {
    /// The recipient node.
    pub agent_id: String,
    /// Its bound session.
    pub session_locator: String,
    /// What the tree already decided this message does to the turn.
    pub disposition: MailDisposition,
    /// The persisted message being delivered.
    pub message_id: String,
    /// Its text.
    pub body: String,
    /// The node's job line, for a delivery that starts a fresh turn.
    pub description: String,
    /// The node's brief, for the same reason.
    pub prompt: String,
    /// The schema a turn started by this delivery must submit against.
    pub output_schema_json: String,
    /// The checkout the recipient works in.
    pub path: String,
    /// The token the recipient's turn must submit under.
    pub turn_token: u64,
}

/// Runtime acknowledgement after Start / FollowUp / Steer.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct MailDeliveryAck {
    /// The turn the delivery started or joined, when it named one. A Start's
    /// turn the deliverer already bound; a joined turn was already bound. The
    /// send never binds it.
    pub turn_id: Option<String>,
}

/// Why an accepted mail record could not be delivered to its child session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MailDeliveryFailure {
    /// Delivery was definitely refused before a turn started; existing child
    /// authority remains reusable after the tree rolls back the new attempt.
    Refused(plakous_host_protocol::status::HostFailure),
    /// Delivery may have changed runtime authority and requires recovery.
    RecoveryRequired(plakous_host_protocol::status::HostFailure),
    /// The mail reached the child after the turn it targeted ended, or (a
    /// Start) found the child's slot held by a turn another command admitted.
    /// Nothing was delivered: the send looks at the child again (spec 03
    /// §4.3, §9).
    TurnEnded,
    /// The child's live slot is still starting or retiring: there is no turn
    /// to join yet, and none ended. Nothing was delivered; the send looks
    /// again once the slot changes.
    SlotBusy,
}

impl MailDeliveryFailure {
    fn into_parts(self) -> (bool, plakous_host_protocol::status::HostFailure) {
        match self {
            Self::Refused(message) => (true, message),
            Self::RecoveryRequired(message) => (false, message),
            Self::TurnEnded => (true, "the child's turn ended before delivery".into()),
            Self::SlotBusy => (
                true,
                "the child's live slot was still starting or retiring".into(),
            ),
        }
    }
}

impl From<String> for MailDeliveryFailure {
    fn from(message: String) -> Self {
        Self::RecoveryRequired(message.into())
    }
}

impl From<&str> for MailDeliveryFailure {
    fn from(message: &str) -> Self {
        Self::RecoveryRequired(message.into())
    }
}

pub(super) fn settle_followup_refusal(
    tree: &mut ProductAgentTree,
    agent: &str,
    message: &str,
    receipt: Option<(String, TopologyErrorCode)>,
) -> Result<(), TopologyError> {
    tree.record_undeliverable_mail(message)?;
    tree.project_mail_decisions(agent)?;
    if let Some((id, code)) = receipt {
        let operation = tree.operations().get(&id).cloned().ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "followup send receipt is missing",
            )
        })?;
        let request: Value = serde_json::from_str(&operation.canonical_request).map_err(|_| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "followup send receipt has invalid immutable request data",
            )
        })?;
        tree.remember_failure(
            &id,
            &operation.caller_agent_id,
            &operation.tool_name,
            &request,
            code,
        )?;
    }
    Ok(())
}

/// What a delivery that reached no turn does with its mail.
enum TurnEndedMail {
    /// The mail stays unread for the running child's park or wake (or it was
    /// already consumed): the send stands.
    Left,
    /// No turn will ever carry the mail: the send is refused.
    Refused(String),
    /// The child has no turn: deliver the mail again as the Start this send
    /// has reserved (an interrupted child's resume, or a Start that found
    /// the slot still starting or retiring).
    Restart,
    /// A turn another command admitted runs: deliver the mail into it as a
    /// follow-up.
    FollowUp,
    /// The sender was cancelled before any redelivery: the mail stays unread
    /// under its original receipt, unless it opened a `followup` attempt
    /// that never started, which is rolled back with the mail settled read
    /// (`TopologyTool::cancel_followup_send`).
    Cancelled(ToolRuntimeError),
    /// An interrupted child's cancelled turn is still bound and no turn
    /// waiter is left to release it: the send fails closed, as a delivery
    /// that found it so does (spec 03 section 9).
    Unwatched,
}

/// The sender's cancellation, polled only where a send waits.
type SendCancelled<'c> = Pin<Box<dyn Future<Output = ()> + Send + 'c>>;

/// What a delivery that came back without reaching a turn found.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Unreached {
    /// A follow-up or steer reached the live turn it targeted after that
    /// turn ended: its waiter parks or wakes a running child with the mail.
    EndedTurn,
    /// A Start found the slot held or vacant of its own turn, or the slot
    /// was still starting or retiring, or the send waited for an unwind or
    /// another send's Start: no turn the mail targeted has ended.
    NoTurn,
}

/// What one look at a recipient the delivery did not reach knows about its
/// send.
#[derive(Clone, Copy)]
struct TurnEndedLook {
    /// The send's budget is spent.
    pub(super) expired: bool,
    /// The sender was cancelled.
    pub(super) cancelled: bool,
    /// What the delivery found.
    unreached: Unreached,
}

/// Cancellation is reported after reconciling any followup opening.
fn send_cancelled() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::Cancelled,
        "send_agent_message was cancelled before delivery was confirmed",
    )
}

/// One send's budget for reaching a turn: an interrupted recipient's
/// unwind, a slot still starting or retiring, and every redelivery.
pub(super) const TURN_UNWIND_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Safety tick behind the tree notifications while it waits.
const TURN_UNWIND_TICK: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
struct MailDeliverySnapshot {
    root_id: String,
    pub(super) agent_id: String,
    pub(super) session_locator: Option<String>,
    pub(super) disposition: MailDisposition,
    pub(super) message_id: String,
    pub(super) body: String,
    pub(super) description: String,
    pub(super) prompt: String,
    pub(super) output_schema_json: String,
    pub(super) path: String,
    pub(super) durable: MailDurableState,
    pub(super) turn_token: u64,
}

/// What one look at a sent message's delivery found.
enum DeliveryClaim {
    /// Nothing is delivered live: the send stands as recorded.
    Delivered,
    /// This send claimed the delivery.
    Claimed(Box<MailDeliverySnapshot>),
    /// A parked wait or another send's Start holds the recipient: look again
    /// once the tree changes.
    Wait(Pin<Box<tokio::sync::futures::OwnedNotified>>),
}

impl TopologyTool {
    /// Claims the live delivery of `output`'s message, or says why not.
    fn claim_delivery(
        tree: &mut ProductAgentTree,
        output: &SendMessageOutput,
    ) -> Result<DeliveryClaim, ToolRuntimeError> {
        let Some(mail) = tree.mail(&output.message_id) else {
            // Only acknowledged history is pruned; its durable send
            // receipt remains replayable without delivering it again.
            return Ok(DeliveryClaim::Delivered);
        };
        // Root and parked-child mail waits for the host's next
        // admission of that node; it is never delivered live.
        if mail.to == tree.root_agent_id
            || mail.durable != MailDurableState::Accepted
            || tree.delivery_claims.contains(&mail.message_id)
            || tree.is_parked(&mail.to)
        {
            return Ok(DeliveryClaim::Delivered);
        }
        if tree
            .parked_waits
            .iter()
            .any(|((recipient, _), targets)| recipient == &mail.to && targets.contains(&mail.from))
            || (output.disposition == MailDisposition::Started
                && tree
                    .start_reservations
                    .get(&mail.to)
                    .is_some_and(|message| message != &mail.message_id))
        {
            // A cancelled or timed-out wait may not consume this mail,
            // and another send's Start to the same child is in
            // flight. Subscribe under the ownership lock and recheck
            // on release.
            let mut notified = Box::pin(tree.waiters().notified_owned());
            notified.as_mut().enable();
            return Ok(DeliveryClaim::Wait(notified));
        }
        let node = tree.node(&mail.to).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        let snapshot = MailDeliverySnapshot {
            root_id: tree.root_agent_id.clone(),
            agent_id: node.agent_id.clone(),
            session_locator: node.session_locator.clone(),
            disposition: output.disposition,
            message_id: output.message_id.clone(),
            body: mail.body.clone(),
            description: node.description.clone(),
            prompt: node.prompt.clone(),
            output_schema_json: node.output_schema_json.clone(),
            path: node.path.display().to_string(),
            durable: mail.durable,
            turn_token: tree
                .attempt(&node.agent_id)
                .map(|attempt| attempt.turn_token)
                .unwrap_or(1),
        };
        tree.delivery_claims.insert(output.message_id.clone());
        if output.disposition == MailDisposition::Started {
            tree.start_reservations
                .insert(snapshot.agent_id.clone(), output.message_id.clone());
        }
        Ok(DeliveryClaim::Claimed(Box::new(snapshot)))
    }

    pub(super) async fn send_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.send_cancellable(operation_id, value, Box::pin(std::future::pending()))
            .await
    }

    /// Sends one message and delivers it live when the recipient can take it.
    /// `cancelled` is the sender's cancellation. It is observed only where
    /// the send waits (behind a parked wait on the mail, or for an
    /// interrupted recipient's turn to unwind), never mid-delivery, and a
    /// cancelled send leaves its mail unread under its original receipt.
    #[allow(
        clippy::too_many_lines,
        reason = "mail admission and delivery retain the same attempt and independently owned operation receipt"
    )]
    pub(super) async fn send_cancellable(
        &self,
        operation_id: String,
        value: &Value,
        mut cancelled: SendCancelled<'_>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        if let Ok(request) = decode::<SendAgentMessageRequest>(value)
            && request.purpose == Some(MessagePurpose::Followup)
        {
            let driver = self.driver.clone();
            let (writer_caller, writer_request, writer_operation) =
                (caller.clone(), request.clone(), operation_id.clone());
            let (prepared, effects) = self
                .driver
                .mutate(move |tree| {
                    let result =
                        tree.rehydrate_followup(&writer_caller, &writer_request, &writer_operation);
                    driver.save(tree).and(result)
                })
                .await
                .map_err(|e| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, e.message))?;
            if let Err(error) = prepared {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
            let settled = self.settle(effects).await?;
            if let Some(error) = settled.cleanup_failures.first() {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
            if let Some(refusal) = self
                .refresh_followup(&caller, &request, &operation_id)
                .await?
            {
                return Ok(refusal);
            }
        }
        let tool = self.clone();
        let (writer_caller, writer_operation, writer_value) =
            (caller.clone(), operation_id.clone(), value.clone());
        let (output, effects) = self
            .driver
            .mutate(move |tree| {
                decode::<SendAgentMessageRequest>(&writer_value).and_then(|request| {
                    tool.dispatch_mutating(
                        tree,
                        &writer_caller,
                        &writer_operation,
                        "send_agent_message",
                        &request,
                        |tree, caller, request, operation_id| {
                            tree.send_agent_message(caller, request, operation_id)
                        },
                    )
                    .map(|(output, _replayed)| output)
                })
            })
            .await
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        self.settle(effects).await?;
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        let Some(deliverer) = self.deliverer() else {
            return ToolOutcome::success_text(json!(output).to_string());
        };
        let snapshot = loop {
            let claimed = {
                let output = output.clone();
                self.driver
                    .write_or_undo(
                        move |tree| Self::claim_delivery(tree, &output),
                        // A send dropped before it holds its claim releases it.
                        |tree, claimed| {
                            if let Ok(DeliveryClaim::Claimed(snapshot)) = claimed {
                                tree.delivery_claims.remove(&snapshot.message_id);
                                if tree.start_reservations.get(&snapshot.agent_id)
                                    == Some(&snapshot.message_id)
                                {
                                    tree.start_reservations.remove(&snapshot.agent_id);
                                }
                                tree.waiters().notify_waiters();
                            }
                        },
                    )
                    .await
                    .map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })??
            };
            let notify = match claimed {
                DeliveryClaim::Delivered => {
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                DeliveryClaim::Claimed(snapshot) => break *snapshot,
                DeliveryClaim::Wait(notify) => notify,
            };
            if let Either::Left(((), _)) = select(cancelled.as_mut(), notify).await {
                let agent = decode::<SendAgentMessageRequest>(value)
                    .map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?
                    .agent_id;
                self.cancel_followup_send(&operation_id, &agent, &output.message_id)
                    .await?;
                return Err(send_cancelled());
            }
        };
        let delivery = MailConsumerGuard::new(
            self.driver.clone(),
            MailConsumer::Delivery(output.message_id.clone()),
        );
        // A send's Starts to one child are exclusive: the reservation lasts
        // until the send returns.
        let mut start = (snapshot.disposition == MailDisposition::Started).then(|| {
            MailConsumerGuard::new(
                self.driver.clone(),
                MailConsumer::Start {
                    agent: snapshot.agent_id.clone(),
                    message: snapshot.message_id.clone(),
                },
            )
        });
        let outcome = self
            .deliver_claimed(
                ClaimedDelivery {
                    operation_id,
                    snapshot,
                    output,
                },
                value,
                &caller,
                Arc::clone(deliverer),
                &mut start,
                &mut cancelled,
            )
            .await;
        if let Some(start) = start {
            start.release().await;
        }
        delivery.release().await;
        outcome
    }

    /// Publishes an admitted delivery: a redelivery's receipt is rewritten
    /// first, from `recorded` (operation, caller, arguments).
    async fn record_delivered(
        &self,
        recorded: Option<(&str, &str, &Value)>,
        output: SendMessageOutput,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let driver = self.driver.clone();
        let redelivery = recorded.map(|(operation_id, caller, value)| {
            (
                operation_id.to_owned(),
                caller.to_owned(),
                value.clone(),
                output.clone(),
            )
        });
        let saved = self
            .driver
            .write(move |tree| {
                if let Some((operation_id, caller, value, recorded)) = &redelivery {
                    Self::record_redelivery(tree, operation_id, caller, value, recorded)?;
                }
                // A Start's deliverer bound the turn it admitted before
                // that turn could end, and a joined turn was already
                // bound; the send binds nothing. A turn that has ended,
                // or that a later turn replaced, is never bound or marked
                // active again here.
                driver.save_guarded(tree)
            })
            .await
            .map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
        if let Err(error) = saved {
            return ToolOutcome::failure_text(
                json!({ "code": error.code, "message": error.message }).to_string(),
            );
        }
        ToolOutcome::success_text(json!(output).to_string())
    }

    async fn deliver_claimed(
        &self,
        claimed: ClaimedDelivery,
        value: &Value,
        caller: &str,
        deliverer: Arc<dyn ChildController>,
        start: &mut Option<MailConsumerGuard>,
        cancelled: &mut SendCancelled<'_>,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let ClaimedDelivery {
            operation_id,
            snapshot,
            output,
        } = claimed;
        if snapshot.agent_id == snapshot.root_id {
            return ToolOutcome::success_text(json!(output).to_string());
        }
        if snapshot.durable != MailDurableState::Accepted {
            return ToolOutcome::success_text(json!(output).to_string());
        }
        let Some(session_locator) = snapshot.session_locator.clone() else {
            return self
                .fail_closed_session(
                    &snapshot.agent_id,
                    "child session locator is missing; spawn bind did not complete",
                )
                .await;
        };
        let mut state = SendState {
            operation_id,
            snapshot,
            output,
            // One budget for the whole send, however often it redelivers.
            deadline: tokio::time::Instant::now() + TURN_UNWIND_WAIT,
            redelivered: false,
        };
        loop {
            if cancelled.as_mut().now_or_never().is_some() {
                self.cancel_followup_send(
                    &state.operation_id,
                    &state.snapshot.agent_id,
                    &state.snapshot.message_id,
                )
                .await?;
                return Err(send_cancelled());
            }
            // The budget bounds waiting for a turn, never a submission in
            // flight: cutting one would fence an attempt whose turn may be
            // running. The delivery is bounded by its own reopen and admission.
            let delivery = Box::pin(deliverer.deliver(MailDeliveryRequest {
                agent_id: state.snapshot.agent_id.clone(),
                session_locator: session_locator.clone(),
                disposition: state.snapshot.disposition,
                message_id: state.snapshot.message_id.clone(),
                body: state.snapshot.body.clone(),
                description: state.snapshot.description.clone(),
                prompt: state.snapshot.prompt.clone(),
                output_schema_json: state.snapshot.output_schema_json.clone(),
                path: state.snapshot.path.clone(),
                turn_token: state.snapshot.turn_token,
            }));
            let answer = match select(cancelled.as_mut(), delivery).await {
                Either::Left(_) => {
                    self.finish_followup_start(
                        &state.snapshot.agent_id,
                        &state.snapshot.message_id,
                        true,
                        None,
                    )
                    .await?;
                    return Err(send_cancelled());
                }
                Either::Right((answer, _)) => answer,
            };
            match answer {
                Ok(_) => {
                    // Only an admitted redelivery rewrites the receipt. Until
                    // that admission `output` stays the receipt already
                    // recorded, so a later `Left` does not answer the
                    // in-memory follow-up.
                    if state.redelivered {
                        state.output.disposition = state.snapshot.disposition;
                    }
                    let recorded =
                        state
                            .redelivered
                            .then_some((state.operation_id.as_str(), caller, value));
                    return self.record_delivered(recorded, state.output).await;
                }
                // The claim is released on return. Unread mail wakes a running
                // child whose turn ended; a child with no turn is started by
                // this same mail once its slot is vacant; a turn another
                // command admitted gets the mail as a follow-up; an attempt
                // that already submitted or ended is never woken, so the send
                // is refused and the mail settled read.
                Err(failure @ (MailDeliveryFailure::TurnEnded | MailDeliveryFailure::SlotBusy)) => {
                    if let Some(outcome) = self
                        .turn_unreached(&mut state, failure, value, caller, start, cancelled)
                        .await?
                    {
                        return Ok(outcome);
                    }
                }
                Err(failure) => {
                    // After a redelivery the receipt becomes this refusal, so
                    // it never records a delivery no turn admitted and a
                    // replay is refused too.
                    if state.redelivered {
                        self.remember_redelivery_failure(&state.operation_id, caller, value)
                            .await?;
                    }
                    return self
                        .refuse_delivery(
                            Some(&state.operation_id),
                            &state.snapshot.agent_id,
                            &state.snapshot.message_id,
                            failure,
                        )
                        .await;
                }
            }
        }
    }

    /// Settles a send whose recipient's attempt already submitted or ended:
    /// the follow-up is withdrawn and the mail is read, refused.
    async fn refuse_ended_send(
        &self,
        state: &SendState,
        value: &Value,
        caller: &str,
        reason: &str,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        self.finish_followup_start(
            &state.snapshot.agent_id,
            &state.snapshot.message_id,
            false,
            Some((
                state.operation_id.clone(),
                TopologyErrorCode::RecipientAttemptTerminal,
            )),
        )
        .await?;
        let tool = self.clone();
        let (operation_id, caller, value, snapshot) = (
            state.operation_id.clone(),
            caller.to_owned(),
            value.clone(),
            state.snapshot.clone(),
        );
        let reason = reason.to_owned();
        self.driver
            .write(move |tree| {
                tool.refuse_turn_ended(tree, &operation_id, &caller, &value, &snapshot, &reason)
            })
            .await
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?
    }

    /// The mail reached no turn: wait for one, start the child, follow up
    /// into the turn another command admitted, or settle the send. `None`
    /// redelivers.
    async fn turn_unreached(
        &self,
        state: &mut SendState,
        failure: MailDeliveryFailure,
        value: &Value,
        caller: &str,
        start: &mut Option<MailConsumerGuard>,
        cancelled: &mut SendCancelled<'_>,
    ) -> Result<Option<ToolOutcome>, ToolRuntimeError> {
        let busy = failure == MailDeliveryFailure::SlotBusy;
        let unreached = if busy || state.snapshot.disposition == MailDisposition::Started {
            Unreached::NoTurn
        } else {
            Unreached::EndedTurn
        };
        match self
            .after_turn_ended(
                &state.snapshot,
                unreached,
                busy || (state.redelivered && unreached == Unreached::NoTurn),
                state.deadline,
                cancelled,
            )
            .await?
        {
            TurnEndedMail::Left => {
                // `output` is still the recorded receipt. The
                // follow-up disposition lives only on `snapshot`
                // until a turn admits it.
                return ToolOutcome::success_text(json!(state.output).to_string()).map(Some);
            }
            TurnEndedMail::Refused(reason) => {
                return self
                    .refuse_ended_send(state, value, caller, &reason)
                    .await
                    .map(Some);
            }
            TurnEndedMail::Cancelled(error) => {
                self.cancel_followup_send(
                    &state.operation_id,
                    &state.snapshot.agent_id,
                    &state.snapshot.message_id,
                )
                .await?;
                return Err(error);
            }
            TurnEndedMail::Unwatched => {
                if state.redelivered {
                    self.remember_redelivery_failure(&state.operation_id, caller, value)
                        .await?;
                }
                return self
                    .refuse_delivery(
                        Some(&state.operation_id),
                        &state.snapshot.agent_id,
                        &state.snapshot.message_id,
                        MailDeliveryFailure::RecoveryRequired(
                            "the child's interrupted turn is still bound and no turn \
                         waiter is left to release it"
                                .into(),
                        ),
                    )
                    .await
                    .map(Some);
            }
            TurnEndedMail::Restart => {
                let _ = start.get_or_insert_with(|| {
                    MailConsumerGuard::new(
                        self.driver.clone(),
                        MailConsumer::Start {
                            agent: state.snapshot.agent_id.clone(),
                            message: state.snapshot.message_id.clone(),
                        },
                    )
                });
                state.snapshot.disposition = MailDisposition::Started;
                state.redelivered = true;
            }
            TurnEndedMail::FollowUp => {
                state.snapshot.disposition = MailDisposition::Queued;
                state.redelivered = true;
            }
        }
        Ok(None)
    }

    /// Rewrites an admitted redelivery's mail and operation receipt to the
    /// delivery that carried it: `started` for a Start, `queued` for a
    /// follow-up into a turn another command admitted (spec 03 sections 4.3
    /// and 9).
    fn record_redelivery(
        tree: &mut ProductAgentTree,
        operation_id: &str,
        caller: &str,
        value: &Value,
        output: &SendMessageOutput,
    ) -> Result<(), TopologyError> {
        let request = decode::<SendAgentMessageRequest>(value)?;
        tree.redeliver_mail(&output.message_id, output.disposition);
        if tree.operations().contains_key(operation_id) {
            tree.remember_success(operation_id, caller, "send_agent_message", &request, output)?;
        }
        Ok(())
    }

    /// A redelivery that was not admitted: the operation receipt becomes
    /// the refusal [`Self::refuse_delivery`] reports.
    async fn remember_redelivery_failure(
        &self,
        operation_id: &str,
        caller: &str,
        value: &Value,
    ) -> Result<(), ToolRuntimeError> {
        let request = decode::<SendAgentMessageRequest>(value).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        let (operation_id, caller) = (operation_id.to_owned(), caller.to_owned());
        self.driver
            .write(move |tree| {
                if tree.operations().contains_key(&operation_id) {
                    tree.remember_failure(
                        &operation_id,
                        &caller,
                        "send_agent_message",
                        &request,
                        TopologyErrorCode::OperationRecoveryRequired,
                    )?;
                }
                Ok::<_, TopologyError>(())
            })
            .await
            .map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?
            .map_err(|error| ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message))
    }

    /// What a delivery that reached no turn does with its mail. An
    /// interrupted child has no park and no wake: its turn waiter only
    /// releases the cancelled turn (spec 03 section 9). The mail is held
    /// (still claimed) until the child can take it, then delivered again:
    /// as the Start this send reserves when the child has no turn, or as a
    /// follow-up into a turn another command admitted. `wait_first` looks
    /// only after the next tree change: the slot was still starting or
    /// retiring, or a redelivery raced it. The send's one `deadline` bounds
    /// the whole wait; the sender's cancellation ends it before any
    /// redelivery.
    async fn after_turn_ended(
        &self,
        snapshot: &MailDeliverySnapshot,
        mut unreached: Unreached,
        wait_first: bool,
        deadline: tokio::time::Instant,
        cancelled: &mut SendCancelled<'_>,
    ) -> Result<TurnEndedMail, ToolRuntimeError> {
        let lock_error = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        };
        let mut wait_first = wait_first;
        let mut cancel_seen = false;
        loop {
            let notified = {
                let look = (!wait_first).then(|| {
                    cancel_seen = cancel_seen || cancelled.as_mut().now_or_never().is_some();
                    TurnEndedLook {
                        expired: tokio::time::Instant::now() >= deadline,
                        cancelled: cancel_seen,
                        unreached,
                    }
                });
                let snapshot = snapshot.clone();
                let (notified, settled) = self
                    .driver
                    .write(move |tree| {
                        let mut notified = Box::pin(tree.waiters().notified_owned());
                        notified.as_mut().enable();
                        let settled = match look {
                            Some(look) => Self::settle_turn_ended(tree, &snapshot, look),
                            None => Ok(None),
                        };
                        (notified, settled)
                    })
                    .await
                    .map_err(|_| lock_error())?;
                if let Some(settled) = settled? {
                    return Ok(settled);
                }
                if !wait_first {
                    // Whatever the send waits for (an unwind, a slot, another
                    // send's Start), the turn its mail targeted is gone.
                    unreached = Unreached::NoTurn;
                }
                notified
            };
            wait_first = false;
            let tick = Box::pin(tokio::time::timeout(TURN_UNWIND_TICK, notified));
            if let Either::Left(((), _)) = select(cancelled.as_mut(), tick).await {
                cancel_seen = true;
            }
        }
    }

    /// One look at a recipient the delivery did not reach: `None` while an
    /// interrupted turn is still unwinding (a turn no waiter will release
    /// fails the send closed instead), while another send's Start for
    /// the child is in flight, or while a slot keeps the next delivery out.
    /// A child with no turn is started by this send, exclusively: it reserves
    /// the Start under the tree lock, and later sends wait for that Start,
    /// then follow up into the turn it admitted. The budget is checked
    /// before any reservation is granted or renewed.
    fn settle_turn_ended(
        tree: &mut ProductAgentTree,
        snapshot: &MailDeliverySnapshot,
        look: TurnEndedLook,
    ) -> Result<Option<TurnEndedMail>, ToolRuntimeError> {
        use crate::protocol::NodeStatus;
        let agent_id = snapshot.agent_id.as_str();
        let state = tree.attempt(agent_id).map(|attempt| {
            (
                attempt.output_json.is_some(),
                attempt.status.is_stopped(),
                attempt.status,
                attempt.turn_id.is_some(),
            )
        });
        let refusal = match state {
            None => Some("its attempt is gone".to_owned()),
            Some((true, ..)) => Some("it submitted its result".to_owned()),
            Some((_, true, status, _)) => Some(format!(
                "its attempt ended {}",
                serde_json::to_value(status)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{status:?}"))
            )),
            Some(_) if tree.is_park_ending(agent_id) => {
                Some("its wake was not admitted and its attempt is ending".to_owned())
            }
            Some(_) => None,
        };
        if let Some(reason) = refusal {
            return Ok(Some(TurnEndedMail::Refused(reason)));
        }
        if tree
            .mail(&snapshot.message_id)
            .is_none_or(|mail| mail.durable != MailDurableState::Accepted)
        {
            return Ok(Some(TurnEndedMail::Left));
        }
        let node = tree.node(agent_id).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        // A parked child's wake carries the mail.
        if tree.is_parked(agent_id) {
            return Ok(Some(TurnEndedMail::Left));
        }
        // A child owed its recover_node takes no delivery until that ran.
        if tree.recovery_owed(agent_id) {
            return Ok(None);
        }
        let bound = state.is_some_and(|(.., bound)| bound);
        let turn = tree.has_current_turn_owner(agent_id);
        let reserved = tree
            .start_reservations
            .get(agent_id)
            .is_some_and(|message| message != &snapshot.message_id);
        let (next, reason) = match node.status {
            // Nobody will release this bound turn: waiting out the budget
            // for it would only stall the send. An active turn not bound yet
            // is a Start still being admitted; its waiter registers with
            // its bind, so it waits below.
            NodeStatus::Interrupted if bound && !tree.has_turn_waiter(agent_id) => {
                return Ok(Some(TurnEndedMail::Unwatched));
            }
            NodeStatus::Interrupted if turn => {
                (None, "its interrupted turn did not finish unwinding")
            }
            NodeStatus::Interrupted if reserved => {
                (None, "its resume turn was not admitted in time")
            }
            NodeStatus::Interrupted => (
                Some(TurnEndedMail::Restart),
                "its resume turn was not admitted in time",
            ),
            // A running child's turn waiter parks or wakes it with this mail.
            NodeStatus::Running if look.unreached == Unreached::EndedTurn => {
                return Ok(Some(TurnEndedMail::Left));
            }
            NodeStatus::Running if turn => (
                Some(TurnEndedMail::FollowUp),
                "the turn it would join was not live in time",
            ),
            NodeStatus::Running if reserved => (None, "its turn was not admitted in time"),
            NodeStatus::Running => (
                Some(TurnEndedMail::Restart),
                "its turn was not admitted in time",
            ),
            // No Start or follow-up is admissible here: the mail waits for
            // the child's next admission.
            _ => return Ok(Some(TurnEndedMail::Left)),
        };
        if look.cancelled {
            return Ok(Some(TurnEndedMail::Cancelled(send_cancelled())));
        }
        // A turn another command admitted is still the child's: its end
        // parks or wakes the child with the unread mail.
        if look.expired && matches!(next, Some(TurnEndedMail::FollowUp)) {
            return Ok(Some(TurnEndedMail::Left));
        }
        if look.expired {
            return Ok(Some(TurnEndedMail::Refused(reason.to_owned())));
        }
        if matches!(next, Some(TurnEndedMail::Restart)) {
            tree.start_reservations
                .insert(agent_id.to_owned(), snapshot.message_id.clone());
        }
        Ok(next)
    }

    /// Refuses a send whose mail no turn will ever carry: the mail is settled
    /// read after its opening is withdrawn or held for recovery, and the
    /// send's Start reservation is released. The receipt becomes
    /// the refusal, so a replay is refused too instead of returning the
    /// success it first recorded.
    fn refuse_turn_ended(
        &self,
        tree: &mut ProductAgentTree,
        operation_id: &str,
        caller: &str,
        value: &Value,
        snapshot: &MailDeliverySnapshot,
        reason: &str,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let code = TopologyErrorCode::RecipientAttemptTerminal;
        let request = decode::<SendAgentMessageRequest>(value).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        if tree.start_reservations.get(&snapshot.agent_id) == Some(&snapshot.message_id) {
            tree.start_reservations.remove(&snapshot.agent_id);
        }
        let settled = tree
            .record_undeliverable_mail(&snapshot.message_id)
            .and_then(|()| tree.project_mail_decisions(&snapshot.agent_id))
            .and_then(|()| {
                if tree.operations().contains_key(operation_id) {
                    tree.remember_failure(
                        operation_id,
                        caller,
                        "send_agent_message",
                        &request,
                        code,
                    )?;
                }
                self.driver.save(tree)
            });
        if let Err(error) = settled {
            return ToolOutcome::failure_text(
                json!({ "code": error.code, "message": error.message }).to_string(),
            );
        }
        ToolOutcome::failure_text(
            json!({
                "code": code,
                "message": format!(
                    "message {} did not reach {}: {reason}; the message was not \
                     delivered",
                    snapshot.message_id, snapshot.agent_id
                ),
            })
            .to_string(),
        )
    }

    async fn finish_followup_start(
        &self,
        agent: &str,
        message: &str,
        uncertain: bool,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Result<bool, ToolRuntimeError> {
        let failure = |error: TopologyError| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        };
        let Some(expected) = self
            .driver
            .read(|tree| tree.owned_followup_attempt(agent, message))
            .map_err(failure)?
            .map_err(failure)?
        else {
            return Ok(false);
        };
        if !uncertain
            && let Some(deliverer) = self.deliverer()
            && deliverer
                .withdraw_followup(
                    self.driver.clone(),
                    agent.to_owned(),
                    message.to_owned(),
                    receipt.clone(),
                )
                .await
                .is_ok()
        {
            return Ok(true);
        }
        let driver = self.driver.clone();
        let (agent, message) = (agent.to_owned(), message.to_owned());
        let (held, effects) = self
            .driver
            .mutate(move |tree| {
                let (agent, message) = (agent.as_str(), message.as_str());
                if tree.node(agent)?.attempt_id.as_ref() == Some(&expected)
                    && !tree
                        .attempt(agent)
                        .is_some_and(|attempt| attempt.status.is_stopped())
                    && (uncertain
                        || tree
                            .attempt(agent)
                            .is_none_or(|attempt| attempt.turn_id.is_none()))
                {
                    tree.record_fault(
                        agent,
                        if uncertain {
                            "followup Start submission outcome is unknown"
                        } else {
                            "followup Start did not reach the runtime but withdrawal was refused"
                        },
                    );
                    tree.record_host_materialization(agent)?;
                    driver.apply_recorded(tree, agent)?;
                }
                if receipt.is_some() {
                    driver.fold_and_commit(tree, |tree| {
                        settle_followup_refusal(tree, agent, message, receipt)
                    })?;
                }
                Ok(())
            })
            .await
            .map_err(failure)?;
        held.map_err(failure)?;
        self.settle(effects).await?;
        Ok(true)
    }

    async fn cancel_followup_send(
        &self,
        operation: &str,
        agent: &str,
        message: &str,
    ) -> Result<(), ToolRuntimeError> {
        self.finish_followup_start(
            agent,
            message,
            false,
            Some((
                operation.to_owned(),
                TopologyErrorCode::RecipientAttemptTerminal,
            )),
        )
        .await?;
        Ok(())
    }

    /// A refused delivery of `message_id`. A `followup` whose Start never
    /// reached the child is undone first, by the send that opened it only:
    /// the child keeps the attempt the followup found, so the caller can
    /// still adopt it, and no candidate is minted for an attempt that never
    /// ran. Anything else is a materialization fault and
    /// the node falls to recovery.
    pub(super) async fn refuse_delivery(
        &self,
        operation: Option<&str>,
        agent_id: &str,
        message_id: &str,
        failure: MailDeliveryFailure,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let (definite_refusal, message) = failure.into_parts();
        if !definite_refusal {
            return self.fail_closed_session(agent_id, message).await;
        }
        let withdrawn = match self.deliverer() {
            Some(deliverer) => {
                deliverer
                    .withdraw_followup(
                        self.driver.clone(),
                        agent_id.to_owned(),
                        message_id.to_owned(),
                        operation.map(|id| (id.to_owned(), TopologyErrorCode::Capacity)),
                    )
                    .await
            }
            None => Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "followup withdrawal has no authoritative session owner",
            )),
        };
        if withdrawn.is_err() {
            return self.fail_closed_session(agent_id, message).await;
        }
        Self::topology_failure(
            &TopologyError::new(
                TopologyErrorCode::Capacity,
                format!(
                    "{message}; the followup attempt was rolled back and {agent_id} still \
                     carries the attempt it had"
                ),
            )
            .with_failure(&message),
        )
    }
}

/// One claimed delivery's progress: its identity, its recorded receipt
/// (rewritten only when a redelivery is admitted), the send's one wait budget,
/// and whether the send is delivering its mail again (a Start it reserved, or
/// a follow-up into a turn another command admitted).
struct SendState {
    operation_id: String,
    snapshot: MailDeliverySnapshot,
    output: SendMessageOutput,
    deadline: tokio::time::Instant,
    redelivered: bool,
}

/// One claimed delivery's identity and recorded receipt, unchanged from the send.
struct ClaimedDelivery {
    operation_id: String,
    snapshot: MailDeliverySnapshot,
    output: SendMessageOutput,
}
