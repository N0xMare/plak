//! Agent mail and the wait/notify seam.

use super::{
    Arc, AwaitAgents, AwaitOutput, DeliveredRow, INBOX_BODY_BYTES, InboxMail, MAX_MAILBOX_RECORDS,
    MAX_QUEUED_MAIL_BYTES, MailDisposition, MailDurableState, MailRecord, MessagePriority,
    MessagePurpose, NodeStatus, ProductAgentTree, RESULT_MAIL_OUTPUT_BYTES,
    SendAgentMessageRequest, SendMessageOutput, TopologyError, TopologyErrorCode, TopologyGrant,
    WaitAgentRow, WaitReceiptSettlement, WaitWakeReason, attempt_status_label, bounded_text,
    first_line_bounded, topology_mail_request_invalid_message,
};

impl ProductAgentTree {
    /// One non-blocking look at whether a wait has settled: `Some` once any
    /// target settled or mail arrived for the caller, and `None` while the
    /// caller should park again.
    ///
    /// The wait is poll-driven by design; this is the poll.
    ///
    /// # Errors
    ///
    /// Refuses an empty target set and a target the caller cannot see.
    pub(crate) fn poll_wait(
        &self,
        caller_id: &str,
        agents: &AwaitAgents,
    ) -> Result<Option<AwaitOutput>, TopologyError> {
        let grant = self.grant(caller_id)?;
        let targets = self.wait_targets(grant, caller_id, agents)?;
        // The root's own turn is cancellable while it waits. An interrupt
        // that reached the caller's node settles the wait immediately instead
        // of leaving the root parked behind a poll loop it cannot leave.
        if self
            .node(caller_id)
            .is_ok_and(|node| node.status.settles_a_wait())
        {
            return Err(TopologyError::new(
                TopologyErrorCode::WaitCallerSettled,
                "the waiting agent's own turn was interrupted; the wait is abandoned",
            ));
        }
        let mut rows = Vec::new();
        let mut newly_settled = Vec::new();
        for id in &targets {
            if !self.can_list(grant, id) {
                return Err(TopologyError::new(
                    TopologyErrorCode::GrantDenied,
                    "wait target is not grant-visible",
                ));
            }
            let row = self.wait_row(id)?;
            if self.row_settled(&row) && !self.delivered_to(caller_id, id) {
                newly_settled.push(id.clone());
            }
            rows.push(row);
        }
        self.drop_delivered_outputs(caller_id, agents, &mut rows);
        let mut settled = !newly_settled.is_empty()
            || (matches!(agents, AwaitAgents::Named(_))
                && targets.iter().all(|id| self.delivered_to(caller_id, id)));
        let mail = self.wait_mail(caller_id, &targets);
        let row_wake = settled
            || (*agents == AwaitAgents::Pending
                && targets.iter().all(|id| self.delivered_to(caller_id, id)));
        if !settled {
            settled = row_wake
                || mail.iter().any(|mail| {
                    matches!(
                        mail.purpose,
                        MessagePurpose::Question | MessagePurpose::Reply
                    ) || mail.priority == MessagePriority::Urgent
                });
        }
        if settled {
            let woke_by = if row_wake {
                WaitWakeReason::Settled
            } else {
                WaitWakeReason::Mail
            };
            Ok(Some(AwaitOutput {
                mail,
                woke_by,
                agents: rows,
                timed_out: false,
                processes: Vec::new(),
                jobs: Vec::new(),
                omitted: None,
                newly_settled,
            }))
        } else {
            Ok(None)
        }
    }

    /// The same rows [`Self::poll_wait`] would return, but for a wait whose
    /// ceiling has expired: `timed_out` is set and the rows are still the
    /// tree's truth at that moment, never an error.
    ///
    /// # Errors
    ///
    /// Refuses a malformed target list and a target the caller cannot see.
    pub(crate) fn timeout_wait(
        &self,
        caller_id: &str,
        agents: &AwaitAgents,
    ) -> Result<AwaitOutput, TopologyError> {
        if let Some(output) = self.poll_wait(caller_id, agents)? {
            Ok(output)
        } else {
            let grant = self.grant(caller_id)?;
            let targets = self.wait_targets(grant, caller_id, agents)?;
            let mut rows = targets
                .iter()
                .map(|id| self.wait_row(id))
                .collect::<Result<Vec<_>, _>>()?;
            self.drop_delivered_outputs(caller_id, agents, &mut rows);
            Ok(AwaitOutput {
                mail: self.wait_mail(caller_id, &targets),
                woke_by: WaitWakeReason::Timeout,
                agents: rows,
                timed_out: true,
                processes: Vec::new(),
                jobs: Vec::new(),
                omitted: None,
                newly_settled: Vec::new(),
            })
        }
    }

    fn wait_mail(&self, caller_id: &str, targets: &[String]) -> Vec<InboxMail> {
        let mut mail: Vec<_> = self
            .inbox_for(caller_id, false)
            .into_iter()
            .filter(|mail| {
                targets.contains(&mail.from) && !self.delivery_claims.contains(&mail.message_id)
            })
            .map(|mut preview| {
                if let Some(record) = self.messages.get(&preview.message_id) {
                    preview.body.clone_from(&record.body);
                }
                preview
            })
            .collect();
        mail.reverse();
        mail
    }

    /// The rows a timed-out wait would return, for out-of-crate render
    /// fixtures. It acknowledges nothing and marks nothing delivered: only
    /// the await, after its budget, may do either.
    ///
    /// # Errors
    ///
    /// Fails exactly as the timed-out wait it reads through.
    #[cfg(feature = "test-support")]
    pub fn timeout_wait_rows_for_test(
        &self,
        caller_id: &str,
        agents: &AwaitAgents,
    ) -> Result<AwaitOutput, TopologyError> {
        self.timeout_wait(caller_id, agents)
    }

    /// A bare await watches a delivered child only for its unread mail, so it
    /// returns that row without the output the caller already holds: re-reads
    /// exist only for named awaits. The row's Result envelope, if still
    /// unread, is then budgeted as plain mail.
    fn drop_delivered_outputs(
        &self,
        caller_id: &str,
        agents: &AwaitAgents,
        rows: &mut [WaitAgentRow],
    ) {
        if *agents != AwaitAgents::Pending {
            return;
        }
        for row in rows {
            if self.delivered_to(caller_id, &row.agent_id) {
                row.output_json = None;
            }
        }
    }

    /// Whether a row ends a wait. A `Sealing` node has not settled: its
    /// attempt is already terminal, so settling on that alone returned a row
    /// with status "sealing" and no candidate on it, and the caller could not
    /// adopt what the wait told it about.
    pub(super) fn row_settled(&self, row: &WaitAgentRow) -> bool {
        !self.seal_pending(&row.agent_id)
            && row.status != NodeStatus::Sealing
            && (row.status.settles_a_wait() || row.attempt.is_stopped())
    }

    /// Records the settled rows a wait handed to `caller_id`, so a later bare
    /// wait does not return them again. Call it only once the result has
    /// been built. A row this wait delivers is no longer an admission's mark:
    /// returning that admission must not withdraw it.
    pub(crate) fn mark_wait_delivered(&mut self, caller_id: &str, output: &AwaitOutput) {
        for row in &output.agents {
            if self.row_settled(row) && !output.output_withheld(&row.agent_id) {
                let key = (caller_id.to_owned(), row.agent_id.clone());
                self.delivered.insert(key.clone(), DeliveredRow::of(row));
                self.admission_marks.remove(&key);
            }
        }
    }

    /// Whether `caller_id` already received this child's current settled row.
    pub(crate) fn delivered_to(&self, caller_id: &str, id: &str) -> bool {
        self.delivered
            .get(&(caller_id.to_owned(), id.to_owned()))
            .is_some_and(|delivered| self.wait_row(id).is_ok_and(|row| delivered.matches(&row)))
            || self.current_result_mail(id).is_some_and(|mail| {
                mail.to == caller_id && mail.durable == MailDurableState::Acknowledged
            })
    }

    fn current_result_mail(&self, agent_id: &str) -> Option<&MailRecord> {
        let attempt = self.current_attempt(agent_id)?;
        self.messages.values().find(|mail| {
            mail.purpose == MessagePurpose::Result
                && mail.from == agent_id
                && serde_json::from_str::<serde_json::Value>(&mail.body).is_ok_and(|body| {
                    body["attempt_id"].as_str() == Some(attempt.attempt_id.as_str())
                })
        })
    }

    /// Whether `caller_id` has unread, unclaimed mail from `id` to carry on
    /// the next return. Reads every inbound id, not the capped
    /// [`Self::unread_mail`] view, so a sender whose envelope sits past the
    /// mailbox cap is still watched.
    fn has_unread_mail_from(&self, caller_id: &str, id: &str) -> bool {
        self.inbound
            .get(caller_id)
            .into_iter()
            .flatten()
            .filter_map(|message_id| self.messages.get(message_id))
            .any(|mail| {
                mail.from == id
                    && mail.durable == MailDurableState::Accepted
                    && !self.delivery_claims.contains(&mail.message_id)
            })
    }

    /// Named ids; or, when the request omits them, every managed child that
    /// is not closed and has something the caller has not seen yet: a row it
    /// has not received, or mail it has not read (including mail an earlier
    /// return's budget withheld after delivering the row). Waiting for
    /// oneself is refused either way.
    pub(super) fn wait_targets(
        &self,
        grant: &TopologyGrant,
        caller_id: &str,
        agents: &AwaitAgents,
    ) -> Result<Vec<String>, TopologyError> {
        let targets = if let AwaitAgents::Named(ids) = agents {
            ids.clone()
        } else if *agents == AwaitAgents::Unwatched {
            return Ok(Vec::new());
        } else {
            let pending: Vec<String> = grant
                .managed_ids
                .iter()
                .filter(|id| {
                    self.node(id)
                        .is_ok_and(|node| node.status != NodeStatus::Closed)
                        && (!self.delivered_to(caller_id, id)
                            || self.has_unread_mail_from(caller_id, id))
                })
                .cloned()
                .collect();
            if pending.is_empty() && !grant.managed_ids.is_empty() {
                return Err(TopologyError::new(
                    TopologyErrorCode::NoPendingChildren,
                    "every child you manage has already reported or is closed; name agent_ids to read a result again",
                ));
            }
            pending
        };
        if targets.iter().any(|id| id == caller_id) {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "waiting for self is forbidden",
            ));
        }
        if targets.is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "wait_agent has nothing to watch: name agent_ids or spawn a child first",
            ));
        }
        Ok(targets)
    }

    /// Acknowledge only the exact envelopes successfully returned to the caller.
    pub(crate) fn acknowledge_wait_mail(&mut self, caller_id: &str, output: &AwaitOutput) {
        for row in &output.mail {
            if self
                .mail(&row.message_id)
                .is_some_and(|mail| mail.to == caller_id)
            {
                let _ = self.acknowledge_mail(&row.message_id);
            }
        }
    }

    /// The wait targets a request resolves to, for callers outside the tree.
    pub fn wait_target_ids(
        &self,
        caller_id: &str,
        agents: &AwaitAgents,
    ) -> Result<Vec<String>, TopologyError> {
        let grant = self.grant(caller_id)?;
        self.wait_targets(grant, caller_id, agents)
    }

    /// Writes one message into the tree's mailbox and decides what it does to
    /// the recipient's turn: start it, steer it, or queue behind it.
    ///
    ///
    /// # Errors
    ///
    /// Refuses an oversized body, a recipient the caller may not address, and
    /// a recipient that is already terminal.
    pub fn send_agent_message(
        &mut self,
        caller_id: &str,
        request: &SendAgentMessageRequest,
        operation_id: Option<&str>,
    ) -> Result<SendMessageOutput, TopologyError> {
        if let Some(prior) = self.replay_typed(operation_id, "send_agent_message", request)? {
            return Ok(prior);
        }
        self.require_live()?;
        request.validate(caller_id).map_err(|code| {
            TopologyError::new(
                code,
                topology_mail_request_invalid_message(code, request, caller_id),
            )
        })?;
        if !self
            .grant(caller_id)?
            .message_peers
            .iter()
            .any(|id| id == &request.agent_id)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "recipient is not a message peer",
            ));
        }
        let (target_status, target_active_turn) = {
            let target = self.node(&request.agent_id)?;
            let settled = self
                .current_attempt(&request.agent_id)
                .is_some_and(|attempt| attempt.status.is_stopped());
            (
                target.status,
                !settled && self.has_current_turn_owner(&request.agent_id),
            )
        };
        let purpose = request.purpose.unwrap_or(MessagePurpose::Coordinate);
        // `followup` exists precisely for the states the ordinary
        // refusals below reject — a finished node and its terminal attempt —
        // so it is admitted against its own rules and opens a fresh attempt
        // once the mailbox budgets have passed.
        let followup_schema = if purpose == MessagePurpose::Followup {
            if self.reclaimed(&self.node(&request.agent_id)?.workspace_id) {
                return Err(TopologyError::new(
                    TopologyErrorCode::FollowupWorkspaceGone,
                    "reclaimed checkout must be rehydrated before opening a followup",
                ));
            }
            Some(self.admit_followup(
                &request.agent_id,
                target_status,
                target_active_turn,
                request.output_schema_json.as_deref(),
            )?)
        } else {
            if target_status == NodeStatus::Closed {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecipientClosed,
                    "recipient is closed",
                ));
            }
            if target_status == NodeStatus::RecoveryRequired {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecipientRecoveryRequired,
                    "recipient requires recovery",
                ));
            }
            if self
                .current_attempt(&request.agent_id)
                .is_some_and(|attempt| attempt.status.is_stopped())
            {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecipientAttemptTerminal,
                    "recipient attempt is already terminal",
                ));
            }
            // Its wake was not admitted and the host is ending the attempt:
            // nothing would ever deliver this mail.
            if self.is_park_ending(&request.agent_id) {
                return Err(TopologyError::new(
                    TopologyErrorCode::RecipientAttemptTerminal,
                    "recipient attempt is ending",
                ));
            }
            None
        };
        if purpose == MessagePurpose::Reply {
            let Some(reply_to) = request
                .in_reply_to
                .as_deref()
                .filter(|value| !value.is_empty())
            else {
                return Err(TopologyError::new(
                    TopologyErrorCode::ReplyInvalid,
                    "reply requires in_reply_to",
                ));
            };
            let referenced = self.messages.get(reply_to).ok_or_else(|| {
                TopologyError::new(
                    TopologyErrorCode::ReplyInvalid,
                    "referenced message is missing",
                )
            })?;
            if referenced.to != caller_id || referenced.from != request.agent_id {
                return Err(TopologyError::new(
                    TopologyErrorCode::ReplyInvalid,
                    "reply must reverse the referenced direction",
                ));
            }
        }
        if self.inbound.get(&request.agent_id).is_some_and(|inbound| {
            inbound
                .iter()
                .filter(|id| {
                    self.messages
                        .get(*id)
                        .is_some_and(|mail| mail.durable == MailDurableState::Accepted)
                })
                .count()
                >= MAX_MAILBOX_RECORDS
        }) {
            return Err(TopologyError::new(
                TopologyErrorCode::MailboxFull,
                "recipient mailbox record budget is exhausted",
            ));
        }
        let queued_bytes: usize = self
            .messages
            .values()
            .filter(|mail| mail.durable == MailDurableState::Accepted)
            .map(|mail| mail.body.len())
            .sum();
        if queued_bytes.saturating_add(request.message.len()) > MAX_QUEUED_MAIL_BYTES {
            return Err(TopologyError::new(
                TopologyErrorCode::MailboxFull,
                "tree queued-mail byte budget is exhausted",
            ));
        }
        let message_id = self.mint_message_id();
        if let Some((output_schema_json, schema_digest)) = followup_schema {
            self.open_followup_attempt(
                &request.agent_id,
                &message_id,
                output_schema_json,
                schema_digest,
            )?;
        }
        let priority = request.priority.unwrap_or_else(|| {
            if target_active_turn
                && target_status == NodeStatus::Running
                && self.nodes[&request.agent_id].parent_agent_id.as_deref() == Some(caller_id)
            {
                MessagePriority::Urgent
            } else {
                MessagePriority::Deferred
            }
        });
        // A bound turn survives restore_paused (active_turn is cleared). Restart
        // drain is forbidden: that child gets follow-up/steer, never a second Start.
        let has_bound_turn = self
            .current_attempt(&request.agent_id)
            .and_then(|attempt| attempt.turn_id.as_ref())
            .is_some();
        // A parked child's next turn is the host's wake, never a Start here.
        let disposition =
            if !target_active_turn && !has_bound_turn && !self.is_parked(&request.agent_id) {
                MailDisposition::Started
            } else if priority == MessagePriority::Urgent {
                MailDisposition::Steered
            } else {
                MailDisposition::Queued
            };
        let mail = MailRecord {
            message_id: message_id.clone(),
            from: caller_id.to_owned(),
            to: request.agent_id.clone(),
            body: request.message.clone(),
            purpose,
            priority,
            in_reply_to: request.in_reply_to.clone(),
            disposition,
            durable: MailDurableState::Accepted,
        };
        self.inbound
            .entry(request.agent_id.clone())
            .or_default()
            .push(message_id.clone());
        self.messages.insert(message_id.clone(), mail);
        self.bump();
        let output = SendMessageOutput {
            message_id,
            disposition,
        };
        if let Some(id) = operation_id {
            self.remember_success(id, caller_id, "send_agent_message", request, &output)?;
        }
        Ok(output)
    }

    /// Records how a message whose first delivery reached no turn was
    /// delivered again: as the admitted Start of the turn it began, or as a
    /// follow-up into a turn another command admitted (spec 03 sections 4.3
    /// and 9). The admitted delivery may already have acknowledged the mail,
    /// and pruned history keeps no record to rewrite.
    pub fn redeliver_mail(&mut self, message_id: &str, disposition: MailDisposition) {
        if let Some(mail) = self.messages.get_mut(message_id) {
            mail.disposition = disposition;
            self.bump();
        }
    }

    /// Pushed completion: a host-authored `result` message into
    /// the parent's inbox, so a parent that is not sitting in `wait_agent`
    /// still learns its child finished without polling. `MessagePurpose::Result`
    /// is refused from a model in `SendAgentMessageRequest::validate`.
    ///
    /// Enqueued from `finish_seal` where there is a seal, so the mail can name
    /// the candidate; a `read_only` node seals nothing and pushes from
    /// `record_turn_completed` instead. One message per attempt.
    /// Pushes the completion now unless a seal is about to.
    ///
    /// A queued ticket means the runtime will land the seal; pushing from
    /// here would send a `result` with no candidate and then suppress the one
    /// that has it.
    pub(super) fn push_result_unless_sealing(&mut self, agent_id: &str) {
        if self.result_effect_reserved(agent_id) || self.seal_pending(agent_id) {
            return;
        }
        if self
            .nodes
            .get(agent_id)
            .is_some_and(|node| node.status == NodeStatus::Sealing)
        {
            return;
        }
        self.enqueue_result_mail(agent_id);
    }

    fn result_effect_reserved(&self, agent_id: &str) -> bool {
        self.current_attempt(agent_id).is_some_and(|attempt| {
            self.operations
                .contains_key(&format!("plak:result:{agent_id}:{}", attempt.attempt_id))
        })
    }

    pub(super) fn enqueue_result_mail(&mut self, agent_id: &str) {
        if self.result_effect_reserved(agent_id) {
            return;
        }
        let Some(node) = self.nodes.get(agent_id) else {
            return;
        };
        if node.status == NodeStatus::Closed {
            return;
        }
        let Some(parent) = node.parent_agent_id.clone() else {
            return;
        };
        if !self.nodes.contains_key(&parent) {
            return;
        }
        let Some(attempt) = self.current_attempt(agent_id) else {
            return;
        };
        // a `result` reports a finished attempt. An interrupted
        // turn leaves the attempt `running`, and pushing from there sent the
        // parent `{"status":"running"}` and then suppressed the real one.
        if !attempt.status.is_stopped() || attempt.status == super::AttemptStatus::Closed {
            return;
        }
        let attempt_id = attempt.attempt_id.clone();
        let label = attempt_status_label(attempt.status);
        let candidate = attempt.candidate_id.clone();
        let output_json = attempt.output_json.clone();
        let continuation = attempt.candidate_handoff();
        // Mail history can prune a Closed node's envelope. The reservation,
        // including a capacity-refused push, survives view deletion instead.
        let effect_id = format!("plak:result:{agent_id}:{attempt_id}");
        let canonical = serde_json::json!({"attempt_id": attempt_id.as_str()}).to_string();
        self.operations.insert(
            effect_id.clone(),
            super::OperationRecord {
                fold: None,
                operation_id: effect_id,
                caller_agent_id: agent_id.to_owned(),
                tool_name: "result_effect".to_owned(),
                request_digest: super::digest_text(&canonical),
                canonical_request: canonical,
                terminal_code: None,
                terminal_response: Some("effects_reserved".to_owned()),
            },
        );
        self.bump();
        if self.inbound.get(&parent).is_some_and(|inbound| {
            inbound
                .iter()
                .filter(|id| {
                    self.messages
                        .get(*id)
                        .is_some_and(|mail| mail.durable == MailDurableState::Accepted)
                })
                .count()
                >= MAX_MAILBOX_RECORDS
        }) {
            // The wait row still carries the attempt's result; only the push
            // is lost, and the canvas says so instead of dropping it silently.
            self.fault_reasons.insert(
                agent_id.to_owned(),
                format!(
                    "result mail for {attempt_id} was not pushed: {parent}'s inbox holds \
                     {MAX_MAILBOX_RECORDS} records; the wait row still carries the result"
                ),
            );
            return;
        }
        // The envelope the TUI's `mail_row` collapses into one transcript line:
        // who finished, which attempt, which candidate, and what it said. The
        // full `output_json` stays on the wait row and in the child's own
        // transcript, so only its first line travels here and only as much of
        // it as the message budget holds.
        let mut envelope = serde_json::json!({
            "purpose": "result",
            "from": agent_id,
            "attempt_id": attempt_id.as_str(),
            "status": label,
        });
        if let Some(candidate) = candidate {
            envelope["candidate"] = serde_json::Value::String(candidate.to_string());
        }
        if let Some(continuation) = continuation {
            envelope["continuation"] = serde_json::Value::String(continuation);
        }
        if let Some(output) = output_json {
            envelope["output"] =
                serde_json::Value::String(first_line_bounded(&output, RESULT_MAIL_OUTPUT_BYTES));
            envelope["output_bytes"] = serde_json::Value::from(output.len());
            envelope["output_truncated"] = serde_json::Value::Bool(
                envelope["output"]
                    .as_str()
                    .is_some_and(|preview| preview.len() < output.len()),
            );
            envelope["full_output_via"] = serde_json::Value::String("wait_agent".to_owned());
        }
        let mut body = envelope.to_string();
        while body.len() > INBOX_BODY_BYTES {
            let Some(output) = envelope
                .get("output")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                return;
            };
            let shorter = bounded_text(
                &output,
                output
                    .len()
                    .saturating_sub(body.len() - INBOX_BODY_BYTES)
                    .max(1),
            );
            envelope["output"] = serde_json::Value::String(if shorter.len() == output.len() {
                String::new()
            } else {
                shorter
            });
            envelope["output_truncated"] = serde_json::Value::Bool(true);
            body = envelope.to_string();
        }
        let queued_bytes: usize = self
            .messages
            .values()
            .filter(|mail| mail.durable == MailDurableState::Accepted)
            .map(|mail| mail.body.len())
            .sum();
        if queued_bytes.saturating_add(body.len()) > MAX_QUEUED_MAIL_BYTES {
            self.fault_reasons.insert(agent_id.to_owned(), format!("result mail for {attempt_id} was not pushed: tree mailbox byte capacity exhausted; the wait row still carries the result"));
            return;
        }
        let message_id = self.mint_message_id();
        self.inbound
            .entry(parent.clone())
            .or_default()
            .push(message_id.clone());
        self.messages.insert(
            message_id.clone(),
            MailRecord {
                message_id,
                from: agent_id.to_owned(),
                to: parent,
                body,
                purpose: MessagePurpose::Result,
                priority: MessagePriority::Deferred,
                in_reply_to: None,
                disposition: MailDisposition::Queued,
                durable: MailDurableState::Accepted,
            },
        );
    }

    /// Marks one accepted message as reported to its recipient, so the next
    /// wait does not settle on it again.
    ///
    /// # Errors
    ///
    /// Refuses an unknown message id.
    pub fn acknowledge_mail(&mut self, message_id: &str) -> Result<(), TopologyError> {
        let mail = self.messages.get_mut(message_id).ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "mail record is missing",
            )
        })?;
        let changed = mail.durable == MailDurableState::Accepted;
        if changed {
            mail.durable = MailDurableState::Acknowledged;
        }
        let recipient = mail.to.clone();
        let result_sender = (mail.purpose == MessagePurpose::Result).then(|| {
            (
                mail.from.clone(),
                serde_json::from_str::<serde_json::Value>(&mail.body).ok(),
            )
        });
        if let Some((sender, Some(envelope))) = result_sender
            && let Ok(row) = self.wait_row(&sender)
            && row.attempt_id.as_ref().map(super::AttemptId::as_str)
                == envelope["attempt_id"].as_str()
            && self.row_settled(&row)
        {
            self.delivered
                .insert((recipient.clone(), sender), DeliveredRow::of(&row));
        }
        if changed {
            self.prune_read_mail(&recipient);
            self.bump();
        }
        Ok(())
    }

    /// Acknowledges the mail an admitted host command carried to
    /// `recipient`. Records, in this process, the result rows whose delivered
    /// mark this acknowledgement made, so [`Self::return_admitted_mail`]
    /// withdraws only those. A row this acknowledgement did not mark drops
    /// any key an earlier admission left, so a later return cannot withdraw a
    /// mark a wait made. The host reconciles only its newest admission
    /// that carried mail, so this retires the returned-admission receipts of
    /// every other admission to `recipient`. Records already pruned are
    /// skipped. The host saves the tree afterwards.
    ///
    /// # Errors
    ///
    /// Refuses mail addressed to someone other than `recipient`.
    pub fn acknowledge_admitted_mail(
        &mut self,
        admission_id: &str,
        recipient: &str,
        message_ids: &[String],
    ) -> Result<(), TopologyError> {
        if message_ids
            .iter()
            .filter_map(|id| self.messages.get(id))
            .any(|mail| mail.to != recipient)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "admitted mail is addressed to another agent",
            ));
        }
        // A later envelope in this same batch must not drop a mark an earlier
        // one just made.
        let mut made = Vec::new();
        for id in message_ids {
            let Some(mail) = self.messages.get(id) else {
                continue;
            };
            let key = (recipient.to_owned(), mail.from.clone());
            let before = self.delivered.get(&key).cloned();
            self.acknowledge_mail(id)?;
            let changed = self
                .delivered
                .get(&key)
                .is_some_and(|after| Some(after) != before.as_ref());
            if changed {
                self.admission_marks.insert(key.clone());
                made.push(key);
            } else if !made.contains(&key) {
                self.admission_marks.remove(&key);
            }
        }
        let before = self.operations.len();
        self.operations.retain(|id, operation| {
            operation.tool_name != super::RETURNED_ADMISSION_KIND
                || operation.caller_agent_id != recipient
                || id == admission_id
        });
        if self.operations.len() != before {
            self.bump();
        }
        Ok(())
    }

    /// Returns the mail an admitted host command acknowledged to unread,
    /// because the command's turn withheld it before the recipient saw
    /// anything (the turn ended with the prompt replaced by a note). The
    /// delivered mark [`Self::acknowledge_admitted_mail`] made for a result
    /// envelope goes with it, so a wait reports the row again; a mark an
    /// earlier wait made stays. A receipt keyed by `admission_id` records the return: it
    /// happens at most once per admission, and
    /// [`Self::admitted_mail_returned`] lets the host's admission
    /// reconciliation leave the mail unread. Records already pruned or no
    /// longer acknowledged are skipped. Returns how many records were
    /// restored. The host saves the tree afterwards.
    ///
    /// # Errors
    ///
    /// Refuses an admission id already used by another operation, or mail
    /// addressed to someone other than `recipient`.
    pub fn return_admitted_mail(
        &mut self,
        admission_id: &str,
        recipient: &str,
        message_ids: &[String],
    ) -> Result<usize, TopologyError> {
        if let Some(prior) = self.operations.get(admission_id) {
            if prior.tool_name == super::RETURNED_ADMISSION_KIND {
                return Ok(0);
            }
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "admission id conflicts with another operation",
            ));
        }
        if message_ids
            .iter()
            .filter_map(|id| self.messages.get(id))
            .any(|mail| mail.to != recipient)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "returned mail is addressed to another agent",
            ));
        }
        let mut restored = 0;
        for id in message_ids {
            let Some(mail) = self.messages.get_mut(id) else {
                continue;
            };
            if mail.durable != MailDurableState::Acknowledged {
                continue;
            }
            mail.durable = MailDurableState::Accepted;
            restored += 1;
            let result_sender = (mail.purpose == MessagePurpose::Result).then(|| {
                (
                    mail.from.clone(),
                    serde_json::from_str::<serde_json::Value>(&mail.body).ok(),
                )
            });
            if let Some((sender, Some(envelope))) = result_sender
                && self
                    .admission_marks
                    .remove(&(recipient.to_owned(), sender.clone()))
                && self.wait_row(&sender).is_ok_and(|row| {
                    row.attempt_id.as_ref().map(super::AttemptId::as_str)
                        == envelope["attempt_id"].as_str()
                })
            {
                self.delivered.remove(&(recipient.to_owned(), sender));
            }
            let ids = self.inbound.entry(recipient.to_owned()).or_default();
            if !ids.contains(id) {
                ids.push(id.clone());
                ids.sort_by(|left, right| {
                    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                });
            }
        }
        let canonical = super::canonical_json(&message_ids)?;
        self.operations.insert(
            admission_id.to_owned(),
            super::OperationRecord {
                fold: None,
                operation_id: admission_id.to_owned(),
                caller_agent_id: recipient.to_owned(),
                tool_name: super::RETURNED_ADMISSION_KIND.to_owned(),
                request_digest: super::digest_text(&canonical),
                terminal_response: Some(canonical.clone()),
                canonical_request: canonical,
                terminal_code: None,
            },
        );
        self.bump();
        Ok(restored)
    }

    /// Whether [`Self::return_admitted_mail`] returned this admission's mail.
    #[must_use]
    pub fn admitted_mail_returned(&self, admission_id: &str) -> bool {
        self.operations
            .get(admission_id)
            .is_some_and(|receipt| receipt.tool_name == super::RETURNED_ADMISSION_KIND)
    }

    fn mint_message_id(&mut self) -> String {
        let id = format!("m{}", self.next_message_sequence);
        self.next_message_sequence += 1;
        id
    }

    fn prune_read_mail(&mut self, recipient: &str) {
        // The current attempt's result envelope is also its durable push and
        // delivery witness. Keep at most one per node, outside read history.
        let witnesses: std::collections::BTreeSet<_> = self
            .nodes
            .values()
            .filter(|node| node.status != NodeStatus::Closed)
            .filter_map(|node| self.current_result_mail(&node.agent_id))
            .map(|mail| mail.message_id.clone())
            .collect();
        let Some(ids) = self.inbound.get_mut(recipient) else {
            return;
        };
        let mut retained = 0;
        ids.retain(|id| self.messages.contains_key(id));
        for id in ids.iter().rev() {
            if self
                .messages
                .get(id)
                .is_some_and(|mail| mail.durable == MailDurableState::Acknowledged)
            {
                retained += 1;
                if retained > 64 && !witnesses.contains(id) {
                    self.messages.remove(id);
                }
            }
        }
        ids.retain(|id| self.messages.contains_key(id));
    }

    /// Retires rollback receipts once the caller's turn committed a closed
    /// exchange: canonical history holds every result they returned, so they
    /// can never roll back. Hosts call this when they publish such a
    /// terminal, before any compaction; a terminal that did not close settles
    /// with [`Self::settle_wait_receipts`] instead.
    pub fn retire_wait_receipts(&mut self, caller_id: &str) -> usize {
        let before = self.operations.len();
        self.operations.retain(|_, operation| {
            operation.caller_agent_id != caller_id
                || operation.tool_name != super::AWAIT_RECEIPT_KIND
        });
        let retired = before - self.operations.len();
        if retired != 0 {
            self.bump();
        }
        retired
    }

    /// The operation ids of `caller_id`'s retained await receipts, for a host
    /// about to look up their canonical commit evidence.
    #[must_use]
    pub fn retained_wait_receipts(&self, caller_id: &str) -> Vec<String> {
        self.operations
            .iter()
            .filter(|(_, receipt)| {
                receipt.tool_name == super::AWAIT_RECEIPT_KIND
                    && receipt.caller_agent_id == caller_id
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Settles retained await receipts from canonical commit evidence, for a
    /// caller whose session has no live turn. `evidence` pairs a receipt's
    /// operation id with whether the caller's committed canonical history
    /// holds that call. A committed receipt is retired and its
    /// acknowledgements stand; an uncommitted one (an abandoned turn, or a
    /// nested wait, which has no history entry of its own) restores its mail
    /// to `Accepted`, forgets the rows it delivered (and any admission mark
    /// for them) and is removed. Settling
    /// is idempotent, so a process loss between canonical abandonment and
    /// saving the tree is settled again by the next open. The host saves the
    /// tree afterwards.
    ///
    /// # Errors
    /// Refuses a malformed durable wait receipt.
    pub fn settle_wait_receipts(
        &mut self,
        caller_id: &str,
        evidence: &[(String, bool)],
    ) -> Result<WaitReceiptSettlement, TopologyError> {
        let mut settled = WaitReceiptSettlement::default();
        let mut restore = Vec::new();
        for (id, committed) in evidence {
            let Some(receipt) = self.operations.get(id).filter(|receipt| {
                receipt.tool_name == super::AWAIT_RECEIPT_KIND
                    && receipt.caller_agent_id == caller_id
            }) else {
                continue;
            };
            if *committed {
                settled.retired += 1;
                restore.push((id.clone(), None));
                continue;
            }
            let Some(response) = receipt.terminal_response.as_ref() else {
                continue;
            };
            let receipt: super::WaitReceipt = serde_json::from_str(response).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "wait receipt could not be reconstructed",
                )
            })?;
            restore.push((id.clone(), Some(receipt)));
        }
        let changed = !restore.is_empty();
        for (id, receipt) in restore {
            self.operations.remove(&id);
            let Some(receipt) = receipt else {
                continue;
            };
            settled.rolled_back += 1;
            for mut mail in receipt.acknowledged {
                mail.durable = MailDurableState::Accepted;
                let ids = self.inbound.entry(mail.to.clone()).or_default();
                if !ids.contains(&mail.message_id) {
                    ids.push(mail.message_id.clone());
                    ids.sort_by(|left, right| {
                        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                    });
                }
                self.messages.insert(mail.message_id.clone(), mail);
                settled.restored_mail += 1;
            }
            for agent in receipt.newly_delivered {
                let key = (caller_id.to_owned(), agent);
                self.delivered.remove(&key);
                self.admission_marks.remove(&key);
            }
        }
        if changed {
            self.bump();
        }
        Ok(settled)
    }

    /// Unread host-authored results, in durable mailbox order. Reading does
    /// not consume them: only an admitted command or an explicit wait does.
    #[must_use]
    pub fn pending_result_mail(&self, recipient: &str) -> Vec<MailRecord> {
        self.inbound
            .get(recipient)
            .into_iter()
            .flatten()
            .filter_map(|id| self.messages.get(id))
            .filter(|mail| {
                mail.purpose == MessagePurpose::Result && mail.durable == MailDurableState::Accepted
            })
            .take(MAX_MAILBOX_RECORDS)
            .cloned()
            .collect()
    }

    /// Unread envelopes in mailbox order, for durable host delivery.
    pub fn pending_mail(&self, recipient: &str) -> Vec<MailRecord> {
        self.unread_mail(recipient).cloned().collect()
    }

    /// The envelopes [`Self::pending_mail`] returns, borrowed, for a reader
    /// that only counts them.
    pub fn unread_mail<'a>(&'a self, recipient: &str) -> impl Iterator<Item = &'a MailRecord> {
        self.inbound
            .get(recipient)
            .into_iter()
            .flatten()
            .filter_map(|id| self.messages.get(id))
            .filter(|mail| mail.durable == MailDurableState::Accepted)
            .take(MAX_MAILBOX_RECORDS)
    }

    pub(super) fn inbox_for(&self, caller_id: &str, include_read: bool) -> Vec<InboxMail> {
        let Some(ids) = self.inbound.get(caller_id) else {
            return Vec::new();
        };
        let mut read = 0;
        let newest: Vec<&MailRecord> = ids
            .iter()
            .rev()
            .filter_map(|id| self.messages.get(id))
            .filter(|mail| {
                if mail.durable == MailDurableState::Accepted {
                    return true;
                }
                read += 1;
                include_read && read <= 64
            })
            .collect();
        newest
            .into_iter()
            .map(|mail| {
                let envelope = (mail.purpose == MessagePurpose::Result)
                    .then(|| serde_json::from_str::<serde_json::Value>(&mail.body).ok())
                    .flatten();
                let field = |name: &str| {
                    envelope
                        .as_ref()
                        .and_then(|value| value.get(name))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                InboxMail {
                    message_id: mail.message_id.clone(),
                    from: mail.from.clone(),
                    purpose: mail.purpose,
                    priority: mail.priority,
                    in_reply_to: mail.in_reply_to.clone(),
                    body_bytes: mail.body.len(),
                    body: bounded_text(
                        &mail.body,
                        if mail.durable == MailDurableState::Accepted {
                            INBOX_BODY_BYTES
                        } else {
                            160
                        },
                    ),
                    attempt_id: field("attempt_id"),
                    status: field("status"),
                    candidate: field("candidate"),
                }
            })
            .collect()
    }

    /// The handle a `wait_agent` poll parks on between ticks.
    pub fn waiters(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use plakous_host_protocol::topology::CandidateId;

    use crate::tree::test_support::*;
    use crate::tree::vcs_fixtures::*;
    use crate::tree::*;

    #[test]
    fn wait_mail_wake_set_and_deferred_delivery() {
        for purpose in [MessagePurpose::Finding, MessagePurpose::Coordinate] {
            let (_tmp, mut tree) = tree();
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            let mut request = mail("root", "deferred finding");
            request.purpose = Some(purpose);
            let sent = tree
                .send_agent_message(&child.agent_id, &request, None)
                .unwrap();
            let named = wait_for(&child.agent_id);
            assert!(tree.poll_wait("root", &named).unwrap().is_none());
            assert!(
                tree.poll_wait("root", &AwaitAgents::Pending)
                    .unwrap()
                    .is_none()
            );
            let timeout = tree.timeout_wait("root", &named).unwrap();
            assert_eq!(timeout.woke_by, WaitWakeReason::Timeout);
            assert_eq!(timeout.mail[0].message_id, sent.message_id);
            assert_eq!(
                tree.mail(&sent.message_id).unwrap().durable,
                MailDurableState::Accepted
            );
            tree.record_turn_completed(&child.agent_id).unwrap();
            tree.settle_for_test();
            let output = tree.poll_wait("root", &named).unwrap().unwrap();
            assert_eq!(output.woke_by, WaitWakeReason::Settled);
            assert_eq!(output.mail[0].message_id, sent.message_id);
        }
    }

    #[test]
    fn urgent_coordinate_and_deferred_reply_wake_waits() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let mut request = mail("root", "urgent coordination");
        request.priority = Some(MessagePriority::Urgent);
        tree.send_agent_message(&child.agent_id, &request, None)
            .unwrap();
        let output = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        assert_eq!(output.woke_by, WaitWakeReason::Mail);
        tree.acknowledge_wait_mail("root", &output);
        let mut question = mail(&child.agent_id, "question for child");
        question.purpose = Some(MessagePurpose::Question);
        let asked = tree.send_agent_message("root", &question, None).unwrap();
        let mut reply = mail("root", "answer");
        reply.purpose = Some(MessagePurpose::Reply);
        reply.in_reply_to = Some(asked.message_id);
        tree.send_agent_message(&child.agent_id, &reply, None)
            .unwrap();
        let output = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        assert_eq!(output.woke_by, WaitWakeReason::Mail);
        assert_eq!(output.mail[0].purpose, MessagePurpose::Reply);
    }

    #[test]
    fn bare_wait_returns_only_deferred_mail_without_parking() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        tree.attempt_mut_for_test(&child.agent_id).status = AttemptStatus::Submitted;
        let output = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        assert!(output.agents[0].output_json.is_some());
        tree.mark_wait_delivered("root", &output);
        tree.attempt_mut_for_test(&child.agent_id).status = AttemptStatus::Completed;
        let output = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        assert!(output.agents[0].output_json.is_none());
        tree.acknowledge_wait_mail("root", &output);
        let sent = tree
            .send_agent_message(&child.agent_id, &mail("root", "later finding"), None)
            .unwrap();
        let output = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        assert_eq!(output.woke_by, WaitWakeReason::Settled);
        assert_eq!(output.mail[0].message_id, sent.message_id);
        assert!(output.agents[0].output_json.is_none());
        let reread = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        assert!(reread.agents[0].output_json.is_some());
    }

    #[test]
    fn delivered_output_class_excludes_failed_and_recovery_required() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.set_node_status_for_test(&child.agent_id, NodeStatus::Completed);
        let mut row = tree.wait_row(&child.agent_id).unwrap();
        let delivered = DeliveredRow::of(&row);
        for status in [
            AttemptStatus::Submitted,
            AttemptStatus::Completed,
            AttemptStatus::Adopted,
        ] {
            row.attempt = status;
            assert!(delivered.matches(&row));
        }
        tree.delivered
            .insert(("root".into(), child.agent_id.clone()), delivered.clone());
        for status in [AttemptStatus::Failed, AttemptStatus::RecoveryRequired] {
            row.attempt = status;
            assert!(!delivered.matches(&row));
            tree.attempt_mut_for_test(&child.agent_id).status = status;
            let output = tree
                .poll_wait("root", &AwaitAgents::Pending)
                .unwrap()
                .unwrap();
            assert_eq!(output.newly_settled, std::slice::from_ref(&child.agent_id));
            assert!(output.agents[0].output_json.is_some());
        }
        row.attempt = AttemptStatus::Completed;
        row.attempt_id = None;
        assert!(!delivered.matches(&row));
    }

    #[test]
    fn omitted_priority_steers_only_a_running_direct_child() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&child.agent_id, true).unwrap();
        let sent = tree
            .send_agent_message("root", &mail(&child.agent_id, "steer"), None)
            .unwrap();
        assert_eq!(sent.disposition, MailDisposition::Steered);
        assert_eq!(
            tree.messages()[&sent.message_id].priority,
            MessagePriority::Urgent
        );
        tree.set_active_turn("root", true).unwrap();
        let sent = tree
            .send_agent_message(&child.agent_id, &mail("root", "report"), None)
            .unwrap();
        assert_eq!(sent.disposition, MailDisposition::Queued);
        assert_eq!(
            tree.messages()[&sent.message_id].priority,
            MessagePriority::Deferred
        );
        let mut request = mail(&child.agent_id, "queue explicitly");
        request.priority = Some(MessagePriority::Deferred);
        assert_eq!(
            tree.send_agent_message("root", &request, None)
                .unwrap()
                .disposition,
            MailDisposition::Queued
        );
        tree.set_active_turn(&child.agent_id, false).unwrap();
        let sent = tree
            .send_agent_message("root", &mail(&child.agent_id, "later"), None)
            .unwrap();
        assert_eq!(
            tree.messages()[&sent.message_id].priority,
            MessagePriority::Deferred
        );
    }

    #[test]
    fn model_mail_accepts_16_kib_but_inbox_previews_stay_2_kib() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let body = "x".repeat(16 * 1024);
        tree.send_agent_message(&child.agent_id, &mail("root", &body), None)
            .unwrap();
        assert_eq!(tree.inbox_for("root", false)[0].body.len(), 2048);
        assert_eq!(
            tree.timeout_wait("root", &wait_for(&child.agent_id))
                .unwrap()
                .mail[0]
                .body,
            body
        );
        assert_eq!(
            tree.send_agent_message(&child.agent_id, &mail("root", &(body + "x")), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::BoundExceeded
        );
    }

    /// A `Sealing` node has not settled. Settling on the
    /// terminal attempt alone handed the caller a row saying "sealing" with no
    /// candidate on it, so there was nothing to adopt.
    #[test]
    fn wait_holds_until_the_seal_finishes_and_the_row_names_the_candidate() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let child_path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(child_path.join("H.txt"), "child work\n").unwrap();

        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();

        // The runtime seals off the tree mutex, so the turn ends with the
        // node `Sealing` and the ticket queued.
        tree.record_turn_completed(&child.agent_id).unwrap();
        assert_eq!(
            tree.node(&child.agent_id).unwrap().status,
            NodeStatus::Sealing
        );
        assert!(
            tree.attempt(&child.agent_id).unwrap().status.is_stopped(),
            "the attempt has already stopped while the copy is still sealing"
        );
        assert!(
            tree.poll_wait("root", &wait_for(&child.agent_id))
                .unwrap()
                .is_none(),
            "a sealing child has not settled"
        );
        assert!(
            !tree
                .messages()
                .values()
                .any(|mail| mail.purpose == MessagePurpose::Result),
            "the result push waits for the candidate too"
        );

        let effects = tree.take_effects();
        assert_eq!(effects.seals.len(), 1);
        tree.complete(effects.run(&SystemVcs));
        let settled = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .expect("leaving Sealing settles the wait");
        let row = &settled.agents[0];
        assert_eq!(row.status, NodeStatus::Completed);
        let candidate = row
            .candidate
            .clone()
            .expect("the settled row names the candidate");
        assert_eq!(candidate, candidate_of(&tree, &child.agent_id));

        // And the pushed result carries it, exactly once.
        let pushed: Vec<&MailRecord> = tree
            .messages()
            .values()
            .filter(|mail| mail.purpose == MessagePurpose::Result)
            .collect();
        assert_eq!(pushed.len(), 1);
        let envelope: serde_json::Value = serde_json::from_str(&pushed[0].body).unwrap();
        assert_eq!(envelope["candidate"], candidate.to_string());
    }

    #[test]
    fn failed_fork_list_and_wait_name_the_adoptable_partial_candidate() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        let path = tree.node(&child.agent_id).unwrap().path.clone();
        std::fs::write(path.join("partial.txt"), "partial work\n").unwrap();
        tree.record_turn_failure(
            &child.agent_id,
            turn_outcome::FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
            "original transient failure".to_owned(),
        )
        .unwrap();
        let effects = tree.take_effects();
        tree.complete(effects.run(&SystemVcs));
        let settled = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        let row = &settled.agents[0];
        assert_eq!(row.attempt, AttemptStatus::Failed);
        let candidate = row.candidate.as_ref().unwrap();
        assert_eq!(candidate, &candidate_of(&tree, &child.agent_id));
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert_eq!(listed.agents[0]["candidate"], candidate.to_string());
        assert_eq!(listed.agents[0]["attempt"], "failed");
        assert!(
            !repo.path().join("partial.txt").exists(),
            "partial work is not adopted implicitly"
        );
    }

    #[test]
    fn mail_to_a_finished_child_is_refused_and_the_wait_settles() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();
        let err = tree
            .send_agent_message("root", &mail(&spawned.agent_id, "late"), None)
            .unwrap_err();
        assert_eq!(err.code, TopologyErrorCode::RecipientAttemptTerminal);
        let wait = tree
            .poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap();
        assert!(wait.is_some(), "wait completes on the terminal attempt");
    }

    /// completion is pushed. A parent that never calls
    /// `wait_agent` still finds the finish in its inbox.
    #[test]
    fn reopened_inbox_preserves_message_sequence_across_decimal_boundaries() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        for index in 1..=12 {
            tree.send_agent_message(
                "root",
                &mail(&child.agent_id, &format!("message {index}")),
                None,
            )
            .unwrap();
        }
        let before = tree.inbound.get(&child.agent_id).unwrap().clone();
        let reopened = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(reopened.inbound.get(&child.agent_id).unwrap(), &before);
    }

    #[test]
    fn result_push_obeys_global_mail_byte_budget_without_losing_attempt_output() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.send_agent_message(
            "root",
            &mail(&child.agent_id, &"x".repeat(MAX_MESSAGE_BYTES)),
            None,
        )
        .unwrap();
        let seed = tree.messages.values().next().unwrap().clone();
        for index in 2..=MAX_QUEUED_MAIL_BYTES / MAX_MESSAGE_BYTES {
            let mut record = seed.clone();
            record.message_id = format!("m{index}");
            tree.messages.insert(record.message_id.clone(), record);
        }
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.to_owned(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.pending_result_mail("root"), [] as [MailRecord; 0]);
        assert_eq!(
            tree.attempt(&child.agent_id)
                .unwrap()
                .output_json
                .as_deref(),
            Some(r#"{"ok":true}"#)
        );
        assert!(
            tree.fault_reasons
                .get(&child.agent_id)
                .unwrap()
                .contains("byte capacity exhausted")
        );
    }

    #[test]
    fn result_mail_is_pushed_to_the_parent_on_completion() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(
            tree.messages().is_empty(),
            "nothing is pushed while the child runs"
        );
        tree.submit_result(
            &spawned.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&spawned.agent_id).unwrap();
        tree.settle_for_test();

        let pushed: Vec<&MailRecord> = tree
            .messages()
            .values()
            .filter(|mail| mail.purpose == MessagePurpose::Result)
            .collect();
        assert_eq!(pushed.len(), 1, "one result message per attempt");
        let mail = pushed[0];
        assert_eq!(mail.to, "root");
        assert_eq!(mail.from, spawned.agent_id);
        // The TUI collapses this envelope into one transcript row, so it is a
        // JSON object with `purpose` and `from`, not prose.
        let envelope: serde_json::Value = serde_json::from_str(&mail.body).unwrap();
        assert_eq!(envelope["purpose"], "result");
        assert_eq!(envelope["from"], spawned.agent_id);
        assert_eq!(envelope["attempt_id"], spawned.attempt_id.as_str());
        assert_eq!(envelope["status"], "completed");
        assert_eq!(envelope["output"], r#"{"ok":true}"#);
        assert_eq!(envelope["output_bytes"], 11);
        assert_eq!(envelope["output_truncated"], false);
        assert_eq!(envelope["full_output_via"], "wait_agent");
        assert!(envelope.get("candidate").is_none(), "nothing sealed yet");
        assert_eq!(mail.disposition, MailDisposition::Queued);
        assert!(
            tree.list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: None,
                    include_completed: Some(true),
                },
            )
            .unwrap()
            .inbox
            .iter()
            .any(|entry| entry.purpose == MessagePurpose::Result),
            "the push lands in the parent inbox projection"
        );

        // A second completion call does not push a second copy.
        let _ = tree.record_turn_completed(&spawned.agent_id);
        tree.settle_for_test();
        assert_eq!(
            tree.messages()
                .values()
                .filter(|mail| mail.purpose == MessagePurpose::Result)
                .count(),
            1
        );
    }

    /// L2: a child that asks its parent a question must not sit
    /// behind the parent's own wait for the full timeout.
    #[test]
    fn urgent_mail_reports_steer_once_without_repeating_the_wake() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn("root", true).unwrap();
        let mut request = mail("root", "urgent question");
        request.purpose = Some(MessagePurpose::Question);
        request.priority = Some(MessagePriority::Urgent);
        let receipt = tree
            .send_agent_message(&child.agent_id, &request, None)
            .unwrap();
        let wait = wait_for(&child.agent_id);
        let output = tree.poll_wait("root", &wait).unwrap().unwrap();
        assert_eq!(output.woke_by, WaitWakeReason::Mail);
        assert_eq!(output.mail[0].message_id, receipt.message_id);
        assert_eq!(output.mail[0].body, request.message);
        tree.acknowledge_wait_mail("root", &output);
        assert!(tree.poll_wait("root", &wait).unwrap().is_none());
    }

    #[test]
    fn child_question_wakes_a_waiting_parent() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(
            tree.poll_wait("root", &wait_for(&spawned.agent_id))
                .unwrap()
                .is_none(),
            "a running child with nothing to say does not settle the wait"
        );
        let mut question = mail("root", "which parser file did you mean?");
        question.purpose = Some(MessagePurpose::Question);
        tree.send_agent_message(&spawned.agent_id, &question, None)
            .unwrap();
        let woken = tree
            .poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap()
            .expect("the question settles the wait");
        assert!(!woken.timed_out);
        assert_eq!(woken.agents.len(), 1);
        assert_eq!(woken.agents[0].status, NodeStatus::Running);
        assert_eq!(woken.woke_by, WaitWakeReason::Mail);
        assert_eq!(woken.mail.len(), 1);
        assert_eq!(woken.mail[0].body, question.message);
        assert_eq!(woken.mail[0].purpose, MessagePurpose::Question);
        assert!(woken.mail[0].in_reply_to.is_none());
        assert!(woken.agents[0].usage.is_none());

        // Once the wait has handed the mail over, it does not settle on it again.
        tree.acknowledge_wait_mail("root", &woken);
        assert!(
            tree.poll_wait("root", &wait_for(&spawned.agent_id))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn wait_acknowledges_only_returned_mail_and_list_defaults_to_unread() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let body = "€".repeat(682) + "ab";
        let first = tree
            .send_agent_message(&child.agent_id, &mail("root", &body), None)
            .unwrap();
        let output = tree
            .timeout_wait("root", &wait_for(&child.agent_id))
            .unwrap();
        assert_eq!(output.mail[0].message_id, first.message_id);
        assert_eq!(output.mail[0].body, body);
        let second = tree
            .send_agent_message(&child.agent_id, &mail("root", "new mail"), None)
            .unwrap();
        tree.acknowledge_wait_mail("root", &output);
        assert_eq!(
            tree.mail(&first.message_id).unwrap().durable,
            MailDurableState::Acknowledged
        );
        assert_eq!(
            tree.mail(&second.message_id).unwrap().durable,
            MailDurableState::Accepted
        );
        let request = ListAgentsRequest {
            include_self: None,
            include_completed: None,
            include_read: None,
        };
        let unread = tree.list_agents("root", &request).unwrap();
        assert_eq!(unread.inbox.len(), 1);
        assert_eq!(unread.inbox[0].message_id, second.message_id);
        let history = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: Some(true),
                    ..request
                },
            )
            .unwrap();
        assert_eq!(history.inbox.len(), 2);
        assert_eq!(history.inbox[0].message_id, second.message_id);
        assert_eq!(history.inbox[1].body, bounded_text(&body, 160));
        assert_eq!(history.inbox[1].body_bytes, body.len());
        let next = tree
            .timeout_wait("root", &wait_for(&child.agent_id))
            .unwrap();
        assert_eq!(next.mail.len(), 1);
        tree.acknowledge_wait_mail("root", &next);
        assert!(
            tree.poll_wait("root", &wait_for(&child.agent_id))
                .unwrap()
                .is_none()
        );
        let timeout = tree
            .timeout_wait("root", &wait_for(&child.agent_id))
            .unwrap();
        assert_eq!(timeout.mail, [] as [InboxMail; 0]);
        assert_eq!(timeout.woke_by, WaitWakeReason::Timeout);
    }

    /// Waiting defaults to 300 seconds: omit the timeout for
    /// 300 s, and a larger ask is clamped to an hour rather than refused.
    #[test]
    fn named_wait_parks_past_delivered_rows_and_admission_marks_results_delivered() {
        let (_tmp, mut tree) = tree();
        let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree.pending_result_mail("root")[0].message_id.clone();
        tree.acknowledge_mail(&result).unwrap();
        let named = AwaitAgents::Named(vec![first.agent_id.clone(), second.agent_id.clone()]);
        assert!(tree.poll_wait("root", &named).unwrap().is_none());
        assert!(
            tree.poll_wait("root", &AwaitAgents::Pending)
                .unwrap()
                .is_none()
        );
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        let output = tree.poll_wait("root", &named).unwrap().unwrap();
        let wire = serde_json::to_value(&output).unwrap();
        assert_eq!(
            wire["mail"][0],
            serde_json::json!({"message_id": output.mail[0].message_id, "purpose": "result", "attempt_id": second.attempt_id.as_str()})
        );
        tree.acknowledge_wait_mail("root", &output);
        tree.mark_wait_delivered("root", &output);
        assert!(
            tree.poll_wait("root", &named).unwrap().is_some(),
            "all delivered is an immediate re-read"
        );
    }

    #[test]
    fn a_returned_admission_restores_its_mail_once_and_a_wait_reads_it_once() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree.pending_result_mail("root")[0].message_id.clone();
        // The admission acknowledged it; its turn then withheld the command.
        let ids = vec![result.clone()];
        let admission = "plak:deliver:item:1";
        tree.acknowledge_admitted_mail(admission, "root", &ids)
            .unwrap();
        assert!(tree.delivered_to("root", &child.agent_id));
        assert!(tree.return_admitted_mail(admission, "c9", &ids).is_err());
        assert_eq!(
            tree.return_admitted_mail(admission, "root", &ids).unwrap(),
            1
        );
        assert!(tree.admitted_mail_returned(admission));
        assert!(!tree.delivered_to("root", &child.agent_id));
        assert_eq!(tree.pending_mail("root").len(), 1);
        // The return is durable and happens once per admission.
        let mut tree = ProductAgentTree::reopen(tree.to_persisted());
        assert!(tree.admitted_mail_returned(admission));
        assert_eq!(
            tree.return_admitted_mail(admission, "root", &ids).unwrap(),
            0
        );
        assert_eq!(tree.pending_mail("root").len(), 1);
        // The model's own wait reads it, once.
        let output = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        assert_eq!(output.mail.len(), 1);
        assert_eq!(output.mail[0].message_id, result);
        tree.acknowledge_wait_mail("root", &output);
        tree.mark_wait_delivered("root", &output);
        assert_eq!(tree.pending_mail("root"), [] as [MailRecord; 0]);
        assert_eq!(
            tree.return_admitted_mail(admission, "root", &ids).unwrap(),
            0
        );
        assert_eq!(tree.pending_mail("root"), [] as [MailRecord; 0]);
        assert!(tree.delivered_to("root", &child.agent_id));
    }

    /// A named wait that delivered the row but whose budget withheld the
    /// result envelope made the delivered mark; the admission that then
    /// carried the envelope did not, so returning the envelope keeps it.
    #[test]
    fn a_returned_admission_keeps_a_delivered_mark_an_earlier_wait_made() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree.pending_result_mail("root")[0].message_id.clone();
        let output = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        // The budget withheld the envelope: it stays unread.
        tree.mark_wait_delivered("root", &output);
        assert!(tree.delivered_to("root", &child.agent_id));
        let ids = vec![result];
        let admission = "plak:deliver:item:1";
        tree.acknowledge_admitted_mail(admission, "root", &ids)
            .unwrap();
        assert_eq!(
            tree.return_admitted_mail(admission, "root", &ids).unwrap(),
            1
        );
        assert_eq!(tree.pending_mail("root").len(), 1);
        assert!(
            tree.delivered_to("root", &child.agent_id),
            "the wait's own delivery stands"
        );
    }

    /// An admission that completed (its acknowledgement made the mark, and
    /// the turn was not withheld) must not leave that process-local key
    /// behind. A later admission of the same child's next result, whose
    /// acknowledgement made no mark because a named wait already did, then
    /// returns the envelope without withdrawing the wait's mark.
    #[test]
    fn a_completed_admission_does_not_withdraw_a_later_waits_mark() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let first = vec![tree.pending_result_mail("root")[0].message_id.clone()];
        tree.acknowledge_admitted_mail("plak:deliver:a:1", "root", &first)
            .unwrap();
        assert!(tree.delivered_to("root", &child.agent_id));
        let mut request = mail(&child.agent_id, "also cover the error paths");
        request.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &request, None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let pending = tree.pending_result_mail("root");
        assert_eq!(pending.len(), 1);
        let second = vec![pending[0].message_id.clone()];
        assert_ne!(first, second);
        let output = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        tree.mark_wait_delivered("root", &output);
        assert!(tree.delivered_to("root", &child.agent_id));
        tree.acknowledge_admitted_mail("plak:deliver:b:1", "root", &second)
            .unwrap();
        assert_eq!(
            tree.return_admitted_mail("plak:deliver:b:1", "root", &second)
                .unwrap(),
            1
        );
        assert!(
            tree.delivered_to("root", &child.agent_id),
            "the wait's own delivery stands"
        );
    }

    /// The delivered row moved without this admission making the mark (a
    /// wait acknowledged the result). Acknowledging the admission then drops
    /// the key the previous admission left, so the return keeps the row.
    #[test]
    fn an_acknowledgement_that_makes_no_mark_drops_the_stale_key() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let first = vec![tree.pending_result_mail("root")[0].message_id.clone()];
        tree.acknowledge_admitted_mail("plak:deliver:a:1", "root", &first)
            .unwrap();
        let mut request = mail(&child.agent_id, "also cover the error paths");
        request.purpose = Some(MessagePurpose::Followup);
        tree.send_agent_message("root", &request, None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let second = tree.pending_result_mail("root")[0].message_id.clone();
        tree.acknowledge_mail(&second).unwrap();
        assert!(tree.delivered_to("root", &child.agent_id));
        let ids = vec![second];
        tree.acknowledge_admitted_mail("plak:deliver:b:1", "root", &ids)
            .unwrap();
        assert_eq!(
            tree.return_admitted_mail("plak:deliver:b:1", "root", &ids)
                .unwrap(),
            1
        );
        assert!(
            tree.delivered_to("root", &child.agent_id),
            "a stale admission key withdrew the later row"
        );
    }

    /// A wait that delivers a row an admission had marked owns that mark.
    /// Returning the admission afterwards keeps the wait's delivery.
    #[test]
    fn a_later_wait_keeps_the_mark_an_admission_made() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let ids = vec![tree.pending_result_mail("root")[0].message_id.clone()];
        tree.acknowledge_admitted_mail("plak:deliver:a:1", "root", &ids)
            .unwrap();
        let output = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .unwrap();
        tree.mark_wait_delivered("root", &output);
        assert_eq!(
            tree.return_admitted_mail("plak:deliver:a:1", "root", &ids)
                .unwrap(),
            1
        );
        assert!(
            tree.delivered_to("root", &child.agent_id),
            "the wait overwrote the admission's mark"
        );
    }

    /// Rolling back a wait forgets the rows it delivered and the admission
    /// key for them, so a later delivery of the same row survives a return
    /// of the old admission.
    #[test]
    fn a_rolled_back_wait_drops_the_admission_mark() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let ids = vec![tree.pending_result_mail("root")[0].message_id.clone()];
        tree.acknowledge_admitted_mail("plak:deliver:a:1", "root", &ids)
            .unwrap();
        tree.remember_success(
            "abandoned-wait",
            "root",
            AWAIT_RECEIPT_KIND,
            &serde_json::json!({}),
            &WaitReceipt {
                output: serde_json::json!({}),
                acknowledged: Vec::new(),
                newly_delivered: vec![child.agent_id.clone()],
            },
        )
        .unwrap();
        let settled = tree
            .settle_wait_receipts("root", &[("abandoned-wait".into(), false)])
            .unwrap();
        assert_eq!(settled.rolled_back, 1);
        // The result mail is still acknowledged, so delivery still reads as
        // done until a later mark is the only witness. Re-mark the row the
        // way a later acknowledgement would, without recording a new key.
        tree.acknowledge_mail(&ids[0]).unwrap();
        assert!(tree.delivered_to("root", &child.agent_id));
        assert_eq!(
            tree.return_admitted_mail("plak:deliver:a:1", "root", &ids)
                .unwrap(),
            1
        );
        assert!(
            tree.delivered_to("root", &child.agent_id),
            "the rollback dropped the admission's claim on this row"
        );
    }

    /// Acknowledging the next admission's batch retires the receipts of
    /// earlier returned admissions, so the operation ledger stays bounded.
    #[test]
    fn the_next_admission_retires_earlier_returned_admission_receipts() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let ids = vec![tree.pending_result_mail("root")[0].message_id.clone()];
        let operations = tree.operations.len();
        tree.acknowledge_admitted_mail("plak:deliver:a:1", "root", &ids)
            .unwrap();
        tree.return_admitted_mail("plak:deliver:a:1", "root", &ids)
            .unwrap();
        tree.acknowledge_admitted_mail("plak:deliver:b:1", "root", &ids)
            .unwrap();
        tree.return_admitted_mail("plak:deliver:b:1", "root", &ids)
            .unwrap();
        assert!(!tree.admitted_mail_returned("plak:deliver:a:1"));
        assert!(tree.admitted_mail_returned("plak:deliver:b:1"));
        tree.acknowledge_admitted_mail("plak:deliver:c:1", "root", &ids)
            .unwrap();
        assert!(!tree.admitted_mail_returned("plak:deliver:b:1"));
        assert_eq!(tree.operations.len(), operations);
        assert_eq!(tree.pending_mail("root"), [] as [MailRecord; 0]);
        // Mail addressed elsewhere is refused.
        assert!(
            tree.acknowledge_admitted_mail("plak:deliver:d:1", "c9", &ids)
                .is_err()
        );
    }

    #[test]
    fn wait_mail_is_in_arrival_order_and_utf8_over_bound_is_refused() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let exact = "🦀".repeat(MAX_MESSAGE_BYTES / 4);
        let first = tree
            .send_agent_message(&child.agent_id, &mail("root", &exact), None)
            .unwrap();
        let second = tree
            .send_agent_message(&child.agent_id, &mail("root", "later"), None)
            .unwrap();
        assert_eq!(
            tree.send_agent_message(&child.agent_id, &mail("root", &(exact.clone() + "a")), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::BoundExceeded
        );
        let output = tree
            .timeout_wait("root", &wait_for(&child.agent_id))
            .unwrap();
        assert_eq!(output.mail[0].message_id, first.message_id);
        assert_eq!(output.mail[0].body.as_bytes(), exact.as_bytes());
        assert_eq!(output.mail[1].message_id, second.message_id);
    }

    #[test]
    fn mailbox_budgets_are_unread_and_history_is_bounded_with_monotonic_ids() {
        let (_tmp, mut tree) = tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        for index in 1..=1100 {
            let message = tree
                .send_agent_message(
                    &child.agent_id,
                    &mail("root", &"x".repeat(MAX_MESSAGE_BYTES)),
                    None,
                )
                .unwrap();
            assert_eq!(message.message_id, format!("m{index}"));
            tree.acknowledge_mail(&message.message_id).unwrap();
        }
        assert_eq!(tree.messages.len(), 64);
        assert_eq!(tree.inbound["root"].len(), 64);
        assert_eq!(tree.inbox_for("root", false), [] as [InboxMail; 0]);
        assert_eq!(tree.inbox_for("root", true).len(), 64);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        reopened.state = TreeAdmissionState::Live;
        let next = reopened
            .send_agent_message(&child.agent_id, &mail("root", "new"), None)
            .unwrap();
        assert_eq!(next.message_id, "m1101");
    }

    #[test]
    fn result_push_remains_once_per_attempt_after_acknowledged_history_is_pruned() {
        let (_tmp, mut tree) = tree();
        let finished = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let sender = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&finished.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree.pending_result_mail("root")[0].message_id.clone();
        tree.acknowledge_mail(&result).unwrap();
        for _ in 0..70 {
            let receipt = tree
                .send_agent_message(&sender.agent_id, &mail("root", "finding"), None)
                .unwrap();
            tree.acknowledge_mail(&receipt.message_id).unwrap();
        }
        assert!(
            tree.mail(&result).is_some(),
            "current result is a durable witness"
        );
        tree.record_turn_completed(&finished.agent_id).unwrap();
        tree.settle_for_test();
        assert_eq!(tree.pending_result_mail("root"), [] as [MailRecord; 0]);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        reopened.record_turn_completed(&finished.agent_id).unwrap();
        reopened.settle_for_test();
        assert_eq!(reopened.pending_result_mail("root"), [] as [MailRecord; 0]);
    }

    #[test]
    fn followup_cannot_reactivate_a_slot_when_direct_children_are_full() {
        let (_tmp, mut tree) = tree();
        let finished = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&finished.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree.pending_result_mail("root")[0].message_id.clone();
        tree.acknowledge_mail(&result).unwrap();
        let running: Vec<_> = (0..DEFAULT_DIRECT_CHILDREN)
            .map(|_| {
                tree.spawn_agent("root", &spawn_req(), None)
                    .unwrap()
                    .agent_id
            })
            .collect();
        let mut request = mail(&finished.agent_id, "another task");
        request.purpose = Some(MessagePurpose::Followup);
        assert_eq!(
            tree.send_agent_message("root", &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SpawnBudget
        );
        assert_eq!(tree.attempt(&finished.agent_id).unwrap().turn_token, 1);
        tree.close_agent("root", &close_req(&running[0]), None)
            .unwrap();
        tree.settle_for_test();
        tree.send_agent_message("root", &request, None).unwrap();
        assert_eq!(tree.attempt(&finished.agent_id).unwrap().turn_token, 2);
    }

    #[test]
    fn escaped_result_excerpt_always_fits_the_mail_envelope() {
        let (_tmp, mut tree) = tree();
        let mut request = spawn_req();
        request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"ok":{"type":"boolean"},"text":{"type":"string"}},"required":["ok","text"],"additionalProperties":false}"#.into();
        let child = tree.spawn_agent("root", &request, None).unwrap();
        let output = serde_json::json!({"ok": true, "text": "\"\\".repeat(2000)}).to_string();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: output.clone(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let mail = tree.pending_result_mail("root");
        assert_eq!(mail.len(), 1);
        assert!(mail[0].body.len() <= 2048);
        let envelope: serde_json::Value = serde_json::from_str(&mail[0].body).unwrap();
        assert!(output.starts_with(envelope["output"].as_str().unwrap()));
        assert_eq!(envelope["output_bytes"], output.len());
        assert_eq!(envelope["output_truncated"], true);
        assert_eq!(envelope["full_output_via"], "wait_agent");
        assert_eq!(
            tree.attempt(&child.agent_id).unwrap().output_json.as_ref(),
            Some(&output)
        );
    }

    #[test]
    fn dormant_children_release_tree_slots_and_followup_rechecks_tree_cap() {
        let (_tmp, mut tree) = tree();
        tree.grants.get_mut("root").unwrap().max_nodes = 2;
        let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
        tree.mark_wait_delivered("root", &result);
        let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let mut followup = mail(&first.agent_id, "another task");
        followup.purpose = Some(MessagePurpose::Followup);
        assert_eq!(
            tree.send_agent_message("root", &followup, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SpawnBudget,
        );
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        let result = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &result);
        tree.mark_wait_delivered("root", &result);
        tree.send_agent_message("root", &followup, None).unwrap();
    }

    #[test]
    fn delivered_finished_children_release_direct_slots_without_losing_candidates() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let first = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&first.agent_id).unwrap().path.join("work.txt"),
            "candidate\n",
        )
        .unwrap();
        tree.record_turn_completed(&first.agent_id).unwrap();
        tree.settle_for_test();
        let candidate = candidate_of(&tree, &first.agent_id);
        for _ in 1..DEFAULT_DIRECT_CHILDREN {
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
            tree.settle_for_test();
        }
        assert_eq!(
            tree.spawn_agent("root", &spawn_req(), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::SpawnBudget
        );
        let output = tree
            .poll_wait("root", &AwaitAgents::Pending)
            .unwrap()
            .unwrap();
        tree.acknowledge_wait_mail("root", &output);
        tree.mark_wait_delivered("root", &output);
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.close_agent("root", &close_req(&first.agent_id), None)
            .unwrap();
        assert!(tree.candidates.contains_key(&candidate));
        tree.adopt_candidate("root", &adopt_req(&candidate), None)
            .unwrap();
    }

    #[test]
    fn acknowledged_results_release_slots_after_reopen_and_history_pruning() {
        let (_tmp, mut tree) = tree();
        let mut finished = Vec::new();
        for _ in 0..DEFAULT_DIRECT_CHILDREN {
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
            finished.push(child.agent_id);
        }
        tree.settle_for_test();
        for result in tree.pending_result_mail("root") {
            tree.acknowledge_mail(&result.message_id).unwrap();
        }
        let sender = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        for _ in 0..70 {
            let sent = tree
                .send_agent_message(&sender.agent_id, &mail("root", "later"), None)
                .unwrap();
            tree.acknowledge_mail(&sent.message_id).unwrap();
        }
        assert_eq!(tree.inbox_for("root", true).len(), 64);
        assert!(tree.messages.len() <= 64 + DEFAULT_DIRECT_CHILDREN);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        reopened.state = TreeAdmissionState::Live;
        for id in finished {
            assert!(reopened.delivered_to("root", &id));
        }
        reopened.spawn_agent("root", &spawn_req(), None).unwrap();
    }

    #[test]
    fn wait_timeout_clamps_to_one_hour() {
        let default = WaitAgentRequest {
            agent_ids: None,
            timeout_ms: None,
        };
        assert_eq!(default.validate().unwrap(), DEFAULT_WAIT_TIMEOUT_MS);
        let over = WaitAgentRequest {
            agent_ids: None,
            timeout_ms: Some(MAX_WAIT_TIMEOUT_MS * 4),
        };
        assert_eq!(over.validate().unwrap(), MAX_WAIT_TIMEOUT_MS);
        assert_eq!(MAX_WAIT_TIMEOUT_MS, 3_600_000);
    }

    #[test]
    fn sibling_cannot_mail_sibling() {
        let (_tmp, mut tree) = tree();
        let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert_eq!(
            tree.send_agent_message(&first.agent_id, &mail(&second.agent_id, "peer"), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::GrantDenied
        );
        tree.send_agent_message("root", &mail(&first.agent_id, "start"), None)
            .unwrap();
    }

    #[test]
    fn list_agents_inbox_exposes_message_id_for_reply() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let mut question = mail("root", "need-reply");
        question.purpose = Some(MessagePurpose::Question);
        let asked = tree
            .send_agent_message(&spawned.agent_id, &question, None)
            .unwrap();
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(true),
                    include_completed: Some(false),
                },
            )
            .unwrap();
        assert_eq!(listed.inbox.len(), 1);
        assert_eq!(listed.inbox[0].message_id, asked.message_id);
        assert_eq!(listed.inbox[0].from, spawned.agent_id);
        assert_eq!(listed.inbox[0].purpose, MessagePurpose::Question);
        let mut reply = mail(&spawned.agent_id, "answer");
        reply.purpose = Some(MessagePurpose::Reply);
        reply.in_reply_to = Some(asked.message_id);
        let replied = tree.send_agent_message("root", &reply, None).unwrap();
        assert_ne!(replied.message_id, "");
    }

    #[test]
    fn reply_without_in_reply_to_is_invalid() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let mut reply = mail(&spawned.agent_id, "answer");
        reply.purpose = Some(MessagePurpose::Reply);
        assert_eq!(
            tree.send_agent_message("root", &reply, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::ReplyInvalid
        );
    }

    #[test]
    fn message_byte_budget_and_urgent_steer_preserves_token() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        let too_big = "x".repeat(MAX_MESSAGE_BYTES + 1);
        assert_eq!(
            tree.send_agent_message("root", &mail(&spawned.agent_id, &too_big), None)
                .unwrap_err()
                .code,
            TopologyErrorCode::BoundExceeded
        );
        let mut urgent = mail(&spawned.agent_id, "now");
        urgent.priority = Some(MessagePriority::Urgent);
        let steered = tree.send_agent_message("root", &urgent, None).unwrap();
        assert_eq!(steered.disposition, MailDisposition::Steered);
        assert_eq!(tree.attempt(&spawned.agent_id).unwrap().turn_token, 1);
    }

    #[test]
    fn interrupt_settles_the_wait_without_terminalizing_and_mail_stays_legal() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        tree.interrupt_agent("root", &close_req(&spawned.agent_id), None)
            .unwrap();
        // The wait now returns with the
        // child's interrupted status so the root continues, instead of parking
        // the root behind a poll loop for the whole timeout.
        assert_eq!(
            tree.node(&spawned.agent_id).unwrap().status,
            NodeStatus::Interrupted
        );
        let output = tree
            .poll_wait("root", &wait_for(&spawned.agent_id))
            .unwrap()
            .expect("an interrupted child settles the wait");
        assert!(!output.timed_out);
        assert_eq!(output.agents[0].status, NodeStatus::Interrupted);
        tree.send_agent_message("root", &mail(&spawned.agent_id, "continue"), None)
            .unwrap();
        assert!(!NodeStatus::Interrupted.is_terminal());
        assert!(NodeStatus::Interrupted.settles_a_wait());
    }

    /// A root interrupt interrupts the children that are running, leaves a
    /// finished one alone, and mail still resumes an interrupted child.
    #[test]
    fn a_root_interrupt_interrupts_running_children_only() {
        let (_tmp, mut tree) = tree();
        let running = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let other = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let finished = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&running.agent_id, true).unwrap();
        tree.set_active_turn(&other.agent_id, true).unwrap();
        tree.record_turn_completed(&finished.agent_id).unwrap();
        tree.settle_for_test();
        let interrupted = tree.interrupt_running_descendants().unwrap();
        assert_eq!(
            interrupted,
            [running.agent_id.clone(), other.agent_id.clone()]
        );
        for id in &interrupted {
            assert_eq!(tree.node(id).unwrap().status, NodeStatus::Interrupted);
        }
        assert_ne!(
            tree.node(&finished.agent_id).unwrap().status,
            NodeStatus::Interrupted
        );
        tree.send_agent_message("root", &mail(&running.agent_id, "continue"), None)
            .unwrap();
    }

    /// Root `Esc` must reach a root that is inside
    /// `wait_agent`. An interrupt on the waiting node itself abandons the wait
    /// rather than leaving the root parked until the timeout expires.
    #[test]
    fn interrupting_the_waiting_agent_abandons_its_own_wait() {
        let (_tmp, mut tree) = tree();
        let spawned = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.set_active_turn(&spawned.agent_id, true).unwrap();
        tree.set_active_turn("root", true).unwrap();
        let request = wait_for(&spawned.agent_id);
        // The wait is open: the child is still running.
        assert!(tree.poll_wait("root", &request).unwrap().is_none());

        tree.nodes.get_mut("root").unwrap().status = NodeStatus::Interrupted;
        let refusal = tree.poll_wait("root", &request).unwrap_err();
        assert_eq!(refusal.code, TopologyErrorCode::WaitCallerSettled);
        assert!(refusal.message.contains("own turn was interrupted"));
    }

    /// omitted `agent_ids` watches every managed child.
    #[test]
    fn wait_without_agent_ids_watches_every_managed_child() {
        let (_tmp, mut tree) = tree();
        let all = AwaitAgents::Pending;
        assert_eq!(
            tree.poll_wait("root", &all).unwrap_err().code,
            TopologyErrorCode::SchemaMismatch,
            "nothing to watch before the first spawn"
        );
        let first = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let second = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        assert!(tree.poll_wait("root", &all).unwrap().is_none());
        tree.record_turn_completed(&second.agent_id).unwrap();
        tree.settle_for_test();
        let output = tree.poll_wait("root", &all).unwrap().unwrap();
        let ids: Vec<&str> = output
            .agents
            .iter()
            .map(|row| row.agent_id.as_str())
            .collect();
        assert_eq!(ids, [first.agent_id.as_str(), second.agent_id.as_str()]);
        let timed_out = tree.timeout_wait("root", &all).unwrap();
        assert_eq!(timed_out.agents.len(), 2);
        // A child watching its own parent is still refused.
        let self_wait = AwaitAgents::Named(vec!["root".into()]);
        assert_eq!(
            tree.poll_wait("root", &self_wait).unwrap_err().code,
            TopologyErrorCode::GrantDenied
        );
    }

    /// A bare wait returns each settled child once, in the order they
    /// settle, and refuses once nothing is left to report. Naming the child
    /// still re-reads its row.
    #[test]
    fn bare_waits_deliver_each_settled_child_once() {
        let (_tmp, mut tree) = tree();
        let all = AwaitAgents::Pending;
        let children: Vec<String> = (0..3)
            .map(|_| {
                tree.spawn_agent("root", &spawn_req(), None)
                    .unwrap()
                    .agent_id
            })
            .collect();
        let mut delivered = Vec::new();
        for id in [&children[2], &children[0], &children[1]] {
            assert!(tree.poll_wait("root", &all).unwrap().is_none());
            tree.record_turn_completed(id).unwrap();
            tree.settle_for_test();
            let output = tree.poll_wait("root", &all).unwrap().unwrap();
            let settled: Vec<&str> = output
                .agents
                .iter()
                .filter(|row| row.attempt.is_stopped())
                .map(|row| row.agent_id.as_str())
                .collect();
            assert_eq!(settled, [id.as_str()], "only the newly settled child");
            tree.acknowledge_wait_mail("root", &output);
            tree.mark_wait_delivered("root", &output);
            delivered.push(id.clone());
        }
        assert_eq!(delivered.len(), 3);
        assert_eq!(
            tree.poll_wait("root", &all).unwrap_err().code,
            TopologyErrorCode::NoPendingChildren
        );
        let named = AwaitAgents::Named(vec![children[0].clone()]);
        assert!(tree.poll_wait("root", &named).unwrap().is_some());
    }

    /// a `result` the parent's inbox cannot hold is not dropped
    /// silently — the wait row still carries it and the node says why the push
    /// was lost.
    #[test]
    fn a_result_push_into_a_full_inbox_records_a_fault_reason() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        for _ in 0..MAX_MAILBOX_RECORDS {
            tree.send_agent_message(&child.agent_id, &mail("root", "queued"), None)
                .unwrap();
        }
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let reason = tree
            .fault_reason(&child.agent_id)
            .unwrap_or_else(|| unreachable!("a lost push is recorded"));
        assert!(
            reason.contains("was not pushed") && reason.contains("root"),
            "{reason}"
        );
        assert!(
            !tree
                .messages()
                .values()
                .any(|record| record.purpose == MessagePurpose::Result),
            "the push itself did not happen"
        );
    }

    /// `list_agents` exposes the pushed `result` on the
    /// caller's `inbox[]` with the envelope fields, and the wait row carries
    /// the candidate.
    #[test]
    fn list_agents_inbox_carries_the_result_envelope() {
        let repo = git_repo();
        let mut tree = tree_at(repo.path().to_path_buf(), WriterMode::SingleLease);
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        std::fs::write(
            tree.node(&child.agent_id).unwrap().path.join("H.txt"),
            "child\n",
        )
        .unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let candidate_id = candidate_of(&tree, &child.agent_id);
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(false),
                    include_completed: Some(true),
                },
            )
            .unwrap();
        assert_eq!(listed.inbox.len(), 1, "{:?}", listed.inbox);
        let mail = &listed.inbox[0];
        assert_eq!(mail.purpose, MessagePurpose::Result);
        assert_eq!(mail.from, child.agent_id);
        assert_eq!(mail.attempt_id.as_deref(), Some(child.attempt_id.as_str()));
        assert_eq!(mail.status.as_deref(), Some("completed"));
        assert_eq!(mail.candidate.as_deref(), Some(candidate_id.as_str()));
        assert!(
            mail.body.contains("\"purpose\":\"result\""),
            "{}",
            mail.body
        );
        let json = serde_json::to_value(&listed).unwrap();
        assert_eq!(json["inbox"][0]["candidate"], candidate_id.to_string());
        assert_eq!(json["inbox"][0]["attempt_id"], child.attempt_id.as_str());
        let waited = tree
            .poll_wait("root", &wait_for(&child.agent_id))
            .unwrap()
            .expect("a completed child settles the wait");
        assert_eq!(
            waited.agents[0].candidate.as_ref().map(CandidateId::as_str),
            Some(candidate_id.as_str())
        );
    }
}
