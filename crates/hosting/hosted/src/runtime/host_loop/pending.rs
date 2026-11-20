//! Idle pending-work indicator.
//!
//! The composer HUD shows unread child results and questions, process exits
//! not yet seen by a turn, and an Esc-suppressed auto-continue that has
//! something to wake for. The snapshot is emitted on attach and on change. On
//! an interactive face the pause also persists one typed notice so a restore
//! shows the same operator line; JSONL gains nothing.
//!
//! Reads use the live hub tree when present; otherwise store observations
//! are limited to the existing tree observation interval.

use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::PendingContinuation;

use super::{Instant, RootActor, TREE_OBSERVE_INTERVAL, commit_session_update};
use crate::runtime::operator_attention::{
    AttentionReason, PendingTreeWork, SessionAttention, prepared_wake_count,
};
use crate::runtime::topology_state::TreeRows;
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) const AUTO_CONTINUE_PAUSE_TEXT: &str = "auto-continue paused until the next prompt";
/// The row a wake that does not fit the window writes for its pause.
pub(super) const WAKE_PREFLIGHT_PAUSE_TEXT: &str =
    "Sub-agent auto-continue preflight paused until the next prompt";
/// The lead of the row a failed wake writes for its pause, before its cause.
pub(super) const WAKE_FAILURE_PAUSE_TEXT: &str =
    "Sub-agent auto-continue paused until the next prompt";

/// What the indicator reads from the session's agent tree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TreeFacts {
    work: PendingTreeWork,
    /// The tree has a node besides the root, so a child could wake the root.
    descendants: bool,
}

impl TreeFacts {
    fn of(
        tree: &crate::agent_tree::ProductAgentTree,
        policy: crate::runtime::ResumeAfterLoss,
    ) -> Self {
        Self {
            descendants: tree.nodes().len() > 1,
            work: PendingTreeWork::read(tree, policy),
        }
    }
}

/// The indicator state of one bound session.
///
/// Keyed by session id, so an in-place switch (`/new`, `/resume`) starts
/// the switched-to session from its own baseline instead of this one's.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct PendingTrack {
    session_id: Option<String>,
    last: Option<PendingContinuation>,
    /// Last observed facts for deciding whether an interrupt could suppress
    /// work. Never used as a fallback for publishing the indicator.
    tree: Option<TreeFacts>,
    store_observed_at: Option<Instant>,
    read_skipped: bool,
    /// The admitted root-turn count when this suppression episode was noted.
    /// Only an admitted user prompt ends suppression and it advances the
    /// count, so a different count is a new episode.
    auto_continue_noted: Option<u64>,
    /// The first publish for this session. A pause already recorded in the
    /// transcript is the restored line, not a second one.
    baseline: bool,
    root_reason: Option<AttentionReason>,
    reason_read_at: Option<Instant>,
    attention: Option<SessionAttention>,
    warned_at: Option<Instant>,
}

impl PendingTrack {
    pub(super) fn unread_count(&self) -> usize {
        self.tree.map_or(0, |tree| {
            tree.work.results as usize + tree.work.questions as usize
        })
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    pub(super) fn has_automatic_continue_work(&mut self) -> bool {
        if !self.role.config.prompt_host_facts().host_wakes {
            return false;
        }
        self.bind_pending_track();
        self.read_tree_facts()
            .map(|(tree, _)| tree)
            .or(self.role.pending_track.tree)
            .is_none_or(|tree| tree.descendants)
            || self
                .opened
                .agent()
                .is_some_and(|agent| !agent.jobs().is_empty())
    }

    pub(super) fn publish_pending_continuation(&mut self) {
        self.bind_pending_track();
        let Some((tree, source)) = self.read_tree_facts() else {
            return;
        };
        self.role.pending_track.tree = Some(tree);
        let jobs = self
            .opened
            .agent()
            .map_or_default(plakous::agent::Agent::jobs);
        let process_exits = jobs
            .iter()
            .filter(|job| job.status != plakous::tools::JobStatus::Running)
            .count();
        let process_exits = u32::try_from(process_exits).unwrap_or(u32::MAX);
        let work = tree.work;
        let prepared_wakes = prepared_wake_count(&self.role.config.session);
        let reason =
            if work.results > 0 || work.questions > 0 || process_exits > 0 || prepared_wakes > 0 {
                self.read_root_attention_reason()
            } else {
                self.role.pending_track.reason_read_at = None;
                None
            };
        let attention = work.attention(reason, process_exits, prepared_wakes);
        let suppressed = self.role.config.session.child_wake_suppressed;
        let suppression_paused = suppressed
            && self.role.config.prompt_host_facts().host_wakes
            && (tree.descendants || !jobs.is_empty());
        let next = PendingContinuation {
            results: work.results,
            questions: work.questions,
            process_exits,
            auto_continue_paused: suppression_paused
                || attention.is_some_and(|attention| attention.reason.pauses_root_wakes()),
        };
        self.publish_operator_attention(attention);
        if self.role.pending_track.last != Some(next) {
            let sent = self.role.events.send(ContentEvent::PendingContinuation {
                results: next.results,
                questions: next.questions,
                process_exits: next.process_exits,
                auto_continue_paused: next.auto_continue_paused,
            });
            tracing::debug!(target: "plak.pending", source, results = next.results,
                questions = next.questions, sent);
            if sent {
                self.role.pending_track.last = Some(next);
            }
        }
        self.note_auto_continue_pause(suppressed, suppression_paused);
        self.role.pending_track.baseline = false;
    }

    fn read_root_attention_reason(&mut self) -> Option<AttentionReason> {
        let now = Instant::now();
        let track = &mut self.role.pending_track;
        if track
            .reason_read_at
            .is_none_or(|last| now.duration_since(last) >= TREE_OBSERVE_INTERVAL)
        {
            track.reason_read_at = Some(now);
            if let Ok(reason) = self.role.config.store.load_attention_reason(
                &self.role.config.session.id,
                self.role.config.resume_after_loss,
            ) {
                track.root_reason = reason;
            }
        }
        track.root_reason
    }

    fn publish_operator_attention(&mut self, attention: Option<SessionAttention>) {
        let track = &mut self.role.pending_track;
        if track.attention == attention {
            return;
        }
        let entered = track.attention.map(|old| old.reason) != attention.map(|new| new.reason);
        track.attention = attention;
        if let Some(release) = &self.role.config.idle_release {
            release.attachments.publish_attention(attention);
        }
        if let Some(attention) = attention {
            let now = Instant::now();
            if entered
                && track.warned_at.is_none_or(|last| {
                    now.duration_since(last) >= std::time::Duration::from_secs(60)
                })
            {
                track.warned_at = Some(now);
                super::wake::log_operator_attention(&self.role.config.session.id, attention);
            }
            let _ = self.role.events.send(ContentEvent::Row(attention.notice()));
        }
    }

    /// The wake path just paused auto-continue and said so in its own row;
    /// this episode needs no second line.
    pub(super) fn auto_continue_pause_reported(&mut self) {
        self.bind_pending_track();
        self.role.pending_track.auto_continue_noted =
            Some(self.role.config.session.root_turn_count);
    }

    fn bind_pending_track(&mut self) {
        let id = &self.role.config.session.id;
        if self.role.pending_track.session_id.as_ref() != Some(id) {
            if let Some(release) = &self.role.config.idle_release {
                release.attachments.publish_attention(None);
            }
            self.role.pending_track = PendingTrack {
                session_id: Some(id.clone()),
                baseline: true,
                ..PendingTrack::default()
            };
        }
    }

    /// A failed or not-yet-due observation is not evidence of a change.
    fn read_tree_facts(&mut self) -> Option<(TreeFacts, &'static str)> {
        let rows = self.role.config.topology_rows();
        let facts_of = |tree: &crate::agent_tree::ProductAgentTree| {
            TreeFacts::of(tree, self.role.config.resume_after_loss)
        };
        let (source, observed) = if let Some(runtime) = self
            .role
            .last_tree_revision
            .runtime(&rows, &self.role.config.session)
        {
            ("hub", runtime.read(facts_of).map_err(|_| ()))
        } else {
            let now = Instant::now();
            if self
                .role
                .pending_track
                .store_observed_at
                .is_some_and(|last| now.duration_since(last) < TREE_OBSERVE_INTERVAL)
            {
                return None;
            }
            self.role.pending_track.store_observed_at = Some(now);
            let observed = self
                .role
                .last_tree_revision
                .tree_id()
                .ok_or(())
                .and_then(|tree_id| rows.tree(tree_id).map_err(|_| ()))
                .map(|tree| tree.as_ref().map_or_default(facts_of));
            ("store", observed)
        };
        let Ok(tree) = observed else {
            if !self.role.pending_track.read_skipped {
                tracing::debug!(target: "plak.pending", source = "skipped",
                    results = ?self.role.pending_track.last.map(|last| last.results),
                    questions = ?self.role.pending_track.last.map(|last| last.questions), sent = false);
            }
            self.role.pending_track.read_skipped = true;
            return None;
        };
        self.role.pending_track.read_skipped = false;
        Some((tree, source))
    }

    fn note_auto_continue_pause(&mut self, suppressed: bool, paused: bool) {
        if !suppressed {
            self.role.pending_track.auto_continue_noted = None;
            return;
        }
        let turn = self.role.config.session.root_turn_count;
        if !paused || self.role.pending_track.auto_continue_noted == Some(turn) {
            return;
        }
        self.role.pending_track.auto_continue_noted = Some(turn);
        self.record_auto_continue_pause();
    }

    fn record_auto_continue_pause(&mut self) {
        if self.role.storage_interrupted && self.role.config.store.storage_unavailable() {
            return;
        }
        let row = TranscriptRow::warning(NoticeScope::Tree, AUTO_CONTINUE_PAUSE_TEXT);
        if !self.role.config.interactive
            || (self.role.pending_track.baseline
                && pause_already_recorded(&self.role.config.session, &row))
        {
            return;
        }
        let stored = row.clone();
        let _ = commit_session_update(
            &*self.role.config.store,
            &self.role.config.home,
            &mut self.role.config.session,
            |session| {
                session.record_notice(stored);
                true
            },
        );
        let _ = self.role.events.send(ContentEvent::Row(row));
    }
}

/// A restored pause line is current when the same notice, or the row a wake
/// failure wrote for its own pause, is already in the transcript and no later
/// user prompt has cleared the episode.
fn pause_already_recorded(session: &crate::sessions::SessionRecord, row: &TranscriptRow) -> bool {
    let rows: &[TranscriptRow] = &session.transcript;
    let Some(index) = rows
        .iter()
        .rposition(|existing| existing == row || reports_wake_pause(existing))
    else {
        return false;
    };
    !rows[index + 1..]
        .iter()
        .any(|later| matches!(later, TranscriptRow::User { .. }))
}

fn reports_wake_pause(row: &TranscriptRow) -> bool {
    matches!(row, TranscriptRow::Notice(notice)
        if notice.scope == NoticeScope::Tree
            && (notice.text == WAKE_PREFLIGHT_PAUSE_TEXT
                || notice.text.starts_with(WAKE_FAILURE_PAUSE_TEXT)))
}

#[cfg(test)]
mod notice_intent_tests;

#[cfg(test)]
mod tests;
