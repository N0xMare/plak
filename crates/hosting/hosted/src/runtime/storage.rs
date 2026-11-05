//! Live, coalesced outage notices independent of blocked publication callers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::Agent;

use crate::session_store::SessionRecordStore;
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::storage_pressure::{FillWarnings, StoreFill};

pub(super) struct StorageNotices(crate::TaskHandle<()>);

impl StorageNotices {
    /// Also warns once at each fill step of the shared session store and
    /// keeps the session facts' fill current (spec 02 §2.2).
    pub(super) fn start<P: plakous_host_protocol::ProtocolExtension>(
        tasks: &crate::TaskSet,
        agent: Agent,
        store: Arc<dyn SessionRecordStore>,
        events: super::Sender<P>,
        roots: crate::agent::ProductStateRoots,
    ) -> Self {
        let runtime_database = roots.runtime_database().to_path_buf();
        Self::observe(
            tasks,
            store,
            events,
            move || agent.storage_unavailable(),
            runtime_database,
            move || {
                let roots = roots.clone();
                async move { roots.read_store_fill().await }
            },
        )
    }

    fn observe<P, F>(
        tasks: &crate::TaskSet,
        store: Arc<dyn SessionRecordStore>,
        events: super::Sender<P>,
        probe: impl Fn() -> bool + Send + 'static,
        runtime_database: PathBuf,
        mut fill: impl FnMut() -> F + Send + 'static,
    ) -> Self
    where
        P: plakous_host_protocol::ProtocolExtension,
        F: Future<Output = Option<StoreFill>> + Send,
    {
        Self(tasks.spawn(async move {
            let mut waiting = false;
            let mut warnings = FillWarnings::default();
            // The fill the session facts last showed; `None` before the first
            // reading, so that reading republishes them once.
            let mut shown = None;
            loop {
                let reading = fill().await;
                if warnings.observe(reading.as_ref()).is_some()
                    && let Some(reading) = reading
                {
                    let _ = events.notice(
                        TranscriptRow::warning(NoticeScope::Settings, reading.notice(&runtime_database))
                            .with_remedy(
                                plakous_host_protocol::notice_remedy::NoticeRemedy::ReclaimRetainedHistory {},
                            ),
                    );
                }
                if let Some(reading) = reading {
                    let percent = reading.shown_percent();
                    if shown != Some(percent) {
                        shown = Some(percent);
                        events.republish_store_fill(percent);
                    }
                }
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
            PathBuf::from("/state/runtime.sqlite3"),
            || std::future::ready(None),
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

    #[tokio::test(start_paused = true)]
    async fn store_fill_warnings_are_live_name_the_store_and_carry_the_reclaim_remedy() {
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        assert!(
            events.send(crate::surface_protocol::ContentEvent::SessionInfo(
                plakous_host_protocol::views::presentation::SessionInfoView::default(),
            ))
        );
        let _ = rx.try_recv().expect("the session facts were published");
        let used = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reading = used.clone();
        let _notices = StorageNotices::observe(
            &crate::TaskSet::new(),
            Arc::new(crate::session_store::MemoryStore::new()),
            events.clone(),
            || false,
            PathBuf::from("/state/runtime.sqlite3"),
            move || {
                std::future::ready(Some(crate::storage_pressure::test_fill(
                    reading.load(Ordering::Acquire),
                )))
            },
        );
        let mut warned = Vec::new();
        let mut shown = Vec::new();
        // The last reading is after `plak session gc`: the HUD clears.
        for percent in [10, 71, 72, 86, 96, 97, 10] {
            used.store(percent * 10, Ordering::Release);
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            while let Ok(event) = rx.try_recv() {
                match event {
                    crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(notice)) => {
                        assert_eq!(notice.scope, NoticeScope::Settings);
                        assert!(
                            notice.text.contains("/state/runtime.sqlite3"),
                            "{}",
                            notice.text
                        );
                        assert_eq!(
                            notice.remedy,
                            Some(plakous_host_protocol::notice_remedy::NoticeRemedy::ReclaimRetainedHistory {})
                        );
                        warned.push(notice.text);
                    }
                    crate::surface_protocol::HostEvent::SessionInfo(view) => {
                        shown.push(view.session_store_percent);
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(warned.len(), 3, "{warned:?}");
        assert!(warned[0].contains("is 71% full"), "{warned:?}");
        assert!(warned[1].contains("is 86% full"), "{warned:?}");
        assert!(warned[2].contains("is 96% full"), "{warned:?}");
        assert_eq!(
            shown,
            [Some(71), Some(72), Some(86), Some(96), Some(97), None],
            "below 70% the facts carry no fill"
        );
        assert!(events.take_notice_intents().is_empty());
    }
}
