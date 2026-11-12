//! Background wake admission must not depend on a quiet control mailbox.

use super::*;
use plakous_host_protocol::views::transcript::TranscriptScope;

#[test]
fn escape_arriving_after_an_idle_read_stays_deferred() {
    with_host(1, |host, _, actions, _| {
        host.role.config.continuation = Some("carry on".into());
        host.role.last_tree_observed_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        let controls = actions.clone();
        crate::runtime::test_seams::arm_after_idle_read(
            &host.role.config.session.id,
            Box::new(move || controls.send(UserAction::Interrupt).unwrap()),
        );
        actions
            .send(UserAction::ResyncTranscript {
                scope: TranscriptScope::Root,
            })
            .unwrap();
        assert!(matches!(
            host.block(async |host| host.next_prompt(None).await)
                .unwrap(),
            HostStep::Continue
        ));
        assert!(host.role.interrupt_state.borrow().has_wake_preemptor());
        assert_eq!(host.role.config.continuation.as_deref(), Some("carry on"));
        assert_eq!(submissions(host), []);
    });
}

#[test]
fn continuation_yields_to_a_prompt_queued_behind_a_read() {
    with_host(0, |host, _, actions, _| {
        host.role.config.continuation = Some("carry on".into());
        host.role.last_tree_observed_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        actions
            .send(UserAction::ResyncTranscript {
                scope: TranscriptScope::Root,
            })
            .unwrap();
        actions
            .send(UserAction::Prompt("human prompt".into()))
            .unwrap();
        let text = host.block(async |host| {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let HostStep::Prompt { text, .. } = host.next_prompt(None).await.unwrap() {
                        break text;
                    }
                }
            })
            .await
            .unwrap()
        });
        assert_eq!(text, "human prompt");
    });
}

#[test]
fn an_extension_queued_behind_a_read_preempts_automatic_work() {
    with_host(1, |host, _, actions, _| {
        host.role.config.continuation = Some("carry on".into());
        host.role.last_tree_observed_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        actions
            .send(UserAction::ResyncTranscript {
                scope: TranscriptScope::Root,
            })
            .unwrap();
        actions
            .send(UserAction::Extension(
                crate::runtime::test_kit::ProbeIntent::InspectUsage,
            ))
            .unwrap();
        assert!(matches!(
            host.block(async |host| host.next_prompt(None).await)
                .unwrap(),
            HostStep::Continue
        ));
        assert!(host.role.interrupt_state.borrow().has_wake_preemptor());
        assert_eq!(host.role.config.continuation.as_deref(), Some("carry on"));
        assert_eq!(submissions(host).len(), 0);
        host.role.last_tree_observed_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            host.block(async |host| host.next_prompt(None).await)
                .unwrap(),
            HostStep::Continue
        ));
        assert_eq!(submissions(host).len(), 0);
    });
}

fn with_resync_traffic(test: impl FnOnce(), actions: &UserActionSender<TestExtension>) {
    std::thread::scope(|scope| {
        let controls = actions.clone();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        scope.spawn(move || {
            let mut ready = Some(ready);
            loop {
                if controls
                    .send(UserAction::ResyncTranscript {
                        scope: TranscriptScope::Root,
                    })
                    .is_err()
                {
                    break;
                }
                if let Some(ready) = ready.take() {
                    ready.send(()).unwrap();
                }
                if stopped.recv_timeout(Duration::from_millis(50))
                    != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                {
                    break;
                }
            }
        });
        started.recv().unwrap();
        test();
        drop(stop);
    });
}

fn admit_under_resync_traffic(
    host: &mut RootActor<'_, TestExtension>,
    controls: &UserActionSender<TestExtension>,
) {
    with_resync_traffic(
        || {
            let step = host.block(async |host| {
                tokio::time::timeout(Duration::from_secs(8), async {
                    loop {
                        let step = host.next_prompt(None).await.unwrap();
                        if matches!(step, HostStep::Prompt { .. }) {
                            break step;
                        }
                    }
                })
                .await
                .expect("ordinary transcript reads starved the idle wake")
            });
            let (text, item, echoed) = take_prompt(step);
            assert!(is_wake_delivery(
                &host.role.config.session,
                item.as_ref().unwrap()
            ));
            host.block(async |host| host.run_prompt(text, item, echoed, None).await)
                .unwrap();
            host.block(async |host| {
                let until = Instant::now() + QUIET_WINDOW + Duration::from_millis(300);
                while Instant::now() < until {
                    assert!(matches!(
                        host.next_prompt(None).await.unwrap(),
                        HostStep::Continue
                    ));
                }
            });
            assert_eq!(
                submissions(host)
                    .iter()
                    .filter(|submission| submission.kind
                        == crate::sessions::projector::SubmissionKind::HostWake)
                    .count(),
                1
            );
        },
        controls,
    );
}

#[test]
fn unread_child_result_wakes_once_under_continuous_resync_reads() {
    with_host(1, |host, _, actions, _| {
        admit_under_resync_traffic(host, actions);
    });
}

#[test]
fn placement_hold_defers_a_root_wake_then_admits_once_under_resync_reads() {
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
            .with_runner_readiness(registry.clone(), "local");
        let now = Instant::now();
        for checked in [now, now + QUIET_WINDOW] {
            actions
                .send(UserAction::ResyncTranscript {
                    scope: TranscriptScope::Root,
                })
                .unwrap();
            assert!(matches!(
                host.block(async |host| host.child_wake_prompt(checked).await)
                    .unwrap(),
                HostStep::Continue
            ));
        }
        assert_eq!(submissions(host).len(), 0);
        assert!(!host.role.config.session.child_wake_suppressed);
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let catalog = Arc::new(
            plakous::runner::WorkspaceCatalog::machine(
                vec![workspace.path().to_owned()],
                home.path(),
                false,
            )
            .unwrap(),
        );
        registry.ready_local(catalog, 1).unwrap();
        admit_under_resync_traffic(host, actions);
    });
}

#[test]
fn process_exit_wakes_once_under_continuous_resync_reads() {
    let jobs = Arc::new(WakeJobFixture::default());
    with_configured_fixture(
        0,
        false,
        |config, _| jobs.configure(config, &[1, 0]),
        |host, _, actions, _| {
            run_user_prompt(host, actions, "start");
            jobs.complete(host, 1);
            admit_under_resync_traffic(host, actions);
        },
    );
}

#[test]
fn deferred_transcript_read_does_not_abandon_a_ready_wake() {
    with_host(1, |host, _, actions, _| {
        let now = Instant::now();
        host.block(async |host| host.child_wake_prompt(now).await)
            .unwrap();
        actions
            .send(UserAction::ResyncTranscript {
                scope: TranscriptScope::Root,
            })
            .unwrap();
        let step = host
            .block(async |host| host.child_wake_prompt(now + QUIET_WINDOW).await)
            .unwrap();
        assert!(
            matches!(step, HostStep::Prompt { .. }),
            "ordinary deferred read abandoned a ready wake"
        );
        assert!(host.role.interrupt_state.borrow().has_deferred());
        let (text, item, echoed) = take_prompt(step);
        host.block(async |host| host.run_prompt(text, item, echoed, None).await)
            .unwrap();
        assert!(!host.role.interrupt_state.borrow().has_deferred());
    });
}
