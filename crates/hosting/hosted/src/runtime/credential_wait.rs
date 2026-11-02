//! Idle session-workflow credential notices, without a root-turn subscription.

use crate::sessions::{NoticeScope, TranscriptRow};
use plakous::agent::{Agent, CredentialWait};
use std::sync::{Arc, Mutex};

pub(super) struct CredentialNotices<P: plakous_host_protocol::ProtocolExtension> {
    task: tokio::task::JoinHandle<()>,
    state: Arc<Mutex<Option<bool>>>,
    events: super::Sender<P>,
}

impl<P: plakous_host_protocol::ProtocolExtension> CredentialNotices<P> {
    pub(super) fn start(
        tasks: &crate::TaskSet,
        agent: Agent,
        account: String,
        events: super::Sender<P>,
    ) -> Self {
        Self::observe(
            tasks,
            move || {
                matches!(
                    agent.credential_wait(),
                    Some(CredentialWait::SessionWorkflow)
                )
            },
            account,
            events,
        )
    }

    fn observe(
        tasks: &crate::TaskSet,
        probe: impl Fn() -> bool + Send + 'static,
        account: String,
        events: super::Sender<P>,
    ) -> Self {
        let state = Arc::new(Mutex::new(Some(false)));
        let observed = Arc::clone(&state);
        let sender = events.clone();
        let task = tasks.spawn(async move {
            loop {
                let current = probe();
                {
                    let mut state = observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(waiting) = state.as_mut() else {
                        return;
                    };
                    if current != *waiting {
                        *waiting = current;
                        if current {
                            let _ = sender.notice(
                                TranscriptRow::info(NoticeScope::Settings, "waiting for login")
                                    .with_remedy(
                                        plakous_host_protocol::notice_remedy::NoticeRemedy::Login {
                                            account:
                                                plakous_host_protocol::notice_remedy::AccountTarget::from_account(
                                                    &account,
                                                ),
                                        },
                                    ),
                            );
                        }
                        let _ = sender.send(super::ContentEvent::CredentialWait {
                            account: current.then(|| account.clone()),
                        });
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        Self {
            task,
            state,
            events,
        }
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> Drop for CredentialNotices<P> {
    fn drop(&mut self) {
        let was_waiting = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.task.abort();
        if was_waiting == Some(true) {
            let _ = self
                .events
                .send(super::ContentEvent::CredentialWait { account: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn workflow_login_wait_has_one_live_notice_and_clears_without_persisting_state() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<
            crate::runtime::test_kit::TestExtension,
        >();
        let events = crate::runtime::Sender::over(tx);
        let flag = Arc::new(AtomicBool::new(true));
        let probe = Arc::clone(&flag);
        let _notices = CredentialNotices::observe(
            &crate::TaskSet::new(),
            move || probe.load(Ordering::Acquire),
            "claude".to_owned(),
            events.clone(),
        );
        tokio::task::yield_now().await;
        let initial: Vec<_> = rx.try_iter().collect();
        let super::super::HostEvent::Row(TranscriptRow::Notice(notice)) = &initial[0] else {
            panic!("login wait did not open with a notice");
        };
        assert_eq!(notice.scope, NoticeScope::Settings);
        assert_eq!(notice.text, "waiting for login");
        assert_eq!(
            notice.remedy,
            Some(plakous_host_protocol::notice_remedy::NoticeRemedy::Login {
                account: plakous_host_protocol::notice_remedy::AccountTarget::Claude,
            })
        );
        assert!(
            matches!(&initial[1], super::super::HostEvent::CredentialWait { account: Some(account) } if account == "claude")
        );
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        assert!(events.take_notice_intents().is_empty());
        flag.store(false, Ordering::Release);
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            rx.try_recv(),
            Ok(super::super::HostEvent::CredentialWait { account: None })
        ));
    }
}
