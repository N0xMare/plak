//! A terminal projector pass that runs out of its bound leaves the idle
//! root's record a turn behind its attached head (spec 02 §2.6). Fork,
//! profile switch and clear check their exact source against views projected
//! to the feed's tail, so they still start from the attached head.

#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{HostStep, RootActor};
use crate::interrupt::InterruptWatch;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::runtime::{HostBoundary, HostConfig, HostWorkflow, RuntimeRole};
use crate::sessions::SessionRecord;
use plakous::sessions::SessionLocator;

/// Holds projector passes while armed. A pass's first store call reads the
/// session's inherited submissions, so the live task's terminal settle waits
/// here and the host publishes the terminal past its bound.
#[derive(Default)]
struct HeldPasses {
    armed: AtomicBool,
    held: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
}

impl HeldPasses {
    fn release(&self) {
        self.armed.store(false, Ordering::SeqCst);
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl StoreFaults for HeldPasses {
    fn before(&self, call: StoreCall, _session: Option<&SessionRecord>) -> Result<(), String> {
        if call == StoreCall::LoadInheritedSubmissions && self.armed.load(Ordering::SeqCst) {
            self.held.store(true, Ordering::SeqCst);
            let released = self.released.lock().unwrap();
            // Bounded, so a pass the host itself awaited cannot hang the test.
            drop(
                self.release
                    .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                    .unwrap(),
            );
        }
        Ok(())
    }
}

/// Opens a root and runs two turns; the second's terminal projector pass is
/// held past its bound and released once the host is idle. Runs `test` on the
/// idle root, whose record's locator now lags its attached head, with that
/// attached head.
fn idle_behind_its_head(
    root: &std::path::Path,
    configure: impl FnOnce(&mut HostConfig<TestExtension>),
    test: impl FnOnce(&mut RootActor<'_, TestExtension>, SessionLocator),
) {
    let (mut config, _) = fresh_injected_attached_host(root);
    configure(&mut config);
    let store = Arc::new(FaultStore::new(config.store.clone(), HeldPasses::default()));
    let passes = Arc::clone(&store.faults);
    config.store = store;
    let arm_at_terminal = Arc::new(AtomicBool::new(false));
    let (arm, faults) = (Arc::clone(&arm_at_terminal), Arc::clone(&passes));
    crate::runtime::test_kit::scripted_mut(&mut config).observe =
        Arc::new(move |workflow, boundary| {
            if workflow == HostWorkflow::RootDelivery
                && boundary == HostBoundary::TerminalReady
                && arm.load(Ordering::SeqCst)
            {
                faults.armed.store(true, Ordering::SeqCst);
            }
        });
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(
        config,
        &receiver,
        crate::runtime::Sender::over(events),
        None,
    )
    .unwrap();
    let turn = |host: &mut RootActor<'_, TestExtension>, text: &str| {
        actions
            .send(crate::surface_protocol::UserAction::Prompt(text.into()))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap()
        else {
            panic!("the prompt was not admitted");
        };
        host.block(async |host| {
            host.run_prompt(text, queued_delivery, user_echoed, None)
                .await
        })
        .unwrap();
    };
    turn(&mut host, "first");
    arm_at_terminal.store(true, Ordering::SeqCst);
    turn(&mut host, "second");
    passes.release();
    assert!(
        passes.held.load(Ordering::SeqCst),
        "the terminal pass was never held"
    );
    let head = host.opened.locator();
    assert_ne!(
        host.role.config.session.plakous_locator,
        Some(head),
        "the record kept up with the attached head; nothing reproduced"
    );
    test(&mut host, head);
    host.block(async |host| host.release().await).unwrap();
}

/// The one workflow request the source session holds, by its source locator.
fn requested_source(host: &RootActor<'_, TestExtension>, source: &str) -> SessionLocator {
    let intents = host.role.config.store.load_intents(source).unwrap();
    assert_eq!(intents.workflow_requests.len(), 1, "{intents:?}");
    intents.workflow_requests[0].source_locator
}

#[test]
fn a_fork_after_a_terminal_pass_ran_out_forks_the_attached_head() {
    let root = tempfile::tempdir().unwrap();
    idle_behind_its_head(
        root.path(),
        |_| {},
        |host, head| {
            let source = host.role.config.session.id.clone();
            host.block(async |host| {
                Box::pin(crate::runtime::fork::handle_fork(
                    InterruptWatch::new(host.role.actions, &host.role.interrupt_state),
                    &mut host.role.config,
                    &mut host.opened,
                    &mut host.role.stderr_buf,
                    &host.role.events,
                    None,
                ))
                .await
            })
            .unwrap();
            assert_ne!(host.role.config.session.id, source, "the fork is attached");
            assert_eq!(requested_source(host, &source), head);
        },
    );
}

#[test]
fn a_clear_after_a_terminal_pass_ran_out_starts_a_new_session() {
    let root = tempfile::tempdir().unwrap();
    idle_behind_its_head(
        root.path(),
        |_| {},
        |host, head| {
            let source = host.role.config.session.id.clone();
            host.block(async |host| {
                crate::runtime::clear_session_rebuild(
                    &mut host.role.config,
                    &mut host.opened,
                    &mut host.role.stderr_buf,
                    &host.role.events,
                    host.role.log.as_ref(),
                )
                .await
            })
            .unwrap();
            assert_ne!(
                host.role.config.session.id, source,
                "the new session is attached"
            );
            assert_eq!(requested_source(host, &source), head);
        },
    );
}

/// The incident: Switch now onto another profile from an idle root whose
/// last terminal pass ran out of its bound.
#[test]
fn a_profile_switch_after_a_terminal_pass_ran_out_switches_from_the_attached_head() {
    let root = tempfile::tempdir().unwrap();
    let evidence = root.path().join("switch-evidence.jsonl");
    idle_behind_its_head(
        root.path(),
        |config| {
            let workspace = config.cwd.clone();
            let controller =
                plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    evidence,
                )
                .unwrap();
            let successor = move || {
                plakous_offline_crash_harness::runtime_binding_for_account(
                    &workspace,
                    controller.clone(),
                    b"successor-account",
                )
                .map_err(|error| error.to_string())
            };
            let injection = crate::runtime::test_kit::scripted_mut(config);
            injection.rebind_target = successor().unwrap().target_binding();
            let primary = injection.runtime.clone();
            injection.runtime = Arc::new(move |role| {
                if role == RuntimeRole::Rebind {
                    successor()
                } else {
                    primary(role)
                }
            });
            config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
                plakous_host_protocol::views::ProviderSection {
                    account: "codex".into(),
                    label: "Codex".into(),
                    ready: true,
                    entries: vec![plakous_host_protocol::views::ModelEntry {
                        id: "offline-loopback-v2".into(),
                        tag: "offline".into(),
                        reasoning: true,
                        effort_options: vec!["medium".into()],
                        extras: Vec::new(),
                        unavailable_reason: None,
                    }],
                },
            ]);
        },
        |host, head| {
            let source = host.role.config.session.id.clone();
            let mut roster = crate::orchestration::RosterInput::unpinned("offline-loopback-v2");
            roster.root.account = "codex".to_owned();
            roster.root.effort = Some("medium".to_owned());
            let rebound = host
                .block(async |host| {
                    crate::runtime::rebind::handle_rebind(
                        InterruptWatch::new(host.role.actions, &host.role.interrupt_state),
                        &mut host.role.config,
                        &mut host.opened,
                        &mut host.role.stderr_buf,
                        &host.role.events,
                        "codex",
                        "offline-loopback-v2",
                        Some("medium"),
                        None,
                        None,
                        None,
                        &roster,
                        Some("mode-3"),
                    )
                    .await
                })
                .unwrap();
            assert_eq!(rebound, crate::runtime::rebind::Rebound::Succeeded);
            assert_ne!(
                host.role.config.session.id, source,
                "the successor is attached"
            );
            assert_eq!(requested_source(host, &source), head);
        },
    );
}
