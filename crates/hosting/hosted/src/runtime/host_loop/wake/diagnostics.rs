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
mod tests {
    use super::*;

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
