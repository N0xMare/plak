//! Deduplicated wake decisions and a watchdog independent of wake admission.

use super::super::RootActor;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Diagnostics {
    session: String,
    last: Option<(&'static str, &'static str)>,
    checked: Option<Instant>,
    pending_since: Option<Instant>,
    warned: bool,
    actions: BTreeMap<&'static str, u64>,
    resync_start: [u64; 2],
    pub(super) mail: usize,
    pub(super) exits: usize,
}

impl Diagnostics {
    fn bind(&mut self, session: &str) {
        if self.session != session {
            *self = Self {
                session: session.to_owned(),
                resync_start: crate::host_service::wake_stats::snapshot(),
                ..Self::default()
            };
        }
    }

    pub(super) fn checked(&mut self, session: &str, now: Instant) {
        self.bind(session);
        self.checked = Some(now);
        self.warned = false;
        self.actions.clear();
        self.resync_start = crate::host_service::wake_stats::snapshot();
    }

    fn admitted(&mut self, session: &str) {
        self.bind(session);
        self.last = None;
        self.pending_since = None;
        self.checked = None;
        self.warned = false;
    }

    fn starved(&mut self, now: Instant, enabled: bool) -> bool {
        if !enabled || self.mail + self.exits == 0 {
            self.pending_since = None;
            self.warned = false;
            return false;
        }
        let pending = *self.pending_since.get_or_insert(now);
        let since = self.checked.map_or(pending, |checked| checked.max(pending));
        if !self.warned && now.saturating_duration_since(since) > Duration::from_secs(10) {
            self.warned = true;
            return true;
        }
        false
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    pub(in crate::runtime::host_loop) fn wake_automatic_precedence(&mut self) {
        let now = Instant::now();
        self.role
            .child_wake
            .diagnostics
            .checked(&self.role.config.session.id, now);
        let reason = if self.role.config.state_roots.runner_pending() {
            "placement_pending"
        } else {
            "automatic_precedence"
        };
        self.wake_decision("deferred", reason, now);
    }

    pub(in crate::runtime::host_loop) fn wake_turn_admitted(&mut self) {
        self.role
            .child_wake
            .diagnostics
            .admitted(&self.role.config.session.id);
    }

    pub(in crate::runtime::host_loop) fn wake_received(
        &mut self,
        action: &crate::surface_protocol::UserAction<P>,
    ) {
        use crate::{intent_dispatch::IntentClass, surface_protocol::UserAction};
        let name = match action {
            UserAction::ResyncTranscript {
                scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
            } => "resync_root",
            UserAction::ResyncTranscript { .. } => "resync_agent",
            _ => match IntentClass::of(action) {
                IntentClass::Urgent => "urgent",
                IntentClass::Quit => "quit",
                IntentClass::Stop => "stop",
                IntentClass::IdleOnly { .. } => "idle_only",
                IntentClass::Ordinary => "ordinary",
            },
        };
        let track = &mut self.role.child_wake.diagnostics;
        track.bind(&self.role.config.session.id);
        let count = track.actions.entry(name).or_default();
        *count = count.saturating_add(1);
    }

    pub(in crate::runtime::host_loop) fn wake_watchdog(&mut self, now: Instant) {
        let mail = self.role.pending_track.unread_count();
        let exits = self.opened.agent().map_or(0, |agent| {
            agent
                .jobs()
                .iter()
                .filter(|job| job.status != plakous::tools::JobStatus::Running)
                .count()
        });
        let enabled = self.role.config.prompt_host_facts().host_wakes;
        let track = &mut self.role.child_wake.diagnostics;
        track.bind(&self.role.config.session.id);
        track.mail = mail;
        track.exits = exits;
        if track.starved(now, enabled) {
            let resync = crate::host_service::wake_stats::snapshot();
            tracing::warn!(target: "plak.wake", session = %track.session, unread = mail, exits,
                actions = ?track.actions, host_resync_root = resync[0].saturating_sub(track.resync_start[0]),
                host_resync_agent = resync[1].saturating_sub(track.resync_start[1]), "idle wake starved");
        }
    }

    pub(in crate::runtime::host_loop) fn wake_decision(
        &mut self,
        decision: &'static str,
        reason: &'static str,
        now: Instant,
    ) {
        let quiet_ms = self.role.child_wake.coalescer.quiet_for(now).as_millis();
        let track = &mut self.role.child_wake.diagnostics;
        track.bind(&self.role.config.session.id);
        if decision == "fired" {
            // Time spent driving the admitted turn is not idle starvation.
            track.pending_since = None;
        }
        if track.last.replace((decision, reason)) != Some((decision, reason)) {
            tracing::info!(target: "plak.wake", session = %track.session, decision, reason,
                mail = track.mail, process_exits = track.exits, quiet_ms, "idle wake decision");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline actor assertions")]
mod tests {
    use super::super::tests::{post_child_result, with_host};
    use super::*;

    #[test]
    fn an_admitted_root_turn_resets_the_armed_watchdog_through_the_actor() {
        with_host(1, |host, runtime, _, _| {
            let now = Instant::now();
            host.block(async |host| host.child_wake_prompt(now).await)
                .unwrap();
            let crate::runtime::host_loop::HostStep::Prompt {
                text,
                queued_delivery,
                user_echoed,
            } = host
                .block(async |host| {
                    host.child_wake_prompt(now + super::super::QUIET_WINDOW)
                        .await
                })
                .unwrap()
            else {
                panic!("ready host wake")
            };
            let old = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
            host.publish_pending_continuation();
            host.wake_watchdog(old);
            host.role
                .child_wake
                .diagnostics
                .checked(&host.role.config.session.id, old);
            host.wake_decision("deferred", "automatic_precedence", old);
            assert!(host.role.child_wake.diagnostics.pending_since.is_some());
            host.block(async |host| {
                host.run_prompt(text, queued_delivery, user_echoed, None)
                    .await
            })
            .unwrap();
            assert_eq!(host.role.config.session.root_turn_count, 1);
            assert!(host.role.child_wake.diagnostics.pending_since.is_none());
            assert!(host.role.child_wake.diagnostics.checked.is_none());
            assert!(host.role.child_wake.diagnostics.last.is_none());
            post_child_result(runtime);
            host.publish_pending_continuation();
            host.wake_watchdog(Instant::now());
            assert!(host.role.child_wake.diagnostics.mail > 0);
            assert!(!host.role.child_wake.diagnostics.warned);
        });
    }

    #[test]
    fn earlier_automatic_work_records_a_decision_during_placement_hold() {
        for loss_resume in [false, true] {
            with_host(1, |host, _, actions, _| {
                let registry = plakous::runner::RunnerRegistry::default();
                registry.configure(plakous::runner::ConfiguredRunner {
                    runner_id: "local".into(),
                    names: vec!["local".into()],
                });
                host.role.config.state_roots = host
                    .role
                    .config
                    .state_roots
                    .clone()
                    .with_runner_readiness(registry, "local");
                if loss_resume {
                    host.role.root_loss_resume_due = true;
                } else {
                    host.role.config.continuation = Some("carry on".into());
                }
                let now = Instant::now();
                host.publish_pending_continuation();
                host.wake_watchdog(now);
                host.role.child_wake.diagnostics.pending_since =
                    now.checked_sub(Duration::from_secs(5));
                host.role.last_tree_observed_at = now.checked_sub(Duration::from_secs(1)).unwrap();
                actions
                    .send(crate::surface_protocol::UserAction::ResyncTranscript {
                        scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
                    })
                    .unwrap();
                assert!(matches!(
                    host.block(async |host| host.next_prompt(None).await)
                        .unwrap(),
                    crate::runtime::host_loop::HostStep::Continue
                ));
                assert_eq!(
                    host.role.child_wake.diagnostics.last,
                    Some(("deferred", "placement_pending"))
                );
                host.wake_watchdog(now + Duration::from_secs(6));
                assert!(host.role.child_wake.diagnostics.mail > 0);
                assert!(!host.role.child_wake.diagnostics.warned);
                assert_eq!(
                    host.role
                        .config
                        .store
                        .load_intents(&host.role.config.session.id)
                        .unwrap()
                        .submissions,
                    []
                );
            });
        }
    }

    #[test]
    fn watchdog_warns_once_until_a_decision_runs_or_work_clears() {
        let now = Instant::now();
        let mut track = Diagnostics::default();
        track.checked("session", now);
        track.mail = 1;
        assert!(!track.starved(now, true));
        assert!(!track.starved(now + Duration::from_secs(10), true));
        assert!(track.starved(now + Duration::from_secs(11), true));
        assert!(!track.starved(now + Duration::from_secs(20), true));
        track.checked("session", now + Duration::from_secs(20));
        assert!(!track.starved(now + Duration::from_secs(21), true));
        assert!(track.starved(now + Duration::from_secs(31), true));
        track.mail = 0;
        assert!(!track.starved(now + Duration::from_secs(40), true));
        track.exits = 1;
        assert!(!track.starved(now + Duration::from_secs(41), true));
        assert!(track.starved(now + Duration::from_secs(52), true));
    }

    #[test]
    fn a_long_admitted_turn_is_not_idle_starvation() {
        let now = Instant::now();
        let mut track = Diagnostics::default();
        track.checked("session", now);
        track.mail = 1;
        assert!(!track.starved(now, true));
        track.admitted("session");
        assert!(!track.starved(now + Duration::from_secs(60), true));
        assert!(!track.starved(now + Duration::from_secs(70), true));
        assert!(track.starved(now + Duration::from_secs(71), true));
    }
}
