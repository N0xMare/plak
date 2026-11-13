use super::{BTreeSet, ProductAgentTree};

impl ProductAgentTree {
    pub(in crate::tree) fn imported_scope(&self, scope: Option<(&str, &str)>) -> BTreeSet<String> {
        let Some(base) = &self.move_base else {
            return BTreeSet::new();
        };
        base.nodes
            .iter()
            .filter(|(id, node)| {
                !self.conversation_retired(id)
                    && scope.is_none_or(|(agent, session)| {
                        *id == agent
                            && ((agent == self.root_agent_id
                                && base.feed_cursors.contains_key(session))
                                || node
                                    .session_locator
                                    .as_deref()
                                    .and_then(|locator| {
                                        locator.parse::<plakous::sessions::SessionLocator>().ok()
                                    })
                                    .is_some_and(|locator| {
                                        locator.session_id().to_string() == session
                                    }))
                    })
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(in crate::tree) fn restore_imported_feed_views(
        &mut self,
        agents: &BTreeSet<String>,
        session: Option<&str>,
    ) {
        let Some(base) = self.move_base.clone() else {
            return;
        };
        for (id, original) in &base.nodes {
            if !agents.contains(id) {
                continue;
            }
            if let Some(node) = self.nodes.get_mut(id) {
                node.fold_order = original.fold_order;
                node.attempt_id.clone_from(&original.attempt_id);
                node.output_schema_json
                    .clone_from(&original.output_schema_json);
                node.schema_digest.clone_from(&original.schema_digest);
                node.active_turn = original.active_turn;
                node.session_locator.clone_from(&original.session_locator);
                node.status = original.status;
            }
        }
        for (id, original) in &base.attempts {
            if !agents.contains(&original.agent_id) {
                continue;
            }
            if let Some(attempt) = self.attempts.get_mut(id) {
                attempt.status = original.status;
                attempt.output_json.clone_from(&original.output_json);
                attempt.candidate_id.clone_from(&original.candidate_id);
                attempt.ended_at = original.ended_at;
                attempt.failure.clone_from(&original.failure);
                attempt.blocked.clone_from(&original.blocked);
                attempt.turn_id.clone_from(&original.turn_id);
                attempt
                    .applied_terminal_read_seqs
                    .clone_from(&original.applied_terminal_read_seqs);
                attempt.usage.clone_from(&original.usage);
                attempt.submit_harness_nudge = original.submit_harness_nudge;
            }
        }
        for (id, original) in &base.commands {
            if !agents.contains(&original.recipient) {
                continue;
            }
            if let Some(command) = self.commands.get_mut(id) {
                command.fact = original.fact;
                command.turn_id.clone_from(&original.turn_id);
                command.accepted_seq = original.accepted_seq;
            }
        }
        for (id, original) in &base.messages {
            if agents.contains(&original.to)
                && let Some(mail) = self.messages.get_mut(id)
            {
                mail.durable = original.durable;
            }
        }
        for (id, floor) in &base.feed_cursors {
            if session.is_none_or(|session| session == id) {
                self.feed_cursors.insert(id.clone(), *floor);
            }
        }
    }
}
