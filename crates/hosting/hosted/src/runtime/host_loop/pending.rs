//! Idle pending-work indicator.
//!
//! The composer HUD shows unread child results and questions, process exits
//! not yet seen by a turn, an Esc-suppressed auto-continue that has something
//! to wake for, and a restored-paused tree. The snapshot is emitted on attach
//! and on change. On an interactive face a pause also persists one typed
//! notice so a restore shows the same operator line; JSONL gains nothing.
//!
//! Reads use the live hub tree when present; otherwise store observations
//! are limited to the existing tree observation interval.

use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::PendingContinuation;

use super::{HostLoop, Instant, TREE_OBSERVE_INTERVAL, commit_session_update};
use crate::runtime::topology_state::TreeRows;
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) const AUTO_CONTINUE_PAUSE_TEXT: &str = "auto-continue paused until the next prompt";
pub(super) const TREE_PAUSE_TEXT: &str = "agents paused · /tree resume";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PauseKind {
    AutoContinue,
    Tree,
}

impl PauseKind {
    fn row(self) -> TranscriptRow {
        let text = match self {
            Self::AutoContinue => AUTO_CONTINUE_PAUSE_TEXT,
            Self::Tree => TREE_PAUSE_TEXT,
        };
        TranscriptRow::warning(NoticeScope::Tree, text)
    }
}

/// What the indicator reads from the session's agent tree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TreeFacts {
    results: u32,
    questions: u32,
    paused: bool,
    /// The tree has a node besides the root, so a child could wake the root.
    descendants: bool,
}

impl TreeFacts {
    fn of(tree: &crate::agent_tree::ProductAgentTree) -> Self {
        let mut facts = Self {
            paused: tree.state == crate::agent_tree_protocol::TreeAdmissionState::RestoredPaused,
            descendants: tree.nodes().len() > 1,
            ..Self::default()
        };
        for mail in tree.unread_mail(&tree.root_agent_id) {
            match mail.purpose {
                crate::agent_tree_protocol::MessagePurpose::Result => {
                    facts.results = facts.results.saturating_add(1);
                }
                crate::agent_tree_protocol::MessagePurpose::Question => {
                    facts.questions = facts.questions.saturating_add(1);
                }
                _ => {}
            }
        }
        facts
    }
}

/// The indicator state of one bound session.
///
/// Keyed by session id, so an in-place switch (`/clear`, `/resume`) starts
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
    tree_noted: bool,
    /// The first publish for this session. A pause already recorded in the
    /// transcript is the restored line, not a second one.
    baseline: bool,
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn has_automatic_continue_work(&mut self) -> bool {
        if !self.config.prompt_host_facts().host_wakes {
            return false;
        }
        self.bind_pending_track();
        self.read_tree_facts()
            .map(|(tree, _)| tree)
            .or(self.pending_track.tree)
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
        self.pending_track.tree = Some(tree);
        let jobs = self
            .opened
            .agent()
            .map(plakous::agent::Agent::jobs)
            .unwrap_or_default();
        let process_exits = jobs
            .iter()
            .filter(|job| job.status != plakous::tools::JobStatus::Running)
            .count();
        let suppressed = self.config.session.child_wake_suppressed;
        let next = PendingContinuation {
            results: tree.results,
            questions: tree.questions,
            process_exits: u32::try_from(process_exits).unwrap_or(u32::MAX),
            auto_continue_paused: suppressed
                && self.config.prompt_host_facts().host_wakes
                && (tree.descendants || !jobs.is_empty()),
            tree_paused: tree.paused,
        };
        if self.pending_track.last != Some(next) {
            let sent = self.events.send(ContentEvent::PendingContinuation {
                results: next.results,
                questions: next.questions,
                process_exits: next.process_exits,
                auto_continue_paused: next.auto_continue_paused,
                tree_paused: next.tree_paused,
            });
            tracing::debug!(target: "plak.pending", source, results = next.results,
                questions = next.questions, sent);
            if sent {
                self.pending_track.last = Some(next);
            }
        }
        self.note_auto_continue_pause(suppressed, next.auto_continue_paused);
        self.note_tree_pause(next.tree_paused);
        self.pending_track.baseline = false;
    }

    /// The wake path just paused auto-continue and said so in its own row;
    /// this episode needs no second line.
    pub(super) fn auto_continue_pause_reported(&mut self) {
        self.bind_pending_track();
        self.pending_track.auto_continue_noted = Some(self.config.session.root_turn_count);
    }

    fn bind_pending_track(&mut self) {
        let id = &self.config.session.id;
        if self.pending_track.session_id.as_ref() != Some(id) {
            self.pending_track = PendingTrack {
                session_id: Some(id.clone()),
                baseline: true,
                ..PendingTrack::default()
            };
        }
    }

    /// A failed or not-yet-due observation is not evidence of a change.
    fn read_tree_facts(&mut self) -> Option<(TreeFacts, &'static str)> {
        let rows = self.config.topology_rows();
        let (source, observed) =
            if let Some(runtime) = self.last_tree_revision.runtime(&rows, &self.config.session) {
                ("hub", runtime.read(TreeFacts::of).map_err(|_| ()))
            } else {
                let now = Instant::now();
                if self
                    .pending_track
                    .store_observed_at
                    .is_some_and(|last| now.duration_since(last) < TREE_OBSERVE_INTERVAL)
                {
                    return None;
                }
                self.pending_track.store_observed_at = Some(now);
                let observed = self
                    .last_tree_revision
                    .tree_id()
                    .ok_or(())
                    .and_then(|tree_id| rows.tree(tree_id).map_err(|_| ()))
                    .map(|tree| tree.as_ref().map(TreeFacts::of).unwrap_or_default());
                ("store", observed)
            };
        let Ok(tree) = observed else {
            if !self.pending_track.read_skipped {
                tracing::debug!(target: "plak.pending", source = "skipped",
                    results = ?self.pending_track.last.map(|last| last.results),
                    questions = ?self.pending_track.last.map(|last| last.questions), sent = false);
            }
            self.pending_track.read_skipped = true;
            return None;
        };
        self.pending_track.read_skipped = false;
        Some((tree, source))
    }

    fn note_auto_continue_pause(&mut self, suppressed: bool, paused: bool) {
        if !suppressed {
            self.pending_track.auto_continue_noted = None;
            return;
        }
        let turn = self.config.session.root_turn_count;
        if !paused || self.pending_track.auto_continue_noted == Some(turn) {
            return;
        }
        self.pending_track.auto_continue_noted = Some(turn);
        self.record_pause(PauseKind::AutoContinue);
    }

    fn note_tree_pause(&mut self, paused: bool) {
        if !paused {
            self.pending_track.tree_noted = false;
            return;
        }
        if !self.pending_track.tree_noted {
            self.pending_track.tree_noted = true;
            self.record_pause(PauseKind::Tree);
        }
    }

    fn record_pause(&mut self, kind: PauseKind) {
        if self.storage_interrupted && self.config.store.storage_unavailable() {
            return;
        }
        let row = kind.row();
        if !self.config.interactive
            || (self.pending_track.baseline && pause_already_recorded(&self.config.session, &row))
        {
            return;
        }
        let stored = row.clone();
        let _ = commit_session_update(
            &*self.config.store,
            &self.config.home,
            &mut self.config.session,
            |session| {
                session.record_notice(stored);
                true
            },
        );
        let _ = self.events.send(ContentEvent::Row(row));
    }
}

/// A restored pause line is current when the same notice is already in the
/// transcript and no later user prompt has cleared the episode.
fn pause_already_recorded(session: &crate::sessions::SessionRecord, row: &TranscriptRow) -> bool {
    let rows: &[TranscriptRow] = &session.transcript;
    let Some(index) = rows.iter().rposition(|existing| existing == row) else {
        return false;
    };
    !rows[index + 1..]
        .iter()
        .any(|later| matches!(later, TranscriptRow::User { .. }))
}

#[cfg(test)]
mod notice_intent_tests;

#[cfg(test)]
mod tests;
