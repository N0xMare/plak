//! Bounded host log evidence, never a durable security journal.

use super::{Console, HostPrincipal, HostRequestContext};
use plakd_wire_types::console::{HostRequest, HostResponse};
use plakous_host_protocol::wire::RequestRefusal;
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct State {
    windows: BTreeMap<HostPrincipal, Window>,
    #[cfg(test)]
    pub(super) records: Vec<serde_json::Value>,
}
struct Window {
    since: Instant,
    emitted: u16,
    suppressed: u64,
}

impl Console {
    pub(in super::super) fn audit(
        &self,
        context: &HostRequestContext,
        action: &'static str,
        target: Option<&str>,
        category: &'static str,
        revisions: Option<(&str, &str)>,
    ) {
        let now = Instant::now();
        let mut state = self.audit.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .windows
            .retain(|_, window| now.duration_since(window.since) < Duration::from_secs(120));
        if !state.windows.contains_key(&context.principal) && state.windows.len() >= 1024 {
            return;
        }
        let window = state
            .windows
            .entry(context.principal.clone())
            .or_insert(Window {
                since: now,
                emitted: 0,
                suppressed: 0,
            });
        let mut suppressed = 0;
        if now.duration_since(window.since) >= Duration::from_secs(60) {
            suppressed = window.suppressed;
            *window = Window {
                since: now,
                emitted: 0,
                suppressed: 0,
            };
        }
        if window.emitted >= 180 {
            window.suppressed = window.suppressed.saturating_add(1);
            return;
        }
        window.emitted += 1;
        drop(state);
        let (face, login) = match &context.principal {
            HostPrincipal::Socket(uid) => ("socket", uid.to_string()),
            HostPrincipal::Web(login) => ("web", login.clone()),
        };
        let (old_etag, new_etag) =
            revisions.map_or((None, None), |(old, new)| (Some(old), Some(new)));
        let record = serde_json::json!({"audit":"operator", "time_ms":super::clock::now_ms(), "login":login, "face":face, "connection_id":context.connection_id, "request_id":context.request_id, "action":action, "target":target, "category":category, "old_etag":old_etag,"new_etag":new_etag,"aggregated_operations":suppressed});
        #[cfg(test)]
        self.audit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .records
            .push(record.clone());
        eprintln!("{record}");
    }
}

pub(super) struct Guard {
    console: Arc<Console>,
    context: HostRequestContext,
    action: &'static str,
    target: Option<String>,
    category: &'static str,
}
impl Guard {
    pub fn new(console: Arc<Console>, context: HostRequestContext, request: &HostRequest) -> Self {
        let action = match request {
            HostRequest::TerminalOpen { .. } => "terminal_open",
            HostRequest::TerminalList { .. } => "terminal_list",
            HostRequest::TerminalAttach { takeover: true, .. } => "terminal_takeover",
            HostRequest::TerminalAttach { .. } => "terminal_attach",
            HostRequest::TerminalResize { .. } => "terminal_resize",
            HostRequest::TerminalClose { .. } => "terminal_close",
            HostRequest::TerminalDetach { .. } => "terminal_detach",
            HostRequest::TerminalInput { .. } => "terminal_input",
            HostRequest::TerminalCreditBatch { .. } => "terminal_credit_batch",
            HostRequest::TerminalReleaseSession { .. } => "terminal_release_session",
            HostRequest::FilesList { .. } => "files_list",
            HostRequest::FilesRead { .. } => "files_read",
            HostRequest::FilesSearch { .. } => "files_search",
            HostRequest::ChangesStatus { .. } => "changes_status",
            HostRequest::ChangesDiff { .. } => "changes_diff",
            HostRequest::OperatorPrepare { .. } => "operator_prepare",
            HostRequest::OperatorConfirm { .. } => "operator_confirm",
            HostRequest::AccountKeySet { .. } => "account_key_set",
            HostRequest::AccountKeyClear { .. } => "account_key_clear",
            HostRequest::AccountLogout {
                local_only: true, ..
            } => "account_logout_local",
            HostRequest::AccountLogout { .. } => "account_logout",
            HostRequest::AccountRefresh { .. } => "account_refresh",
            HostRequest::AccountRemove { .. } => "account_remove",
            HostRequest::AccountLoginStart {
                replace_newer: true,
                ..
            } => "account_login_start_replace_newer",
            HostRequest::AccountLoginStart { .. } => "account_login_start",
            HostRequest::AccountLoginPoll { .. } => "account_login_poll",
            HostRequest::AccountLoginCancel { .. } => "account_login_cancel",
            HostRequest::AccountLoginComplete { .. } => "account_login_complete",
            HostRequest::ConfigPatch { .. } => "config_patch",
            HostRequest::TemplatePut { .. } => "template_put",
            HostRequest::ProfilePut { .. } => "profile_put",
            _ => "console_read",
        };
        let target = if let Some(target) = super::files::target(request) {
            let safe = |id: &str| {
                !id.is_empty()
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            };
            safe(&target.session).then(|| {
                target
                    .node
                    .as_deref()
                    .filter(|node| safe(node))
                    .map_or_else(
                        || target.session.clone(),
                        |node| format!("{}:{node}", target.session),
                    )
            })
        } else {
            match request {
                HostRequest::TemplatePut { id, .. } | HostRequest::ProfilePut { id, .. }
                    if crate::product_config::validate_template_name(id).is_ok() =>
                {
                    Some(id.clone())
                }
                HostRequest::ConfigPatch { .. } => Some("home".into()),
                _ => super::account_writes::action(request)
                    .as_ref()
                    .and_then(super::operator::account)
                    .filter(|id| super::accounts::exact(id).is_ok())
                    .map(str::to_owned),
            }
        };
        Self {
            console,
            context,
            action,
            target,
            category: "outcome_unknown",
        }
    }
    pub fn finish(&mut self, result: &Result<HostResponse, RequestRefusal>) {
        self.category = match result {
            Ok(HostResponse::OperationOutcomeUnknown {}) => "outcome_unknown",
            Ok(HostResponse::Refused { .. }) => "refusal",
            Ok(HostResponse::Saved {
                published_durability_uncertain: true,
                ..
            }) => "published_durability_uncertain",
            Ok(_) => "success",
            Err(RequestRefusal::RateLimited { .. }) => "rate_limited",
            Err(_) => "refusal",
        };
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.console.audit(
            &self.context,
            self.action,
            self.target.as_deref(),
            self.category,
            None,
        );
    }
}
