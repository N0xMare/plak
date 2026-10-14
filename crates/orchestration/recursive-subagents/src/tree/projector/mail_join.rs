use super::{CommandFact, ProductAgentTree, TopologyError, TopologyErrorCode};

pub(crate) const CONSUMED_COMMAND_KIND: &str = "consumed_command";
pub(crate) const FAILED_WAKE_MAIL_KIND: &str = "failed_wake_mail";
pub(crate) const UNDELIVERABLE_MAIL_KIND: &str = "undeliverable_mail";

#[derive(serde::Serialize, serde::Deserialize)]
struct FailedWakeMail {
    attempt_id: Option<crate::protocol::AttemptId>,
    message_ids: Vec<String>,
    notice: String,
}

impl ProductAgentTree {
    /// Records a terminal/cancelled send's undeliverable envelope. This is
    /// the host's decision, not an invented runtime admission.
    ///
    /// # Errors
    /// Refuses a missing envelope or an invalid intent encoding.
    pub fn record_undeliverable_mail(&mut self, message_id: &str) -> Result<(), TopologyError> {
        let mail = self.mail(message_id).ok_or_else(conflict)?;
        let recipient = mail.to.clone();
        let request = super::super::canonical_json(&FailedWakeMail {
            attempt_id: None,
            message_ids: vec![message_id.to_owned()],
            notice: String::new(),
        })?;
        self.record_mail_decision(
            format!("plak:undeliverable:{message_id}"),
            recipient,
            UNDELIVERABLE_MAIL_KIND,
            request,
        );
        Ok(())
    }

    /// Records exact-resubmission consumption below retention. No turn,
    /// binding, usage or runtime fact is invented by this host decision.
    ///
    /// # Errors
    /// Refuses a missing, rejected or withdrawn command.
    pub fn record_consumed_command(&mut self, command_id: &str) -> Result<(), TopologyError> {
        let command = self.command(command_id).ok_or_else(conflict)?;
        if command.withdrawn || command.fact == Some(CommandFact::Rejected) {
            return Err(conflict());
        }
        let recipient = command.recipient.clone();
        let request = super::super::canonical_json(&command_id)?;
        self.record_mail_decision(
            format!("plak:consumed:{command_id}"),
            recipient,
            CONSUMED_COMMAND_KIND,
            request,
        );
        Ok(())
    }

    /// Records the exact non-result mail a failed wake can no longer deliver.
    /// The first decision for this attempt and turn stands across retries.
    ///
    /// # Errors
    /// Refuses a missing attempt or an invalid intent encoding.
    pub fn record_failed_wake_mail(
        &mut self,
        agent_id: &str,
        turn_id: &str,
        error: &str,
    ) -> Result<(), TopologyError> {
        let attempt = self.current_attempt(agent_id).ok_or_else(conflict)?;
        let id = format!(
            "plak:failed-wake-mail:{agent_id}:{}:{turn_id}",
            attempt.attempt_id
        );
        if self.operations.contains_key(&id) {
            return Ok(());
        }
        let message_ids: Vec<_> = self
            .wake_mail(agent_id)
            .into_iter()
            .filter(|mail| mail.purpose != crate::protocol::MessagePurpose::Result)
            .map(|mail| mail.message_id)
            .collect();
        if message_ids.is_empty() {
            return Ok(());
        }
        let notice = format!(
            "mail {} was not delivered: the wake that would carry it was not admitted ({error}) and the attempt ended",
            message_ids.join(", ")
        );
        let request = super::super::canonical_json(&FailedWakeMail {
            attempt_id: Some(attempt.attempt_id.clone()),
            message_ids,
            notice,
        })?;
        self.record_mail_decision(id, agent_id.to_owned(), FAILED_WAKE_MAIL_KIND, request);
        Ok(())
    }

    fn record_mail_decision(&mut self, id: String, recipient: String, kind: &str, request: String) {
        if self.operations.contains_key(&id) {
            return;
        }
        self.operations.insert(
            id.clone(),
            super::super::OperationRecord {
                fold: None,
                operation_id: id,
                caller_agent_id: recipient,
                tool_name: kind.to_owned(),
                request_digest: super::super::digest_text(&request),
                canonical_request: request,
                terminal_code: None,
                terminal_response: None,
            },
        );
        self.bump();
    }

    /// Joins host mail decisions through the same projector used by feed passes.
    ///
    /// # Errors
    /// Refuses malformed intents or a conflicting command outcome.
    pub fn project_mail_decisions(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        let mut next = self.clone();
        next.join_mail_decisions(agent_id)?;
        *self = next;
        Ok(())
    }

    pub(super) fn join_mail_decisions(&mut self, agent_id: &str) -> Result<(), TopologyError> {
        let intents: Vec<_> = self
            .operations
            .values()
            .filter(|op| {
                op.caller_agent_id == agent_id
                    && matches!(
                        op.tool_name.as_str(),
                        CONSUMED_COMMAND_KIND | FAILED_WAKE_MAIL_KIND | UNDELIVERABLE_MAIL_KIND
                    )
            })
            .cloned()
            .collect();
        for intent in intents {
            if intent.tool_name == CONSUMED_COMMAND_KIND {
                let command: String =
                    serde_json::from_str(&intent.canonical_request).map_err(|_| conflict())?;
                if self
                    .command(&command)
                    .is_none_or(|command| command.fact.is_none())
                {
                    self.apply_accepted(
                        agent_id,
                        &command,
                        None,
                        &mut super::TreeProjection::default(),
                    )?;
                }
            } else {
                let decision: FailedWakeMail =
                    serde_json::from_str(&intent.canonical_request).map_err(|_| conflict())?;
                for id in decision.message_ids {
                    if self.mail(&id).is_some() {
                        self.acknowledge_mail(&id)?;
                    }
                }
                if !decision.notice.is_empty()
                    && self.current_attempt(agent_id).is_some_and(|attempt| {
                        Some(&attempt.attempt_id) == decision.attempt_id.as_ref()
                    })
                    && self.fault_reason(agent_id) != Some(decision.notice.as_str())
                {
                    self.record_fault(agent_id, decision.notice);
                }
            }
        }
        Ok(())
    }

    pub(in crate::tree) fn failed_wake_mail_ids(&self) -> Vec<String> {
        self.operations
            .values()
            .filter(|op| {
                matches!(
                    op.tool_name.as_str(),
                    FAILED_WAKE_MAIL_KIND | UNDELIVERABLE_MAIL_KIND
                )
            })
            .filter_map(|op| serde_json::from_str::<FailedWakeMail>(&op.canonical_request).ok())
            .flat_map(|intent| intent.message_ids)
            .collect()
    }
}

fn conflict() -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::OperationRecoveryRequired,
        "mail decision conflicts with its recorded outcome",
    )
}
