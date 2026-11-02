//! Live, coalesced outage notices independent of blocked publication callers.

use std::sync::Arc;
use std::time::Duration;

use plakous::agent::Agent;

use crate::session_store::SessionRecordStore;
use crate::sessions::{NoticeScope, TranscriptRow};

pub(super) struct StorageNotices(crate::TaskHandle<()>);

impl StorageNotices {
    pub(super) fn start<P: plakous_host_protocol::ProtocolExtension>(
        tasks: &crate::TaskSet,
        agent: Agent,
        store: Arc<dyn SessionRecordStore>,
        events: super::Sender<P>,
    ) -> Self {
        Self::observe(tasks, store, events, move || agent.storage_unavailable())
    }

    fn observe<P: plakous_host_protocol::ProtocolExtension>(
        tasks: &crate::TaskSet,
        store: Arc<dyn SessionRecordStore>,
        events: super::Sender<P>,
        probe: impl Fn() -> bool + Send + 'static,
    ) -> Self {
        Self(tasks.spawn(async move {
            let mut waiting = false;
            loop {
                let unavailable = probe() || store.storage_unavailable();
                if unavailable != waiting {
                    waiting = unavailable;
                    let message = if waiting {
                        format!(
                            "storage unavailable · {}; existing work retained; retrying writes every 5 s",
                            store.storage_outage_notice(),
                        )
                    } else {
                        "storage writes resumed; retained work continues".to_owned()
                    };
                    // Settings are live-only: neither export documents nor
                    // provider-bound history receives the volume's location.
                    let _ = events.notice(TranscriptRow::info(NoticeScope::Settings, message));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }))
    }
}

impl Drop for StorageNotices {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Sender, test_kit::TestExtension};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test(start_paused = true)]
    async fn outage_notices_are_coalesced_live_only_and_clear_after_recovery() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let full = Arc::new(AtomicBool::new(true));
        let probe = full.clone();
        let _notices = StorageNotices::observe(
            &crate::TaskSet::new(),
            Arc::new(crate::session_store::MemoryStore::new()),
            events.clone(),
            move || probe.load(Ordering::Acquire),
        );
        tokio::task::yield_now().await;
        let first = rx.try_recv().expect("outage notice");
        assert!(
            matches!(first, crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(ref notice)) if notice.scope == NoticeScope::Settings && notice.text.contains("storage unavailable"))
        );
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "one notice for the entire outage");
        full.store(false, Ordering::Release);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        let recovered = rx.try_recv().expect("recovery notice");
        assert!(
            matches!(recovered, crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(ref notice)) if notice.text.contains("storage writes resumed"))
        );
        assert!(events.take_notice_intents().is_empty());
    }
}
