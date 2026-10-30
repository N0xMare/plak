//! Live activity and cumulative usage owned by the turn projection.

use plakous::agent::AgentEventKind;
use plakous::model::{AttemptId, CallId, ModelEvent, Usage};
use plakous_host_protocol::views::ChildProgress;
use std::collections::{BTreeSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default)]
pub(in crate::runtime) struct Progress {
    pub(in crate::runtime) account: Option<String>,
    pub(in crate::runtime) waiting_for_login: bool,
    started: BTreeSet<(AttemptId, CallId)>,
    pub(in crate::runtime) view: ChildProgress,
    pub(in crate::runtime) terminal: bool,
    pub(in crate::runtime) base: [u64; 5],
    pub(in crate::runtime) subjects: VecDeque<(String, Option<String>)>,
}

impl Progress {
    pub(in crate::runtime) fn observe(
        &mut self,
        kind: &AgentEventKind,
        call: Option<(AttemptId, CallId)>,
    ) {
        self.view.last_activity_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|value| u64::try_from(value.as_millis()).ok());
        match kind {
            AgentEventKind::ModelDispatchStarted => {
                self.waiting_for_login = false;
                self.view.credential_wait = None;
                self.base = [
                    self.view.input_tokens,
                    self.view.output_tokens,
                    self.view.reasoning_tokens,
                    self.view.cached_input_tokens,
                    self.view.cache_write_tokens,
                ];
                self.view.activity = Some("thinking".into());
            }
            AgentEventKind::ModelRequestQueued { slots } => {
                self.view.activity = Some(format!("waiting for a {slots} slot"));
            }
            AgentEventKind::ModelCredentialWait { waiting } => {
                self.waiting_for_login = *waiting;
                self.view.credential_wait = waiting.then(|| self.account.clone()).flatten();
                self.view.activity = waiting.then(|| self.login_notice());
            }
            AgentEventKind::ToolStarted {
                identity,
                observation,
                ..
            } => {
                if call.is_some_and(|binding| self.started.insert(binding)) {
                    self.view.tool_calls = self.view.tool_calls.saturating_add(1);
                }
                let arguments = observation.as_ref().and_then(|item| {
                    serde_json::from_str::<serde_json::Value>(item.arguments_preview()).ok()
                });
                let name = plakous_host_protocol::topology::presented_slot_name(
                    identity.canonical_name().as_str(),
                    arguments.as_ref(),
                    plakous_host_protocol::topology::is_topology_registration(
                        identity.registered_tool_id().as_str(),
                    ),
                );
                self.view.activity = Some(
                    plakous_host_protocol::views::presentation::activity_label(
                        name,
                        crate::activity::tool_activity_kind(name),
                    )
                    .into(),
                );
            }
            AgentEventKind::ToolCompleted { .. } => self.view.activity = None,
            AgentEventKind::Model {
                event: Some(model), ..
            } => match model.as_ref() {
                ModelEvent::TextDelta(_) => self.view.activity = Some("writing".into()),
                ModelEvent::ReasoningDelta(_) => self.view.activity = Some("thinking".into()),
                ModelEvent::Usage(update) => self.usage(update.usage()),
                _ => {}
            },
            AgentEventKind::TurnCompleted { usage } | AgentEventKind::TurnFailed { usage, .. } => {
                self.base = [0; 5];
                self.usage(usage.model());
                self.view.activity = None;
                self.view.credential_wait = None;
                self.waiting_for_login = false;
                self.terminal = true;
            }
            _ => {}
        }
    }

    pub(in crate::runtime) fn login_notice(&self) -> String {
        "waiting for login".to_owned()
    }

    fn usage(&mut self, usage: Usage) {
        self.view.input_tokens = self.base[0].saturating_add(usage.input_tokens());
        self.view.output_tokens = self.base[1].saturating_add(usage.output_tokens());
        self.view.reasoning_tokens = self.base[2].saturating_add(usage.reasoning_tokens());
        self.view.cached_input_tokens = self.base[3].saturating_add(usage.cached_input_tokens());
        self.view.cache_write_tokens = self.base[4].saturating_add(usage.cache_write_tokens());
    }
}
