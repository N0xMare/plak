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
    #[cfg(any(test, feature = "test-support"))]
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
    #[allow(
        clippy::too_many_lines,
        reason = "mail admission distinguishes steer, queue and followup while recording one immutable replay receipt"
    )]
    pub fn send_agent_message(
        &mut self,
        caller_id: &str,
        request: &SendAgentMessageRequest,
        operation_id: Option<&str>,
    ) -> Result<SendMessageOutput, TopologyError> {
        let _ = self.parent_followup_recovery(caller_id, request, operation_id)?;
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
                request.acknowledge_recovery.as_ref(),
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
        let refresh = (purpose == MessagePurpose::Followup)
            .then(|| {
                self.nodes
                    .get(&request.agent_id)
                    .and_then(|node| self.refresh_report(&node.workspace_id, operation_id))
            })
            .flatten();
        let body = refresh.as_ref().map_or_else(
            || request.message.clone(),
            |report| format!("{}\n\n{}", request.message, report.start_line()),
        );
        self.check_mailbox_budget(&request.agent_id, body.len())?;
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
        // A bound turn survives apply_reopen_law (active_turn is cleared). Restart
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
            body,
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
            recovery_warning: request
                .acknowledge_recovery
                .as_ref()
                .map(|_| super::text::PARENT_RECOVERY_WARNING.to_owned()),
            refresh,
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

    #[allow(
        clippy::too_many_lines,
        reason = "result mail retains exact attempt output, candidate provenance and failure evidence in its bounded notice"
    )]
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
        if self.seal_history_notice(agent_id, &attempt_id) {
            envelope["notice"] = serde_json::Value::String(
                "history left plak_base(); the candidate is the whole tree".to_owned(),
            );
        }
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

    pub(super) fn check_mailbox_budget(
        &self,
        recipient: &str,
        body_bytes: usize,
    ) -> Result<(), TopologyError> {
        if self.inbound.get(recipient).is_some_and(|inbound| {
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
        if queued_bytes.saturating_add(body_bytes) > MAX_QUEUED_MAIL_BYTES {
            return Err(TopologyError::new(
                TopologyErrorCode::MailboxFull,
                "tree queued-mail byte budget is exhausted",
            ));
        }
        Ok(())
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
mod tests;
