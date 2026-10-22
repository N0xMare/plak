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

use plakous_host_protocol::PendingContinuation;

use super::{HostEvent, HostLoop, Instant, TREE_OBSERVE_INTERVAL, commit_session_update};
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
            let sent = self.events.send(HostEvent::PendingContinuation {
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
        let _ = self.events.send(HostEvent::Row(row));
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
mod tests {
    use super::super::HostLoop;
    use super::{AUTO_CONTINUE_PAUSE_TEXT, PendingContinuation, TREE_PAUSE_TEXT};
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{
        MessagePurpose, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest,
        TreeAdmissionState, WriterMode,
    };
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::sessions::{NoticeScope, NoticeTone, TranscriptRow};
    use crate::surface_protocol::{HostEventReceiver, UserAction, UserActionSender};
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    pub(super) fn snapshots(
        observed: &HostEventReceiver<TestExtension>,
    ) -> Vec<PendingContinuation> {
        observed
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::PendingContinuation {
                    results,
                    questions,
                    process_exits,
                    auto_continue_paused,
                    tree_paused,
                } => Some(PendingContinuation {
                    results,
                    questions,
                    process_exits,
                    auto_continue_paused,
                    tree_paused,
                }),
                _ => None,
            })
            .collect()
    }

    pub(super) fn pause_rows(session: &crate::sessions::SessionRecord, text: &str) -> usize {
        session
            .transcript
            .rows()
            .iter()
            .filter(|(row, _)| {
                matches!(
                    row,
                    TranscriptRow::Notice(notice)
                        if notice.scope == NoticeScope::Tree
                            && notice.tone == NoticeTone::Warning
                            && notice.text == text
                )
            })
            .count()
    }

    fn with_tree(
        results: usize,
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &UserActionSender<TestExtension>,
            &HostEventReceiver<TestExtension>,
        ),
    ) {
        with_configured_tree(results, |_| {}, test);
    }

    pub(super) fn with_configured_tree(
        results: usize,
        configure: impl FnOnce(&mut crate::runtime::HostConfig<TestExtension>),
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &UserActionSender<TestExtension>,
            &HostEventReceiver<TestExtension>,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(dir.path());
        configure(&mut config);
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &config.session.id,
            "root",
            "owner",
            config.session.workspace_root.clone(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        for _ in 0..results {
            let child = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
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
        }
        tree.settle_for_test();
        let runtime =
            crate::topology_runtime::insert_root(config.session.id.clone(), tree, None).unwrap();
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events),
            None,
        )
        .unwrap();
        test(&mut host, &runtime, &actions, &observed);
        host.release().unwrap();
    }

    pub(super) fn reopen(
        host: &mut HostLoop<'_, TestExtension>,
        test: impl FnOnce(&mut HostLoop<'_, TestExtension>),
    ) {
        host.config.store.save(&mut host.config.session).unwrap();
        host.executor
            .block_on(host.opened.detach())
            .unwrap()
            .unwrap();
        let root = host.config.product_home.parent().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root);
        config.state_roots = host.config.state_roots.clone();
        config.store = host.config.store.clone();
        config.session = config.store.load(&host.config.session.id).unwrap();
        config.runtime_factory = host.config.runtime_factory.clone();
        config.interactive = host.config.interactive;
        let mut restored = HostLoop::open(config, host.actions, host.events.clone(), None).unwrap();
        test(&mut restored);
        restored
            .executor
            .block_on(restored.opened.detach())
            .unwrap()
            .unwrap();
        host.opened = restored.opened;
        host.config.session = restored.config.session;
    }

    /// A running child: something that could wake the root, with nothing
    /// pending yet.
    pub(super) fn spawn_child(runtime: &crate::topology_runtime::ProductTopologyRuntime) {
        runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)
            })
            .unwrap();
    }

    pub(super) fn pause_tree(runtime: &crate::topology_runtime::ProductTopologyRuntime) {
        runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)?;
                tree.set_active_turn(&child.agent_id, true)
                    .map_err(|error| error.message)?;
                tree.state = TreeAdmissionState::RestoredPaused;
                Ok(())
            })
            .unwrap();
    }

    fn notes(events: &[HostEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(notice)) => Some(notice.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn take_prompt(
        step: super::super::HostStep,
    ) -> (String, Option<crate::sessions::QueueItem>, bool) {
        let super::super::HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = step
        else {
            panic!("expected a prompt")
        };
        (text, queued_delivery, user_echoed)
    }

    fn last_prompt_times(observed: &HostEventReceiver<TestExtension>) -> Vec<Option<u64>> {
        observed
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::LastUserPromptAt(sent) => Some(sent),
                _ => None,
            })
            .collect()
    }

    /// The offline fixture's driver clock: every entry identity it mints is
    /// a `UUIDv7` stamped at this Unix time.
    const FIXTURE_CLOCK_MS: u64 = 10_000;

    /// Spec 02 §5.4: the idle clock survives reopen. The attach reads the
    /// operator's last prompt time from the entry identity in canonical
    /// history; nothing new is stored.
    #[test]
    fn reopen_restores_the_stored_time_of_the_last_operator_prompt() {
        with_tree(0, |host, _, actions, observed| {
            assert_eq!(last_prompt_times(observed), [None], "no prompt yet");
            actions.send(UserAction::Prompt("hello".into())).unwrap();
            let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
            host.run_prompt(text, item, echoed, None).unwrap();
            let _ = last_prompt_times(observed);
            reopen(host, |_| {
                assert_eq!(last_prompt_times(observed), [Some(FIXTURE_CLOCK_MS)]);
            });
        });
    }

    /// An auto-continue wake is host-authored: a session whose only turn was
    /// a wake has no operator prompt to restore.
    #[test]
    fn a_wake_turn_restores_no_prompt_time() {
        with_tree(1, |host, _, _, observed| {
            assert_eq!(last_prompt_times(observed), [None]);
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let (text, item, echoed) = take_prompt(
                host.child_wake_prompt(now + Duration::from_secs(2))
                    .unwrap(),
            );
            assert!(item.is_some(), "the wake is a host-authored delivery");
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(host.config.session.root_turn_count, 1, "the wake ran");
            reopen(host, |_| {
                assert_eq!(last_prompt_times(observed), [None]);
            });
        });
    }

    #[test]
    fn unread_results_raise_the_indicator_clear_after_the_turn_and_restore() {
        with_tree(1, |host, _, _, observed| {
            let raised = snapshots(observed);
            assert_eq!(raised.last().map(|pending| pending.results), Some(1));
            reopen(host, |restored| {
                let again = snapshots(observed);
                assert_eq!(again.last().map(|pending| pending.results), Some(1));
                let now = Instant::now();
                restored.child_wake_prompt(now).unwrap();
                let (text, item, echoed) = take_prompt(
                    restored
                        .child_wake_prompt(now + Duration::from_secs(2))
                        .unwrap(),
                );
                restored.run_prompt(text, item, echoed, None).unwrap();
                let cleared = snapshots(observed);
                assert!(
                    cleared.iter().any(|pending| pending.results == 0),
                    "result indicator did not clear: {cleared:?}"
                );
            });
        });
    }

    #[test]
    fn pending_store_observations_refresh_acknowledged_results() {
        use crate::runtime::topology_state::TreeRows;

        let dir = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(dir.path());
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &config.session.id,
            "root",
            "owner",
            config.session.workspace_root.clone(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
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
        config.topology_rows().save_tree(&mut tree).unwrap();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events),
            None,
        )
        .unwrap();
        assert!(crate::topology_runtime::lookup(&host.config.session.id).is_none());
        host.publish_pending_continuation();
        assert_eq!(
            snapshots(&observed).last().map(|pending| pending.results),
            Some(1)
        );
        let rows = host.config.topology_rows();
        let mut tree = rows.tree(&host.config.session.id).unwrap().unwrap();
        let mail = tree.unread_mail("root").next().unwrap().message_id.clone();
        tree.acknowledge_mail(&mail).unwrap();
        rows.save_tree(&mut tree).unwrap();
        host.pending_track.store_observed_at =
            Instant::now().checked_sub(super::super::TREE_OBSERVE_INTERVAL);
        host.publish_pending_continuation();
        assert_eq!(
            snapshots(&observed).last().map(|pending| pending.results),
            Some(0)
        );
        host.release().unwrap();
    }

    #[test]
    fn pending_refused_zero_is_resent_when_facts_are_unchanged() {
        with_tree(1, |host, runtime, _, observed| {
            assert_eq!(snapshots(observed).last().unwrap().results, 1);
            runtime
                .mutate_then_emit(false, |tree| {
                    let id = tree.unread_mail("root").next().unwrap().message_id.clone();
                    tree.acknowledge_mail(&id).map_err(|error| error.message)
                })
                .unwrap();
            // Terminal receipts cannot be evicted to admit the zero snapshot.
            while host.events.send(HostEvent::ModelRefusal { detail: None }) {}
            host.publish_pending_continuation();
            let _ = snapshots(observed);
            host.publish_pending_continuation();
            assert_eq!(
                snapshots(observed).last().map(|pending| pending.results),
                Some(0)
            );
        });
    }

    #[test]
    fn pending_wake_and_run_prompt_stays_clear_across_idle_iterations() {
        with_tree(1, |host, _, actions, observed| {
            assert_eq!(snapshots(observed).last().unwrap().results, 1);
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let (text, item, echoed) = take_prompt(
                host.child_wake_prompt(now + Duration::from_secs(2))
                    .unwrap(),
            );
            host.run_prompt(text, item, echoed, None).unwrap();
            let mut seen = snapshots(observed);
            for _ in 0..10 {
                actions.send(UserAction::Interrupt).unwrap();
                let _ = host.next_prompt(None).unwrap();
                seen.extend(snapshots(observed));
                assert_eq!(host.pending_track.last.unwrap().results, 0);
            }
            assert_eq!(seen.last().map(|pending| pending.results), Some(0));
            assert!(seen.iter().all(|pending| pending.results == 0), "{seen:?}");
        });
    }

    #[test]
    fn unread_questions_raise_and_clear_the_indicator() {
        with_tree(0, |host, runtime, _, observed| {
            let _ = snapshots(observed);
            let child = runtime
                .mutate_then_emit(false, |tree| {
                    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .map_err(|error| error.message)
                })
                .unwrap();
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.send_agent_message(
                        &child.agent_id,
                        &SendAgentMessageRequest {
                            agent_id: "root".into(),
                            message: "question".into(),
                            purpose: Some(MessagePurpose::Question),
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
            host.publish_pending_continuation();
            let raised = snapshots(observed);
            assert_eq!(raised.last().map(|pending| pending.questions), Some(1));
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let (text, item, echoed) = take_prompt(
                host.child_wake_prompt(now + Duration::from_secs(2))
                    .unwrap(),
            );
            host.run_prompt(text, item, echoed, None).unwrap();
            let cleared = snapshots(observed);
            assert!(
                cleared.iter().any(|pending| pending.questions == 0),
                "question indicator did not clear: {cleared:?}"
            );
        });
    }

    #[test]
    fn auto_continue_pause_raises_a_notice_and_restores_the_same_row() {
        with_tree(0, |host, runtime, actions, observed| {
            spawn_child(runtime);
            let _ = snapshots(observed);
            actions.send(UserAction::Interrupt).unwrap();
            assert!(matches!(
                host.next_prompt(None).unwrap(),
                super::super::HostStep::Continue
            ));
            let raised = snapshots(observed);
            assert!(
                raised
                    .last()
                    .is_some_and(|pending| pending.auto_continue_paused)
            );
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                1
            );
            reopen(host, |restored| {
                assert_eq!(
                    pause_rows(&restored.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                    1,
                    "restore duplicated or dropped the pause notice"
                );
                let again = snapshots(observed);
                assert!(
                    again
                        .last()
                        .is_some_and(|pending| pending.auto_continue_paused)
                );
                assert!(
                    restored
                        .config
                        .session
                        .transcript
                        .rows()
                        .iter()
                        .any(|(row, _)| {
                            *row == &TranscriptRow::warning(
                                NoticeScope::Tree,
                                AUTO_CONTINUE_PAUSE_TEXT,
                            )
                        })
                );
            });
        });
    }

    #[test]
    fn paused_tree_raises_the_indicator_and_restores_the_same_notice() {
        with_tree(0, |host, runtime, _, observed| {
            let _ = snapshots(observed);
            pause_tree(runtime);
            host.publish_pending_continuation();
            let raised = snapshots(observed);
            assert!(raised.last().is_some_and(|pending| pending.tree_paused));
            assert_eq!(pause_rows(&host.config.session, TREE_PAUSE_TEXT), 1);
            reopen(host, |restored| {
                assert_eq!(pause_rows(&restored.config.session, TREE_PAUSE_TEXT), 1);
                let again = snapshots(observed);
                assert!(
                    again.last().is_some_and(|pending| pending.tree_paused),
                    "restored tree did not stay paused: {again:?}"
                );
            });
        });
    }

    /// The attach-time paused-queue note restates state every open
    /// re-announces, so it is live only: reopening a still-paused session
    /// never adds a copy.
    #[test]
    fn reopening_a_paused_queue_does_not_grow_the_transcript() {
        with_tree(0, |host, _, _, _| {
            host.config.session.queue_drain_state =
                crate::sessions::QueueDrainState::RestoredPaused;
            host.config
                .session
                .pending_queue
                .push(crate::sessions::QueueItem::new("held prompt"));
            let queue_notes = |session: &crate::sessions::SessionRecord| {
                session
                    .transcript
                    .iter()
                    .filter(|row| {
                        matches!(
                            row,
                            TranscriptRow::Notice(notice)
                                if notice.scope == NoticeScope::Queue
                                    && notice.text.contains("the queue is paused")
                        )
                    })
                    .count()
            };
            for _ in 0..3 {
                reopen(host, |restored| {
                    restored.commit_notice_intents();
                });
            }
            let stored = host.config.store.load(&host.config.session.id).unwrap();
            assert_eq!(
                queue_notes(&stored),
                0,
                "three opens of one paused session stored copies of the open note"
            );
        });
    }

    /// Spec 02 §5.4: a notice an open re-announces is live only, and the
    /// interactive pause row persists once per episode, so after the first
    /// open of a paused session later opens add nothing to its transcript.
    #[test]
    fn reopening_an_unchanged_paused_session_stores_nothing_new() {
        with_tree(0, |host, runtime, _, _| {
            pause_tree(runtime);
            host.config.session.queue_drain_state =
                crate::sessions::QueueDrainState::RestoredPaused;
            host.config
                .session
                .pending_queue
                .push(crate::sessions::QueueItem::new("held prompt"));
            reopen(host, |restored| {
                restored.commit_notice_intents();
            });
            let first = host.config.store.load(&host.config.session.id).unwrap();
            for _ in 0..2 {
                reopen(host, |restored| {
                    restored.commit_notice_intents();
                });
            }
            let stored = host.config.store.load(&host.config.session.id).unwrap();
            assert_eq!(stored.transcript, first.transcript);
        });
    }

    /// A JSONL face writes no pause rows (spec 02 §5.4): its folded open
    /// note names the paused tree live only, and the drain stores nothing.
    #[test]
    fn a_jsonl_face_persists_no_tree_pause_note() {
        with_configured_tree(
            0,
            |config| config.interactive = false,
            |host, runtime, _, _| {
                pause_tree(runtime);
                host.commit_notice_intents();
                let before = host.config.session.clone();
                for _ in 0..2 {
                    reopen(host, |restored| {
                        restored.commit_notice_intents();
                    });
                }
                let stored = host.config.store.load(&host.config.session.id).unwrap();
                assert_eq!(
                    stored.transcript, before.transcript,
                    "an open of a JSONL face stores nothing"
                );
                let persisted: Vec<_> = stored
                    .transcript
                    .iter()
                    .filter_map(|row| match row {
                        TranscriptRow::Notice(notice)
                            if notice.text.starts_with("the agent tree is paused") =>
                        {
                            Some(notice.text.clone())
                        }
                        _ => None,
                    })
                    .collect();
                assert!(
                    persisted.is_empty(),
                    "a JSONL face persisted {} pause notes",
                    persisted.len()
                );
            },
        );
    }

    #[test]
    fn opening_a_paused_tree_shows_one_tree_line() {
        with_tree(0, |host, runtime, _, observed| {
            pause_tree(runtime);
            reopen(host, |restored| {
                let events: Vec<_> = observed.try_iter().collect();
                let tree_lines: Vec<_> = notes(&events)
                    .into_iter()
                    .filter(|note| note.contains("paused") && !note.contains("queue"))
                    .collect();
                assert_eq!(
                    tree_lines,
                    [TREE_PAUSE_TEXT],
                    "opening a paused tree said it more than once"
                );
                assert_eq!(pause_rows(&restored.config.session, TREE_PAUSE_TEXT), 1);
            });
        });
    }

    #[test]
    fn a_jsonl_face_keeps_the_open_note_and_writes_no_pause_rows() {
        with_configured_tree(
            0,
            |config| config.interactive = false,
            |host, runtime, actions, observed| {
                spawn_child(runtime);
                actions.send(UserAction::Interrupt).unwrap();
                let _ = host.next_prompt(None).unwrap();
                pause_tree(runtime);
                host.publish_pending_continuation();
                reopen(host, |restored| {
                    let events: Vec<_> = observed.try_iter().collect();
                    let notes = notes(&events);
                    assert!(
                        notes
                            .iter()
                            .any(|note| note.starts_with("the agent tree is paused")),
                        "the JSONL open note lost its tree remedy: {notes:?}"
                    );
                    assert!(
                        !notes
                            .iter()
                            .any(|note| note == TREE_PAUSE_TEXT || note == AUTO_CONTINUE_PAUSE_TEXT),
                        "a JSONL face gained a pause line: {notes:?}"
                    );
                    for text in [TREE_PAUSE_TEXT, AUTO_CONTINUE_PAUSE_TEXT] {
                        assert_eq!(pause_rows(&restored.config.session, text), 0);
                    }
                });
            },
        );
    }

    #[test]
    fn interrupt_with_nothing_that_could_wake_shows_no_pause() {
        with_tree(0, |host, _, actions, observed| {
            let before = host.config.session.clone();
            actions.send(UserAction::Interrupt).unwrap();
            let _ = host.next_prompt(None).unwrap();
            assert!(!host.config.session.child_wake_suppressed);
            assert_eq!(host.config.session, before);
            assert!(
                !snapshots(observed)
                    .iter()
                    .any(|pending| pending.auto_continue_paused)
            );
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                0
            );
        });
    }

    #[test]
    fn a_new_suppression_after_an_admitted_prompt_is_a_new_pause_line() {
        with_tree(0, |host, runtime, actions, _| {
            spawn_child(runtime);
            actions.send(UserAction::Interrupt).unwrap();
            let _ = host.next_prompt(None).unwrap();
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                1
            );
            // An admitted user prompt ends the episode; an interrupt that the
            // idle loop never saw unpaused starts the next one.
            host.config.session.root_turn_count += 1;
            host.publish_pending_continuation();
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                2
            );
            host.publish_pending_continuation();
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                2
            );
        });
    }

    #[test]
    fn a_pause_the_wake_path_already_reported_writes_no_second_line() {
        with_tree(0, |host, runtime, _, observed| {
            spawn_child(runtime);
            host.config.session.child_wake_suppressed = true;
            host.auto_continue_pause_reported();
            host.publish_pending_continuation();
            assert!(
                snapshots(observed)
                    .last()
                    .is_some_and(|pending| pending.auto_continue_paused)
            );
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                0
            );
        });
    }

    /// An in-place resume is an attach of the resumed session; it keeps that
    /// session's already recorded pause row single.
    ///
    /// The clear's source attachment release is held half a second past its runtime
    /// release, so the resume always arrives while it is still pending: the
    /// switch joins it before reading the source, never a copy whose clear
    /// workflow is still unresolved.
    #[test]
    fn in_place_resume_keeps_a_recorded_pause_row_single() {
        let slow_release = |config: &mut crate::runtime::HostConfig<TestExtension>| {
            config.runtime_factory = crate::runtime::test_kit::scripted(config)
                .with_source_release_delay(Duration::from_millis(500));
        };
        with_configured_tree(0, slow_release, |host, runtime, actions, _| {
            spawn_child(runtime);
            actions.send(UserAction::Interrupt).unwrap();
            let _ = host.next_prompt(None).unwrap();
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                1
            );
            let paused = host.config.session.id.clone();
            actions.send(UserAction::ClearSession).unwrap();
            let _ = host.next_prompt(None).unwrap();
            assert_ne!(host.config.session.id, paused, "clear did not switch");
            let _ = host.next_prompt(None).unwrap();
            actions
                .send(UserAction::ResumeSession {
                    id: Some(paused.clone()),
                })
                .unwrap();
            let _ = host.next_prompt(None).unwrap();
            assert_eq!(host.config.session.id, paused, "resume did not switch");
            assert_eq!(
                host.config.session.plakous_locator,
                Some(host.opened.locator())
            );
            assert!(host.config.session.child_wake_suppressed);
            let _ = host.next_prompt(None).unwrap();
            assert_eq!(
                pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                1,
                "in-place resume wrote a second pause row"
            );
        });
    }

    /// The injection the attachment release tests below run under: `on_release`
    /// runs on every source attachment release's thread after runtime
    /// release, before the host's own observer.
    fn release_hook(
        config: &mut crate::runtime::HostConfig<TestExtension>,
        on_release: impl Fn() + Send + Sync + 'static,
    ) {
        let injection = crate::runtime::test_kit::scripted(config);
        let observe = Arc::clone(&injection.observe);
        config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
            runtime: Arc::clone(&injection.runtime),
            rebind_target: injection.rebind_target.clone(),
            observe: Arc::new(move |workflow, boundary| {
                if workflow == crate::runtime::HostWorkflow::SourceRelease {
                    on_release();
                }
                observe(workflow, boundary);
            }),
            child_runtime: injection.child_runtime.clone(),
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
    }

    /// Only the first release (the clear's, of the source) runs
    /// `on_release`; the resume's own release of the clear target
    /// finishes normally, so teardown stays short and clean.
    fn first_release_hook(
        config: &mut crate::runtime::HostConfig<TestExtension>,
        on_release: impl Fn() + Send + Sync + 'static,
    ) {
        let first = std::sync::atomic::AtomicBool::new(true);
        release_hook(config, move || {
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                on_release();
            }
        });
    }

    /// Clears off the paused session, then resumes back onto it in place.
    /// Returns the source id and how long the resume step held the host.
    fn clear_then_resume_back(
        host: &mut HostLoop<'_, TestExtension>,
        runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
        actions: &UserActionSender<TestExtension>,
    ) -> (String, Duration) {
        spawn_child(runtime);
        actions.send(UserAction::Interrupt).unwrap();
        let _ = host.next_prompt(None).unwrap();
        let source = host.config.session.id.clone();
        actions.send(UserAction::ClearSession).unwrap();
        let _ = host.next_prompt(None).unwrap();
        assert_ne!(host.config.session.id, source, "clear did not switch");
        let _ = host.next_prompt(None).unwrap();
        let started = Instant::now();
        resume(host, actions, &source);
        (source, started.elapsed())
    }

    fn resume(
        host: &mut HostLoop<'_, TestExtension>,
        actions: &UserActionSender<TestExtension>,
        id: &str,
    ) {
        actions
            .send(UserAction::ResumeSession {
                id: Some(id.to_owned()),
            })
            .unwrap();
        let _ = host.next_prompt(None).unwrap();
    }

    fn error_rows(observed: &HostEventReceiver<TestExtension>) -> Vec<String> {
        observed
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(notice))
                    if notice.tone == NoticeTone::Error =>
                {
                    Some(notice.text)
                }
                _ => None,
            })
            .collect()
    }

    fn assert_attached_resolved(
        host: &HostLoop<'_, TestExtension>,
        source: &str,
        errors: &[String],
    ) {
        assert_eq!(
            host.config.session.id, source,
            "resume did not switch back onto the source: {errors:?}"
        );
        assert_eq!(
            host.config.session.plakous_locator,
            Some(host.opened.locator())
        );
        let sessions = host.config.state_roots.runtime_sessions().unwrap();
        let projected = host
            .executor
            .block_on(crate::runtime::projection::project_session(
                &sessions,
                &host.config.store,
                &host.config.session,
            ))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(projected.head, host.opened.locator());
    }

    /// Releasing an attachment never competes with an operator intent write.
    #[test]
    fn source_release_does_not_race_an_operator_intent_write() {
        let racing_writer = |config: &mut crate::runtime::HostConfig<TestExtension>| {
            let store = Arc::clone(&config.store);
            let source = config.session.id.clone();
            first_release_hook(config, move || {
                let mut stored = store.load(&source).unwrap();
                store.save(&mut stored).unwrap();
            });
        };
        with_configured_tree(0, racing_writer, |host, runtime, actions, observed| {
            let (source, _) = clear_then_resume_back(host, runtime, actions);
            let errors = error_rows(observed);
            assert!(
                errors.is_empty(),
                "attachment release must not save the source: {errors:?}"
            );
            assert_attached_resolved(host, &source, &errors);
        });
    }

    /// A release-thread panic is reported; switch-back still follows the feed.
    #[test]
    fn a_panicked_source_release_is_reported_and_resume_projects_the_head() {
        let panicking = |config: &mut crate::runtime::HostConfig<TestExtension>| {
            first_release_hook(config, || panic!("injected source release panic"));
        };
        with_configured_tree(0, panicking, |host, runtime, actions, observed| {
            let (source, _) = clear_then_resume_back(host, runtime, actions);
            let errors = error_rows(observed);
            assert!(
                errors
                    .iter()
                    .any(|text| text.contains("source release panicked")),
                "a panicked source attachment release left no notice: {errors:?}"
            );
            assert_attached_resolved(host, &source, &errors);
        });
    }

    /// A release that does not finish holds a switch back onto its
    /// source for `SETTLE_DEADLINE` at most: the resume is refused and the
    /// release stays tracked, so a retry once it has landed attaches
    /// the source from its projected head.
    #[test]
    fn a_resume_onto_a_stuck_source_release_is_refused_within_its_bound() {
        let (release_attachment, held_attachment) = std::sync::mpsc::channel::<()>();
        let held_attachment = std::sync::Mutex::new(held_attachment);
        let stuck = move |config: &mut crate::runtime::HostConfig<TestExtension>| {
            first_release_hook(config, move || {
                let _ = held_attachment
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60));
            });
        };
        with_configured_tree(0, stuck, |host, runtime, actions, observed| {
            let (source, held) = clear_then_resume_back(host, runtime, actions);
            assert!(
                held < crate::runtime::HOST_RELEASE_DEADLINE,
                "the resume held the host worker for {held:?} waiting on the source's attachment release"
            );
            assert_ne!(
                host.config.session.id, source,
                "the resume attached a source whose finalization was still running"
            );
            let errors = error_rows(observed);
            assert!(
                errors
                    .iter()
                    .any(|text| text.contains("still finishing its previous switch")),
                "the refused resume did not say why: {errors:?}"
            );
            drop(release_attachment);
            resume(host, actions, &source);
            assert_attached_resolved(host, &source, &error_rows(observed));
        });
    }

    /// With auto-continue disabled an interrupt has nothing to pause.
    #[test]
    fn interrupt_without_auto_continue_shows_no_pause() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(dir.path());
        config.auto_continue = false;
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events),
            None,
        )
        .unwrap();
        assert!(!host.config.prompt_host_facts().host_wakes);
        actions.send(UserAction::Interrupt).unwrap();
        let _ = host.next_prompt(None).unwrap();
        let seen = snapshots(&observed);
        let rows = pause_rows(&host.config.session, AUTO_CONTINUE_PAUSE_TEXT);
        host.release().unwrap();
        assert!(
            !seen.iter().any(|pending| pending.auto_continue_paused) && rows == 0,
            "auto-continue disabled, yet indicator {seen:?} and {rows} pause rows"
        );
    }

    /// A tree lock held elsewhere does not end the pause episode: the HUD
    /// does not flap and no second row is written.
    ///
    /// Each round the holder takes the tree lock first and only then lets the
    /// host publish, so every publish starts against a held lock. The holder
    /// releases when the publish finishes (a read that does not wait) or after
    /// a short hold (a read that waits), and takes the lock again only after
    /// the publish returned: a holder that re-locked in a tight loop could
    /// starve the host's wait indefinitely on a loaded machine.
    #[test]
    fn contended_tree_lock_neither_flaps_nor_rewrites_the_tree_pause_row() {
        const ROUNDS: usize = 200;
        const HOLD: Duration = Duration::from_millis(1);
        with_tree(0, |host, runtime, _, observed| {
            pause_tree(runtime);
            host.publish_pending_continuation();
            assert_eq!(pause_rows(&host.config.session, TREE_PAUSE_TEXT), 1);
            let _ = snapshots(observed);
            let (held, held_rx) = std::sync::mpsc::channel::<()>();
            let (published, published_rx) = std::sync::mpsc::channel::<()>();
            std::thread::scope(|scope| {
                // Owned here, so a panicking side drops its end and the other
                // side fails instead of waiting forever.
                let (published, held_rx) = (published, held_rx);
                scope.spawn(move || {
                    for _ in 0..ROUNDS {
                        let finished_under_hold = runtime
                            .read(|_| {
                                held.send(()).unwrap();
                                published_rx.recv_timeout(HOLD).is_ok()
                            })
                            .unwrap();
                        if !finished_under_hold {
                            published_rx.recv().unwrap();
                        }
                    }
                });
                for _ in 0..ROUNDS {
                    held_rx.recv().unwrap();
                    host.publish_pending_continuation();
                    published.send(()).unwrap();
                }
            });
            let flaps = snapshots(observed)
                .iter()
                .filter(|pending| !pending.tree_paused)
                .count();
            assert_eq!(
                (pause_rows(&host.config.session, TREE_PAUSE_TEXT), flaps),
                (1, 0),
                "(pause rows, unpaused snapshots) under tree lock contention"
            );
        });
    }
}
