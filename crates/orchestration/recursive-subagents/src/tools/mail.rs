//! Mail: the delivery port and `send_agent_message` reaching, following up or steering a turn.

use super::*;

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

pub(super) struct MailConsumerGuard {
    pub(super) tree: Arc<Mutex<ProductAgentTree>>,
    pub(super) consumer: MailConsumer,
}

impl Drop for MailConsumerGuard {
    fn drop(&mut self) {
        if let Ok(mut tree) = self.tree.lock() {
            match &self.consumer {
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
    Refused(String),
    /// Delivery may have changed runtime authority and requires recovery.
    RecoveryRequired(String),
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
    fn into_parts(self) -> (bool, String) {
        match self {
            Self::Refused(message) => (true, message),
            Self::RecoveryRequired(message) => (false, message),
            Self::TurnEnded => (true, "the child's turn ended before delivery".to_owned()),
            Self::SlotBusy => (
                true,
                "the child's live slot was still starting or retiring".to_owned(),
            ),
        }
    }
}

impl From<String> for MailDeliveryFailure {
    fn from(message: String) -> Self {
        Self::RecoveryRequired(message)
    }
}

impl From<&str> for MailDeliveryFailure {
    fn from(message: &str) -> Self {
        Self::RecoveryRequired(message.to_owned())
    }
}

/// Starts, follows up, or steers the bound child session for one mail record.
///
/// A Start the deliverer admits is bound to the recipient's attempt and
/// marked active by the deliverer, under the tree lock, before that turn can
/// end. A follow-up or steer joins the turn the attempt already binds; a
/// follow-up the runtime admits as the next turn is bound by the host's turn
/// waiter when the turn before it ends. The send never binds or activates a
/// turn or acknowledges mail: the deliverer records a command intent and
/// projects its committed acceptance, including the exact envelope, before
/// returning success. A no-fact refusal remains a keyed host decision.
pub trait MailDeliverer: Send + Sync {
    /// Withdraws only after a definitive Start refusal, holding startup and
    /// exact session ownership across an authoritative tail read and tree CAS.
    /// The opening mail and supplied send failure receipt settle in that CAS.
    fn withdraw_followup(
        &self,
        driver: TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>>;

    /// Hands one accepted message to the recipient's session, in whatever
    /// shape the tree's `disposition` already chose.
    fn deliver(
        &self,
        request: MailDeliveryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>;
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

impl TopologyTool {
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
            let (prepared, effects) = self
                .driver
                .mutate(|tree| {
                    let result = tree.rehydrate_followup(&caller, &request, &operation_id);
                    self.driver.save(tree).and(result)
                })
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
        }
        let (output, effects) = self
            .driver
            .mutate(|tree| {
                decode::<SendAgentMessageRequest>(value).and_then(|request| {
                    self.dispatch_mutating(
                        tree,
                        &caller,
                        &operation_id,
                        "send_agent_message",
                        &request,
                        |tree, caller, request, operation_id| {
                            tree.send_agent_message(caller, request, operation_id)
                        },
                    )
                    .map(|(output, _replayed)| output)
                })
            })
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
        let Some(deliverer) = &self.deliverer else {
            return ToolOutcome::success_text(json!(output).to_string());
        };
        let snapshot = loop {
            let notify = {
                let mut tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let Some(mail) = tree.mail(&output.message_id) else {
                    // Only acknowledged history is pruned; its durable send
                    // receipt remains replayable without delivering it again.
                    return ToolOutcome::success_text(json!(output).to_string());
                };
                // Root and parked-child mail waits for the host's next
                // admission of that node; it is never delivered live.
                if mail.to == tree.root_agent_id
                    || mail.durable != MailDurableState::Accepted
                    || tree.delivery_claims.contains(&mail.message_id)
                    || tree.is_parked(&mail.to)
                {
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                if tree.parked_waits.iter().any(|((recipient, _), targets)| {
                    recipient == &mail.to && targets.contains(&mail.from)
                }) || (output.disposition == MailDisposition::Started
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
                    notified
                } else {
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
                    break snapshot;
                }
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
        let _delivery = MailConsumerGuard {
            tree: Arc::clone(&self.driver.tree),
            consumer: MailConsumer::Delivery(output.message_id.clone()),
        };
        // A send's Starts to one child are exclusive: the reservation lasts
        // until the send returns.
        let mut start =
            (snapshot.disposition == MailDisposition::Started).then(|| MailConsumerGuard {
                tree: Arc::clone(&self.driver.tree),
                consumer: MailConsumer::Start {
                    agent: snapshot.agent_id.clone(),
                    message: snapshot.message_id.clone(),
                },
            });
        if snapshot.agent_id == snapshot.root_id {
            return ToolOutcome::success_text(json!(output).to_string());
        }
        if snapshot.durable != MailDurableState::Accepted {
            return ToolOutcome::success_text(json!(output).to_string());
        }
        let Some(session_locator) = snapshot.session_locator.clone() else {
            return self.fail_closed_session(
                &snapshot.agent_id,
                "child session locator is missing; spawn bind did not complete".into(),
            );
        };
        let mut snapshot = snapshot;
        let mut output = output;
        // One budget for the whole send, however often it redelivers.
        let deadline = tokio::time::Instant::now() + TURN_UNWIND_WAIT;
        // Set once the send is delivering its mail again (a Start it reserved,
        // or a follow-up into a turn another command admitted). The recorded
        // receipt changes only when that delivery is admitted.
        let mut redelivered = false;
        loop {
            if cancelled.as_mut().now_or_never().is_some() {
                self.cancel_followup_send(&operation_id, &snapshot.agent_id, &snapshot.message_id)
                    .await?;
                return Err(send_cancelled());
            }
            // The budget bounds waiting for a turn, never a submission in
            // flight: cutting one would fence an attempt whose turn may be
            // running. The delivery is bounded by its own reopen and admission.
            let delivery = Box::pin(deliverer.deliver(MailDeliveryRequest {
                agent_id: snapshot.agent_id.clone(),
                session_locator: session_locator.clone(),
                disposition: snapshot.disposition,
                message_id: snapshot.message_id.clone(),
                body: snapshot.body.clone(),
                description: snapshot.description.clone(),
                prompt: snapshot.prompt.clone(),
                output_schema_json: snapshot.output_schema_json.clone(),
                path: snapshot.path.clone(),
                turn_token: snapshot.turn_token,
            }));
            let answer = match select(cancelled.as_mut(), delivery).await {
                Either::Left(_) => {
                    self.finish_followup_start(
                        &snapshot.agent_id,
                        &snapshot.message_id,
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
                    let mut tree = self.driver.tree.lock().map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })?;
                    // Only an admitted redelivery rewrites the receipt. Until
                    // that admission `output` stays the receipt already
                    // recorded, so a later `Left` does not answer the
                    // in-memory follow-up.
                    if redelivered {
                        output.disposition = snapshot.disposition;
                        if let Err(error) = self.record_redelivery(
                            &mut tree,
                            &operation_id,
                            &caller,
                            value,
                            &output,
                        ) {
                            return ToolOutcome::failure_text(
                                json!({ "code": error.code, "message": error.message }).to_string(),
                            );
                        }
                    }
                    // A Start's deliverer bound the turn it admitted before
                    // that turn could end, and a joined turn was already
                    // bound; the send binds nothing. A turn that has ended,
                    // or that a later turn replaced, is never bound or marked
                    // active again here.
                    if let Err(error) = self.driver.save(&mut tree) {
                        return ToolOutcome::failure_text(
                            json!({ "code": error.code, "message": error.message }).to_string(),
                        );
                    }
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                // The claim is released on return. Unread mail wakes a running
                // child whose turn ended; a child with no turn is started by
                // this same mail once its slot is vacant; a turn another
                // command admitted gets the mail as a follow-up; an attempt
                // that already submitted or ended is never woken, so the send
                // is refused and the mail settled read.
                Err(failure @ (MailDeliveryFailure::TurnEnded | MailDeliveryFailure::SlotBusy)) => {
                    let busy = failure == MailDeliveryFailure::SlotBusy;
                    let unreached = if busy || snapshot.disposition == MailDisposition::Started {
                        Unreached::NoTurn
                    } else {
                        Unreached::EndedTurn
                    };
                    match self
                        .after_turn_ended(
                            &snapshot,
                            unreached,
                            busy || (redelivered && unreached == Unreached::NoTurn),
                            deadline,
                            &mut cancelled,
                        )
                        .await?
                    {
                        TurnEndedMail::Left => {
                            // `output` is still the recorded receipt. The
                            // follow-up disposition lives only on `snapshot`
                            // until a turn admits it.
                            return ToolOutcome::success_text(json!(output).to_string());
                        }
                        TurnEndedMail::Refused(reason) => {
                            self.finish_followup_start(
                                &snapshot.agent_id,
                                &snapshot.message_id,
                                false,
                                Some((
                                    operation_id.clone(),
                                    TopologyErrorCode::RecipientAttemptTerminal,
                                )),
                            )
                            .await?;
                            let mut tree = self.driver.lock().map_err(|error| {
                                ToolRuntimeError::new(
                                    ToolRuntimeErrorKind::Execution,
                                    error.message,
                                )
                            })?;
                            return self.refuse_turn_ended(
                                &mut tree,
                                &operation_id,
                                &caller,
                                value,
                                &snapshot,
                                &reason,
                            );
                        }
                        TurnEndedMail::Cancelled(error) => {
                            self.cancel_followup_send(
                                &operation_id,
                                &snapshot.agent_id,
                                &snapshot.message_id,
                            )
                            .await?;
                            return Err(error);
                        }
                        TurnEndedMail::Unwatched => {
                            if redelivered {
                                self.remember_redelivery_failure(&operation_id, &caller, value)?;
                            }
                            return self
                                .refuse_delivery(
                                    Some(&operation_id),
                                    &snapshot.agent_id,
                                    &snapshot.message_id,
                                    MailDeliveryFailure::RecoveryRequired(
                                        "the child's interrupted turn is still bound and no turn \
                                     waiter is left to release it"
                                            .to_owned(),
                                    ),
                                )
                                .await;
                        }
                        TurnEndedMail::Restart => {
                            let _ = start.get_or_insert_with(|| MailConsumerGuard {
                                tree: Arc::clone(&self.driver.tree),
                                consumer: MailConsumer::Start {
                                    agent: snapshot.agent_id.clone(),
                                    message: snapshot.message_id.clone(),
                                },
                            });
                            snapshot.disposition = MailDisposition::Started;
                            redelivered = true;
                        }
                        TurnEndedMail::FollowUp => {
                            snapshot.disposition = MailDisposition::Queued;
                            redelivered = true;
                        }
                    }
                }
                Err(failure) => {
                    // After a redelivery the receipt becomes this refusal, so
                    // it never records a delivery no turn admitted and a
                    // replay is refused too.
                    if redelivered {
                        self.remember_redelivery_failure(&operation_id, &caller, value)?;
                    }
                    return self
                        .refuse_delivery(
                            Some(&operation_id),
                            &snapshot.agent_id,
                            &snapshot.message_id,
                            failure,
                        )
                        .await;
                }
            }
        }
    }

    /// Rewrites an admitted redelivery's mail and operation receipt to the
    /// delivery that carried it: `started` for a Start, `queued` for a
    /// follow-up into a turn another command admitted (spec 03 sections 4.3
    /// and 9).
    fn record_redelivery(
        &self,
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
    fn remember_redelivery_failure(
        &self,
        operation_id: &str,
        caller: &str,
        value: &Value,
    ) -> Result<(), ToolRuntimeError> {
        let request = decode::<SendAgentMessageRequest>(value).map_err(|error| {
            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
        })?;
        let mut tree = self.driver.tree.lock().map_err(|_| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        })?;
        if tree.operations().contains_key(operation_id) {
            tree.remember_failure(
                operation_id,
                caller,
                "send_agent_message",
                &request,
                TopologyErrorCode::OperationRecoveryRequired,
            )
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        }
        Ok(())
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
                let mut tree = self.driver.tree.lock().map_err(|_| lock_error())?;
                let mut notified = Box::pin(tree.waiters().notified_owned());
                notified.as_mut().enable();
                if !wait_first {
                    cancel_seen = cancel_seen || cancelled.as_mut().now_or_never().is_some();
                    let look = TurnEndedLook {
                        expired: tokio::time::Instant::now() >= deadline,
                        cancelled: cancel_seen,
                        unreached,
                    };
                    if let Some(settled) = self.settle_turn_ended(&mut tree, snapshot, look)? {
                        return Ok(settled);
                    }
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
        &self,
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
            && let Some(deliverer) = &self.deliverer
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
        let (held, effects) = self
            .driver
            .mutate(|tree| {
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
                    self.driver.apply_recorded(tree, agent)?;
                }
                if receipt.is_some() {
                    self.driver.fold_and_commit(tree, |tree| {
                        settle_followup_refusal(tree, agent, message, receipt)
                    })?;
                }
                Ok(())
            })
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
            return self.fail_closed_session(agent_id, message);
        }
        let withdrawn = match &self.deliverer {
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
            return self.fail_closed_session(agent_id, message);
        }
        ToolOutcome::failure_text(
            json!({
                "code": if definite_refusal {
                    TopologyErrorCode::Capacity
                } else {
                    TopologyErrorCode::OperationRecoveryRequired
                },
                "message": format!(
                    "{message}; the followup attempt was rolled back and {agent_id} still \
                     carries the attempt it had"
                ),
            })
            .to_string(),
        )
    }
}
