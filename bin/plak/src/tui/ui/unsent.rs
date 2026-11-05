//! An action the session did not take, learned after its send returned
//! (spec 02 §4.4): what the user wrote comes back beside what they are
//! typing now, and a background intent is sent again a little later.

use std::time::{Duration, Instant};

use super::{App, HostPhase, ScrollBlock};
use crate::tui::{SurfaceIntent, UserAction, UserActionSendError};

/// How long a background intent or reload answer the session did not take
/// waits before it is sent again, so a refusing session is not asked on
/// every frame.
const REARM_AFTER: Duration = Duration::from_secs(1);

/// Why the session did not take an action.
pub(super) enum Unsent {
    /// Refused as the mailbox refuses an intent.
    Refused(UserActionSendError),
    /// Refused for this face's role; `row` is its row in the role table.
    ForRole { row: String, action: UserAction },
    /// Sent with no verdict: the session may still run it.
    Unknown(UserAction),
}

impl App {
    /// Takes back what the TUI recorded of `unsent` when it sent it. A draft
    /// or a queued edit is restored beside the composer's text and its
    /// refusal shown, as is a `/new` the session never acknowledged; a
    /// reload check ends as failed. Any other intent is background work and
    /// is armed again, silently, after [`REARM_AFTER`].
    pub(super) fn unsent_action(&mut self, unsent: Unsent) {
        let (action, reason) = match unsent {
            Unsent::Refused(error) => {
                let reason = super::terminal::refusal_reason(&error);
                (error.into_action(), Some(reason))
            }
            Unsent::ForRole { row, action } => {
                (action, Some(super::terminal::role_refusal_reason(&row)))
            }
            Unsent::Unknown(action) => (action, None),
        };
        match action {
            UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_) => {
                let action = self.remove_pending_draft(&action).unwrap_or(action);
                if let UserAction::Prompt(draft)
                | UserAction::FollowUp(draft)
                | UserAction::Steer(draft) = action
                {
                    self.restore_draft(draft);
                }
            }
            UserAction::EditQueued { id, text, .. } => {
                if self
                    .pending_queue_edit
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending == id)
                {
                    self.pending_queue_edit = None;
                }
                self.restore_draft_text(text);
            }
            UserAction::NewSession => {
                if !self.clear_pending {
                    return;
                }
                self.clear_pending = false;
                self.status_detail = None;
                if self.phase == HostPhase::Clear {
                    self.phase = HostPhase::Idle;
                }
            }
            UserAction::Extension(SurfaceIntent::ProbeReload) => {
                self.apply_reload_probed(None);
                return;
            }
            background => {
                let due = self.now() + REARM_AFTER;
                self.rearms.push((due, background));
                return;
            }
        }
        self.push_root_block(ScrollBlock::Error(match reason {
            Some(reason) => format!("{reason} — action not sent; retry when the host catches up"),
            None => "no answer from the product host — the session may still run this; check before sending it again"
                .to_owned(),
        }));
        self.mark_dirty();
    }

    /// When the next background intent the session did not take is armed
    /// again.
    pub(super) fn next_rearm(&self) -> Option<Instant> {
        self.rearms.iter().map(|(due, _)| *due).min()
    }

    /// Arms again every background intent due by `now`, so its own sender
    /// sends it on this pass.
    pub(super) fn rearm_due(&mut self, now: Instant) {
        let (due, later) = std::mem::take(&mut self.rearms)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.rearms = later;
        for (_, action) in due {
            self.rearm(action);
        }
    }

    fn rearm(&mut self, action: UserAction) {
        match action {
            UserAction::Extension(SurfaceIntent::CheckBuild) => self.build_check_sent = None,
            UserAction::Extension(SurfaceIntent::RefreshComposerChrome) => {
                self.composer_chrome.refresh_pending = true;
            }
            UserAction::Extension(SurfaceIntent::RereadUsage) => {
                self.usage_refresh.pending_read = true;
            }
            UserAction::Extension(SurfaceIntent::WatchUsage(armed)) => {
                if self.usage_watch_sent == armed {
                    self.usage_watch_sent = !armed;
                }
            }
            UserAction::Extension(SurfaceIntent::ListAgentAcknowledgements { tree_id }) => {
                self.agent_acknowledgements_unsent(tree_id);
            }
            UserAction::OpenAgentTranscript { agent_id }
                if self
                    .host_agent_transcript
                    .as_ref()
                    .is_some_and(|(served, _)| *served == agent_id) =>
            {
                self.host_agent_transcript = None;
            }
            action @ UserAction::Extension(
                SurfaceIntent::AcceptReloadRequest { .. }
                | SurfaceIntent::RefuseReloadRequest { .. },
            ) => self.reload_answer_unsent(action),
            // Resyncs and call details ask again on their own timers; the
            // rest is nothing to send again.
            _ => {}
        }
    }
}
