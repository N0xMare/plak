//! Idle pending-work indicator.
//!
//! The composer HUD shows unread child results and questions, process exits
//! not yet seen by a turn, and an Esc-suppressed auto-continue that has
//! something to wake for. The snapshot is emitted on attach and on change. On
//! an interactive face the pause also persists one typed notice so a restore
//! shows the same operator line; JSONL gains nothing.
//!
//! Idle observations use the live hub and projected facts. A boundary may
//! read a nonresident tree from storage; a settled session does not poll it.

use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::PendingContinuation;

use super::{Instant, RootActor, TREE_OBSERVE_INTERVAL};
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
    running_child: bool,
}

impl TreeFacts {
    fn of(
        tree: &crate::agent_tree::ProductAgentTree,
        policy: crate::runtime::ResumeAfterLoss,
    ) -> Self {
        Self {
            descendants: tree.nodes().len() > 1,
            running_child: tree
                .nodes()
                .values()
                .any(|node| node.parent_agent_id.is_some() && node.active_turn),
            work: PendingTreeWork::read(tree, policy),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RootControlRead {
    #[default]
    Due,
    Current(u64),
}

/// The indicator state of one bound session.
///
/// Keyed by session id, so an in-place switch (`/new`, `/resume`) starts
/// the switched-to session from its own baseline instead of this one's.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct PendingTrack {
    session_id: Option<String>,
    last: Option<PendingContinuation>,
    /// Last observed facts for a nonresident tree and for deciding whether
    /// an interrupt could suppress work.
    tree: Option<TreeFacts>,
    process_exits: u32,
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
    root_paused: bool,
    root_controls: RootControlRead,
    reason_read_at: Option<(Instant, bool)>,
    attention: Option<SessionAttention>,
    warned_at: Option<Instant>,
}

impl PendingTrack {
    pub(super) fn unread_count(&self) -> usize {
        self.tree.map_or(0, |tree| {
            tree.work.results as usize + tree.work.questions as usize
        })
    }

    pub(super) fn wake_candidates(&self, tree_is_live: bool) -> bool {
        self.process_exits > 0 || (tree_is_live && self.unread_count() > 0)
    }

    pub(super) fn has_running_child(&self) -> bool {
        self.tree.is_some_and(|tree| tree.running_child)
    }

    pub(super) fn wake_paused(&self) -> bool {
        self.root_paused
    }

    pub(super) fn invalidate_root_controls(&mut self) {
        self.root_controls = RootControlRead::Due;
    }

    fn should_warn(&mut self, entered: bool, at: Instant) -> bool {
        if entered
            && self
                .warned_at
                .is_none_or(|last| at.duration_since(last) >= std::time::Duration::from_secs(60))
        {
            self.warned_at = Some(at);
            return true;
        }
        false
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
        self.publish_pending(false);
    }

    pub(super) fn publish_idle_pending_continuation(&mut self) {
        self.publish_pending(true);
    }

    fn publish_pending(&mut self, idle: bool) {
        self.bind_pending_track();
        if self
            .role
            .live
            .as_mut()
            .is_some_and(super::super::projection::live::LiveProjector::take_change)
        {
            self.role.pending_track.invalidate_root_controls();
        }
        let Some((tree, source)) = self.read_tree_facts_with_store(!idle) else {
            return;
        };
        let tree_changed = self.role.pending_track.tree != Some(tree);
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
        let exits_changed = self.role.pending_track.process_exits != process_exits;
        self.role.pending_track.process_exits = process_exits;
        let work = tree.work;
        let prepared_wakes = prepared_wake_count(&self.role.config.session);
        let has_work =
            work.results > 0 || work.questions > 0 || process_exits > 0 || prepared_wakes > 0;
        let reason = self.read_root_attention_reason(has_work, idle, tree_changed || exits_changed);
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
                || self.role.pending_track.root_paused
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

    fn read_root_attention_reason(
        &mut self,
        has_work: bool,
        idle: bool,
        changed: bool,
    ) -> Option<AttentionReason> {
        let now = Instant::now();
        let track = &mut self.role.pending_track;
        let revision = self.role.config.session.store_revision;
        if changed || track.root_controls != RootControlRead::Current(revision) {
            track.invalidate_root_controls();
        }
        // Work becoming visible refreshes the reason immediately. Otherwise
        // keep the idle observation cadence even when a write dirtied it.
        if track.reason_read_at.is_none_or(|(last, work)| {
            work != has_work
                || ((!idle || track.root_controls == RootControlRead::Due)
                    && now.duration_since(last) >= TREE_OBSERVE_INTERVAL)
        }) {
            track.reason_read_at = Some((now, has_work));
            track.invalidate_root_controls();
            let reason = if has_work {
                self.role.config.store.load_attention_reason(
                    &self.role.config.session.id,
                    self.role.config.resume_after_loss,
                )
            } else {
                Ok(None)
            };
            if let Ok(controls) = self
                .role
                .config
                .store
                .load_wake_controls(&self.role.config.session.id)
            {
                track.root_paused = controls.paused;
                if let Ok(reason) = reason {
                    track.root_reason = reason;
                    track.root_controls = RootControlRead::Current(revision);
                }
            }
        }
        if has_work { track.root_reason } else { None }
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
            if track.should_warn(entered, Instant::now()) {
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
        self.read_tree_facts_with_store(true)
    }

    fn read_tree_facts_with_store(&mut self, store: bool) -> Option<(TreeFacts, &'static str)> {
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
            if !store && let Some(tree) = self.role.pending_track.tree {
                return Some((tree, "last"));
            }
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
        let _ = self.role.events.notice(row);
        let _ = crate::runtime::events::commit_notice_intents(
            &self.role.events,
            &*self.role.config.store,
            &self.role.config.home,
            self.role.log.as_ref(),
            &mut self.role.config.session,
            false,
        );
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
