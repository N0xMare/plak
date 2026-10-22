//! Real hosted execution with Core and an independent driver extension.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]

use super::test_kit::{ProbeIntent, ProbeObservation, TestExtension};
use super::test_support::fresh_injected_attached_host_for;
use super::{
    HostConfig, HostEvent, OneShot, OneShotOutcome, ScriptedRuntimeFactory, Sender, SurfaceEffect,
    SurfaceFacts, SurfaceHandler, SurfacePhase, SurfaceRefusal, UserAction,
};
use plakous_host_protocol::views::{ProviderDef, ProviderSection};
use plakous_host_protocol::{Core, ProtocolExtension};
use std::sync::{Arc, Mutex};

struct ProbeSurface(Arc<Mutex<Vec<(SurfacePhase, bool)>>>);
impl SurfaceHandler<TestExtension> for ProbeSurface {
    fn needs_usage(&self, _: &ProbeIntent) -> bool {
        true
    }
    fn dispatch(
        &self,
        _: ProbeIntent,
        facts: SurfaceFacts<'_>,
        events: &Sender<TestExtension>,
    ) -> SurfaceEffect {
        assert_eq!(facts.queue_len, 0);
        self.0
            .lock()
            .unwrap()
            .push((facts.phase, facts.attached_one_shot));
        events.send(HostEvent::Extension(ProbeObservation::UsageObserved));
        SurfaceEffect::default()
    }
    fn publish_template_readiness(&self, _: &[ProviderSection], _: &Sender<TestExtension>) {}
    fn refresh_readiness(&self) -> Result<(Vec<ProviderDef>, Vec<ProviderSection>), String> {
        Ok((Vec::new(), Vec::new()))
    }
}

fn gated<P: ProtocolExtension>(root: &std::path::Path) -> (HostConfig<P>, std::path::PathBuf) {
    let (mut config, _) = fresh_injected_attached_host_for::<P>(root);
    let release = root.join("release");
    let controller =
        plakous_offline_crash_harness::BoundaryController::quiet_gated_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::ProviderOutcomeOne,
            root.join("events.jsonl"),
            release.clone(),
        )
        .unwrap();
    let workspace = config.cwd.clone();
    let binding = plakous_offline_crash_harness::text_runtime_binding(
        &workspace,
        controller.clone(),
        "prefix ",
        "prefix complete",
    )
    .unwrap();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        rebind_target: binding.target_binding(),
        runtime: Arc::new(move |_| {
            plakous_offline_crash_harness::text_runtime_binding(
                &workspace,
                controller.clone(),
                "prefix ",
                "prefix complete",
            )
            .map_err(|e| e.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    });
    (config, release)
}

#[test]
fn core_without_an_extension_runs_the_production_one_shot() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, release) = gated::<Core>(root.path());
    config.surface = None;
    let result = OneShot::run(config, "core turn".into(), |event, _| {
        if matches!(event, Some(HostEvent::AssistantDelta(_))) {
            std::fs::write(&release, "release").unwrap();
        }
    })
    .unwrap();
    assert_eq!(
        result,
        OneShotOutcome::Completed {
            assistant: "prefix complete".into()
        }
    );
}

#[test]
fn independent_extension_is_dispatched_with_busy_facts_inside_one_shot() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, release) = gated::<TestExtension>(root.path());
    let phases = Arc::new(Mutex::new(Vec::new()));
    config.surface = Some(Box::new(ProbeSurface(phases.clone())));
    let store = config.store.clone();
    let id = config.session.id.clone();
    let mut observed = false;
    let result = OneShot::run(
        config,
        "inspect while busy".into(),
        |event, actions| match event {
            Some(HostEvent::AssistantDelta(text)) if text == "prefix " => {
                actions
                    .send(UserAction::Extension(ProbeIntent::InspectUsage))
                    .unwrap();
            }
            Some(HostEvent::Extension(ProbeObservation::UsageObserved)) => {
                observed = true;
                std::fs::write(&release, "release").unwrap();
            }
            _ => {}
        },
    )
    .unwrap();
    assert!(observed);
    assert_eq!(*phases.lock().unwrap(), vec![(SurfacePhase::Busy, true)]);
    assert_eq!(
        result,
        OneShotOutcome::Completed {
            assistant: "prefix complete".into()
        }
    );
    assert_eq!(
        store
            .load(&id)
            .unwrap()
            .transcript
            .iter()
            .filter(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "prefix complete"))
            .count(),
        1
    );
}

#[test]
fn unhandled_independent_extension_is_refused_without_losing_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, release) = gated::<TestExtension>(root.path());
    config.surface = None;
    let mut refusal = false;
    let result = OneShot::run(
        config,
        "no extension handler".into(),
        |event, actions| match event {
            Some(HostEvent::AssistantDelta(text)) if text == "prefix " => {
                actions
                    .send(UserAction::Extension(ProbeIntent::InspectUsage))
                    .unwrap();
            }
            Some(HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
                ),
            )) if note == SurfaceRefusal::NoHandler.note() => {
                refusal = true;
                std::fs::write(&release, "release").unwrap();
            }
            _ => {}
        },
    )
    .unwrap();
    assert!(refusal);
    assert_eq!(
        result,
        OneShotOutcome::Completed {
            assistant: "prefix complete".into()
        }
    );
}

// One total observation budget allows quiet work without resetting the deadline
// on status traffic. Cleanup has a separate bound, below the nextest timeout.
const INTERACTIVE_OBSERVATION_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
const INTERACTIVE_CLEANUP_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

struct InteractiveWorker {
    worker: Option<super::startup::HostWorker<TestExtension>>,
    release: std::path::PathBuf,
    prefix_observed: bool,
}

impl InteractiveWorker {
    fn settle(&mut self) -> Result<(), &'static str> {
        let Some((actions, _events, worker)) = self.worker.take() else {
            return Ok(());
        };
        // Release even when no prefix was consumed; Quit also cancels an active
        // turn. Dropping the sender closes the idle path if Quit cannot be sent.
        let released = std::fs::write(&self.release, "release").is_ok();
        let _ = actions.send(UserAction::Quit);
        drop(actions);
        let deadline = std::time::Instant::now() + INTERACTIVE_CLEANUP_BUDGET;
        while !worker.is_finished() {
            if std::time::Instant::now() >= deadline {
                return Err("interactive worker cleanup did not join before deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        worker
            .join()
            .map_err(|_| "interactive worker panicked")?
            .map_err(|_| "interactive worker returned an error")?;
        if !released {
            return Err("interactive provider gate could not be released");
        }
        Ok(())
    }

    fn observe(&mut self, force_timeout_after_prefix: bool) -> Result<bool, &'static str> {
        let (actions, events, _) = self.worker.as_ref().ok_or("worker already settled")?;
        actions
            .send(UserAction::Extension(ProbeIntent::InspectUsage))
            .map_err(|_| "extension send failed")?;
        actions
            .send(UserAction::Prompt("interactive turn".into()))
            .map_err(|_| "prompt send failed")?;
        let mut deadline = std::time::Instant::now() + INTERACTIVE_OBSERVATION_BUDGET;
        let mut observed = false;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err("interactive observation deadline elapsed");
            }
            let event = events
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        "interactive observation deadline elapsed"
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        "interactive observations disconnected"
                    }
                })?;
            match event {
                HostEvent::Extension(ProbeObservation::UsageObserved) => observed = true,
                HostEvent::AssistantDelta(text) if text == "prefix " => {
                    self.prefix_observed = true;
                    if force_timeout_after_prefix {
                        // Deterministically enter the real timeout path while the
                        // provider gate is held, without sleeping or racing a timer.
                        deadline = std::time::Instant::now();
                    } else {
                        std::fs::write(&self.release, "release")
                            .map_err(|_| "interactive provider release failed")?;
                    }
                }
                HostEvent::AssistantDone => return Ok(observed && self.prefix_observed),
                _ => {}
            }
        }
    }
}

impl Drop for InteractiveWorker {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

#[test]
fn interactive_observation_timeout_releases_gate_and_joins_without_masking_failure() {
    let root = tempfile::tempdir().unwrap();
    let (config, release) = gated::<TestExtension>(root.path());
    let mut worker = InteractiveWorker {
        worker: Some(super::spawn_host_worker(config).unwrap()),
        release,
        prefix_observed: false,
    };
    let observation = worker.observe(true);
    assert!(
        !worker.release.exists(),
        "forced timeout must retain the held gate"
    );
    let cleanup = worker.settle();
    assert!(
        worker.prefix_observed,
        "forced timeout must follow the provider prefix"
    );
    assert_eq!(observation, Err("interactive observation deadline elapsed"));
    assert_eq!(cleanup, Ok(()));
    assert!(worker.worker.is_none());
    assert!(worker.release.exists());
}

#[test]
fn independent_extension_uses_the_interactive_host_and_bounded_observations() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, release) = gated::<TestExtension>(root.path());
    let phases = Arc::new(Mutex::new(Vec::new()));
    config.surface = Some(Box::new(ProbeSurface(phases.clone())));
    let store = config.store.clone();
    let id = config.session.id.clone();
    let mut worker = InteractiveWorker {
        worker: Some(super::spawn_host_worker(config).unwrap()),
        release,
        prefix_observed: false,
    };
    let observation = worker.observe(false);
    let cleanup = worker.settle();
    assert_eq!(
        observation,
        Ok(true),
        "required observations were not completed; cleanup={cleanup:?}"
    );
    assert_eq!(cleanup, Ok(()));
    assert_eq!(*phases.lock().unwrap(), vec![(SurfacePhase::Idle, false)]);
    assert_eq!(
        store
            .load(&id)
            .unwrap()
            .transcript
            .iter()
            .filter(|line| matches!(line, plakous_host_protocol::views::transcript::TranscriptRow::Assistant { text } if text == "prefix complete"))
            .count(),
        1
    );
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let publisher = Sender::over(sender);
    for _ in 0..10_000 {
        assert!(publisher.send(HostEvent::Extension(ProbeObservation::UsageObserved)));
    }
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        vec![HostEvent::Extension(ProbeObservation::UsageObserved)]
    );
}

#[test]
fn idle_one_shot_configuration_publishes_usage_then_dispatches_independent_extension() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host_for::<TestExtension>(root.path());
    config.attached_one_shot = true;
    let facts = Arc::new(Mutex::new(Vec::new()));
    config.surface = Some(Box::new(ProbeSurface(facts.clone())));
    let (actions, events, worker) = super::spawn_host_worker(config).unwrap();
    // Discard startup projections so only this intent can satisfy the ordering assertion.
    let _ = events.try_iter().collect::<Vec<_>>();
    actions
        .send(UserAction::Extension(ProbeIntent::InspectUsage))
        .unwrap();
    let mut usage_observed = false;
    let mut extension_observed = false;
    while let Ok(event) = events.recv_timeout(std::time::Duration::from_secs(5)) {
        match event {
            HostEvent::Usage { .. } => usage_observed = true,
            HostEvent::Extension(ProbeObservation::UsageObserved) => {
                assert!(usage_observed, "usage must precede driver dispatch");
                extension_observed = true;
                break;
            }
            _ => {}
        }
    }
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
    assert!(extension_observed);
    assert_eq!(*facts.lock().unwrap(), vec![(SurfacePhase::Idle, true)]);
}
