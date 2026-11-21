use super::*;
use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
use crate::runtime::surface::{
    AccountsRead, ReadinessRead, SurfaceEffect, SurfaceFacts, SurfaceHandler, SurfacePhase,
};
use crate::runtime::test_kit::ProbeIntent;
use crate::runtime::turn_drive::wake_trace::{MAIL_REPLIES, WAITING_ENGINE, WaitingEngine};
use crate::sessions::{NoticeScope, SessionRecord, TranscriptRow};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};

const NOTICE: &str = "notice written while waiting";
const QUESTION: &str = "Should I keep the earlier result?";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    Mail,
    JobTerminal,
    NoticeIntent,
    StepPause,
    CancelSettlement,
    NoticeWriteRetry,
    TreeNoticeRetry,
    AccountRead,
}

#[derive(Default)]
struct NoticeWrites {
    refused: AtomicBool,
    attempted: AtomicUsize,
    refuse_tree_notices: AtomicBool,
    tree_refusals: AtomicUsize,
}

impl StoreFaults for Arc<NoticeWrites> {
    fn before(&self, call: StoreCall, session: Option<&SessionRecord>) -> Result<(), String> {
        if call == StoreCall::LoadIntents && self.refuse_tree_notices.load(Ordering::Relaxed) {
            self.tree_refusals.fetch_add(1, Ordering::Relaxed);
            return Err("injected notice ID read refusal".into());
        }
        if call == StoreCall::Save
            && session.is_some_and(|session| {
                session
                    .transcript
                    .pending_notices()
                    .iter()
                    .any(|row| row.text() == Some(NOTICE))
            })
        {
            self.attempted.fetch_add(1, Ordering::Relaxed);
            if self.refused.load(Ordering::Relaxed) {
                return Err("injected notice write refusal".into());
            }
        }
        Ok(())
    }
}

struct AccountSurface {
    started: mpsc::Sender<()>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
    replied: mpsc::Sender<()>,
}

impl SurfaceHandler<TestExtension> for AccountSurface {
    fn accounts_read(&self, _: &ProbeIntent, _: &SurfaceFacts<'_>) -> Option<AccountsRead> {
        let release = self.release.lock().unwrap().take().unwrap();
        let started = self.started.clone();
        Some(Box::new(move || {
            started.send(()).unwrap();
            release.recv().map_err(|error| error.to_string())?;
            Ok(Vec::new())
        }))
    }

    fn dispatch(
        &self,
        _: ProbeIntent,
        facts: SurfaceFacts<'_>,
        _: &crate::runtime::Sender<TestExtension>,
    ) -> SurfaceEffect {
        assert!(matches!(facts.phase, SurfacePhase::Busy));
        assert!(matches!(facts.accounts, Some(Ok(rows)) if rows.is_empty()));
        self.replied.send(()).unwrap();
        SurfaceEffect::default()
    }

    fn publish_template_readiness(
        &self,
        _: &[plakous_host_protocol::views::ProviderSection],
        _: &crate::runtime::Sender<TestExtension>,
    ) {
    }

    fn readiness_read(&self) -> ReadinessRead {
        Box::new(|| Ok((Vec::new(), Vec::new())))
    }
}

async fn until(
    case: Wake,
    mut drive: std::pin::Pin<&mut impl Future>,
    mut condition: impl FnMut() -> bool,
) {
    let bound = Instant::now() + Duration::from_secs(10);
    while !condition() {
        tokio::select! {
            biased;
            _ = &mut drive => panic!("{case:?}: root stopped driving before the effect"),
            () = tokio::task::yield_now() => {}
        }
        assert!(
            Instant::now() < bound,
            "{case:?}: wake did not deliver its effect"
        );
    }
}

async fn without_clock<F: Future>(future: F) -> F::Output {
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            () = tokio::task::yield_now() => {}
        }
    }
}

fn notice_present(store: &dyn crate::session_store::SessionRecordStore, id: &str) -> bool {
    store
        .load_intents(id)
        .unwrap()
        .notices
        .iter()
        .any(|notice| notice.row.text() == Some(NOTICE))
}

#[test]
fn independent_sources_wake_a_root_waiting_on_a_child() {
    for case in [
        Wake::Mail,
        Wake::JobTerminal,
        Wake::NoticeIntent,
        Wake::StepPause,
        Wake::CancelSettlement,
        Wake::NoticeWriteRetry,
        Wake::TreeNoticeRetry,
        Wake::AccountRead,
    ] {
        run_case(case);
    }
}

fn run_case(case: Wake) {
    use plakous::tools::{JobStatus, JobView};
    let child = std::cell::OnceCell::new();
    let writes = Arc::new(NoticeWrites::default());
    let (started, reading) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let (replied, dispatched) = mpsc::channel();
    with_configured_fixture(
        0,
        false,
        |config, runtime| {
            let id = runtime
                .mutate_then_emit(false, |tree| {
                    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .map(|child| child.agent_id)
                        .map_err(|error| error.message)
                })
                .block()
                .unwrap();
            configure_wait(config, runtime, std::slice::from_ref(&id), 30_000);
            child.set(id).unwrap();
            config.store = Arc::new(FaultStore::new(config.store.clone(), writes.clone()));
            if case == Wake::AccountRead {
                config.surface = Some(Box::new(AccountSurface {
                    started,
                    release: Mutex::new(Some(released)),
                    replied,
                }));
            }
        },
        |host, runtime, actions, observed| {
            let id = host.role.config.session.id.clone();
            let store = host.role.config.store.clone();
            let events = host.role.events.pinned_to(&id);
            let jobs = host.role.config.state_roots.job_registry().clone();
            let (key, job) = if case == Wake::JobTerminal {
                let agent = host.opened.agent().unwrap();
                let key = (agent.session_id(), agent.branch_id());
                let job = JobView {
                    number: 17,
                    command: "controlled job".into(),
                    started_at: plakous::model::Timestamp::new(1, 0).unwrap(),
                    owner: agent.id().to_string(),
                    elapsed: Duration::ZERO,
                    status: JobStatus::Running,
                    output: String::new(),
                    session_id: [17; 16],
                    owner_generation: 1,
                };
                jobs.record_test_observation(key, job.clone());
                (Some(key), Some(job))
            } else {
                (None, None)
            };
            actions
                .send(UserAction::Prompt("wait for the child".into()))
                .unwrap();
            let (text, item, echoed) = take_prompt(
                host.block(async |host| host.next_prompt(None).await)
                    .unwrap(),
            );
            host.block(async |host| {
                if let Some(live) = host.role.live.as_mut() {
                    live.drain_ticks().await;
                }
            });
            host.role.live = None;
            let clock = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            clock.block_on(async {
                let mail_replies = Rc::new(Cell::new(0));
                let engine = Rc::new(RefCell::new(WaitingEngine::new(case == Wake::CancelSettlement)));
                let drive = MAIL_REPLIES.scope(mail_replies.clone(), host.run_prompt(text, item, echoed, None));
                let mut drive = Box::pin(WAITING_ENGINE.scope(engine.clone(), drive));
                until(case, drive.as_mut(), || observed.try_iter().any(|event| {
                    matches!(event, HostEvent::ToolLine { name, state, .. } if name == "wait_agent" && state == "running")
                })).await;
                tokio::time::pause();
                let now = tokio::time::Instant::now();
                match case {
                    Wake::Mail | Wake::TreeNoticeRetry => {
                        let retry = case == Wake::TreeNoticeRetry;
                        writes.refuse_tree_notices.store(retry, Ordering::Relaxed);
                        let from = child.get().unwrap().clone();
                        let (message, purpose) = if retry {
                            (QUESTION, MessagePurpose::Question)
                        } else {
                            ("I found something during the wait", MessagePurpose::Finding)
                        };
                        without_clock(runtime.mutate_then_emit(false, move |tree| {
                            tree.send_agent_message(&from, &SendAgentMessageRequest {
                                acknowledge_recovery: None,
                                refresh: None,
                                agent_id: "root".into(),
                                message: message.into(),
                                purpose: Some(purpose),
                                priority: None,
                                in_reply_to: None,
                                output_schema_json: None,
                            }, None).map_err(|error| error.message)
                        })).await.unwrap();
                        until(case, drive.as_mut(), || mail_replies.get() > 0).await;
                        if retry {
                            assert!(writes.tree_refusals.load(Ordering::Relaxed) > 0);
                            let question = |event| matches!(
                                event,
                                HostEvent::Row(TranscriptRow::Milestone(row)) if row.text.as_deref() == Some(QUESTION)
                            );
                            assert!(!observed.try_iter().any(question));
                            writes.refuse_tree_notices.store(false, Ordering::Relaxed);
                            tokio::time::advance(Duration::from_millis(251)).await;
                            until(case, drive.as_mut(), || observed.try_iter().any(question)).await;
                        }
                    }
                    Wake::JobTerminal => {
                        let job = JobView { status: JobStatus::Exited(Some(0)), ..job.unwrap() };
                        jobs.record_test_observation(key.unwrap(), job);
                        until(case, drive.as_mut(), || observed.try_iter().any(|event| {
                            matches!(event, HostEvent::JobTerminal { job_id, exit_code: Some(0), .. } if job_id == "p17")
                        })).await;
                    }
                    Wake::NoticeIntent | Wake::NoticeWriteRetry => {
                        let retry = case == Wake::NoticeWriteRetry;
                        writes.refused.store(retry, Ordering::Relaxed);
                        assert!(events.notice(TranscriptRow::info(NoticeScope::Session, NOTICE)));
                        if retry {
                            until(case, drive.as_mut(), || writes.attempted.load(Ordering::Relaxed) == 1).await;
                            writes.refused.store(false, Ordering::Relaxed);
                            tokio::time::advance(Duration::from_millis(249)).await;
                            tokio::select! {
                                biased;
                                _ = &mut drive => panic!("the root is still waiting"),
                                () = tokio::task::yield_now() => {}
                            }
                            assert!(!notice_present(store.as_ref(), &id));
                            tokio::time::advance(Duration::from_millis(2)).await;
                        }
                        until(case, drive.as_mut(), || notice_present(store.as_ref(), &id)).await;
                    }
                    Wake::StepPause => {
                        let pause = events.step_control.pause().unwrap();
                        until(case, drive.as_mut(), || pause.boundary().unwrap().is_some_and(|boundary| boundary.turn.is_some())).await;
                    }
                    Wake::CancelSettlement => {
                        let control = engine.borrow().control.clone().unwrap();
                        assert!(!control.is_settled());
                        // Do not drive the root during the runtime cancel or
                        // supply a tool/terminal event to advance its pass.
                        without_clock(control.request_cancel()).await.unwrap();
                        until(case, drive.as_mut(), || observed.try_iter().any(|event| {
                            matches!(event, HostEvent::QueueSnapshot { drain: plakous_host_protocol::QueueDrainState::RestoredPaused, .. })
                        })).await;
                    }
                    Wake::AccountRead => {
                        actions.send(UserAction::Extension(ProbeIntent::InspectUsage)).unwrap();
                        until(case, drive.as_mut(), || reading.try_recv().is_ok()).await;
                        assert!(dispatched.try_recv().is_err());
                        release.send(()).unwrap();
                        until(case, drive.as_mut(), || dispatched.try_recv().is_ok()).await;
                    }
                }
                if !matches!(case, Wake::NoticeWriteRetry | Wake::TreeNoticeRetry) {
                    assert_eq!(tokio::time::Instant::now(), now, "{case:?} advanced virtual time");
                }
                tokio::time::resume();
                engine.borrow_mut().control = None;
                if case != Wake::CancelSettlement {
                    let child = child.get().unwrap().clone();
                    runtime.mutate_then_emit(false, move |tree| {
                        tree.submit_result(&child, &SubmitResultRequest {
                            turn_token: 1,
                            output_json: r#"{"ok":true}"#.into(),
                        }, None).map_err(|error| error.message)?;
                        tree.record_turn_completed(&child).map_err(|error| error.message)
                    }).await.unwrap();
                }
                events.notice(TranscriptRow::info(NoticeScope::Session, "resume engine observation"));
                tokio::time::timeout(Duration::from_secs(15), drive).await
                    .unwrap_or_else(|_| panic!("{case:?}: root did not finish after restoring runtime observations"))
                    .unwrap();
            });
        },
    );
}
