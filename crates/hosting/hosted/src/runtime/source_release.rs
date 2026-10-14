//! Bounded, tracked release of an attachment left behind by a switch.
//! Source records and workflow consequences are exclusively feed-projected.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use plakous::application::ApplicationExecutor;

use crate::agent::AttachedCodingSession;
use crate::sessions::{NoticeScope, TranscriptRow};

use super::{HOST_RELEASE_DEADLINE, Sender, TestHostInjection};

pub(super) const SETTLE_DEADLINE: Duration = Duration::from_secs(5);
const EXIT_DEADLINE: Duration = Duration::from_secs(20);

/// Outstanding attachment releases, joined before switch-back or host exit.
#[derive(Default)]
pub struct SourceReleases {
    pending: Vec<PendingRelease>,
}

#[must_use = "track the attachment release so switch-back and shutdown can join it"]
pub(super) struct PendingRelease {
    source_id: String,
    done: mpsc::Receiver<()>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Settlement {
    Released,
    StillRunning,
}

impl PendingRelease {
    fn wait(self, deadline: Instant) -> Result<(), Self> {
        match self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self),
        }
    }
}

impl SourceReleases {
    pub(super) fn track(&mut self, release: PendingRelease) {
        let now = Instant::now();
        self.pending = std::mem::take(&mut self.pending)
            .into_iter()
            .filter_map(|pending| pending.wait(now).err())
            .collect();
        self.pending.push(release);
    }

    pub(super) fn settle(&mut self, session_id: &str, waiting: impl FnOnce()) -> Settlement {
        let (settle, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| pending.source_id == session_id);
        self.pending = keep;
        let mut outstanding: Vec<_> = settle
            .into_iter()
            .filter_map(|pending| pending.wait(Instant::now()).err())
            .collect();
        if !outstanding.is_empty() {
            waiting();
        }
        let deadline = Instant::now() + SETTLE_DEADLINE;
        outstanding = outstanding
            .into_iter()
            .filter_map(|pending| pending.wait(deadline).err())
            .collect();
        if outstanding.is_empty() {
            Settlement::Released
        } else {
            self.pending.extend(outstanding);
            Settlement::StillRunning
        }
    }
}

impl Drop for SourceReleases {
    fn drop(&mut self) {
        let deadline = Instant::now() + EXIT_DEADLINE;
        for pending in std::mem::take(&mut self.pending) {
            let _ = pending.wait(deadline);
        }
    }
}

pub(super) fn release_source_attachment<P: plakous_host_protocol::ProtocolExtension>(
    previous: AttachedCodingSession,
    source_id: String,
    events: Sender<P>,
    label: &'static str,
    test_injection: Option<Arc<TestHostInjection>>,
) -> PendingRelease {
    let (done, outcome) = mpsc::sync_channel(1);
    let background_events = events.clone();
    let spawn = thread::Builder::new()
        .name(format!("plak-{label}-release"))
        .spawn(move || {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let released = release(previous);
                #[cfg(any(test, feature = "test-support"))]
                super::startup::observe_test_boundary(
                    test_injection.as_deref(),
                    super::TestHostWorkflow::SourceRelease,
                    super::TestHostBoundary::RuntimeReturned,
                );
                #[cfg(not(any(test, feature = "test-support")))]
                let _ = test_injection;
                released
            }))
            .unwrap_or_else(|payload| {
                Err(format!(
                    "source release panicked: {}",
                    panic_message(payload.as_ref())
                ))
            });
            if let Err(error) = result {
                let _ = background_events.notice(TranscriptRow::error(
                    NoticeScope::Session,
                    format!("{label} target is active; source attachment release failed: {error}"),
                ));
            }
            let _ = done.send(());
        });
    if let Err(error) = spawn {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Session,
            format!("{label} target is active; source attachment release could not be scheduled: {error}"),
        ));
    }
    PendingRelease {
        source_id,
        done: outcome,
    }
}

fn release(previous: AttachedCodingSession) -> Result<(), String> {
    let executor =
        ApplicationExecutor::new().map_err(|_| "release executor unavailable".to_owned())?;
    executor
        .block_on(async { tokio::time::timeout(HOST_RELEASE_DEADLINE, previous.release()).await })
        .map_err(|_| "release join failed".to_owned())?
        .map_err(|_| "release exceeded its deadline".to_owned())?
        .map(|_| ())
        .map_err(|error| format!("release failed: {error}"))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-text panic payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_releases_are_forgotten_without_waiting() {
        let (done, outcome) = mpsc::channel();
        done.send(()).unwrap();
        let mut releases = SourceReleases::default();
        releases.track(PendingRelease {
            source_id: "source".into(),
            done: outcome,
        });
        assert_eq!(
            releases.settle("source", || panic!("already released")),
            Settlement::Released
        );
        assert!(releases.pending.is_empty());
    }

    #[test]
    fn switching_back_joins_only_its_own_attachment() {
        let (done, outcome) = mpsc::channel();
        let (other_done, other_outcome) = mpsc::channel();
        let mut releases = SourceReleases::default();
        releases.track(PendingRelease {
            source_id: "source".into(),
            done: outcome,
        });
        releases.track(PendingRelease {
            source_id: "other".into(),
            done: other_outcome,
        });
        assert_eq!(
            releases.settle("source", || done.send(()).unwrap()),
            Settlement::Released
        );
        assert_eq!(releases.pending.len(), 1);
        assert_eq!(releases.pending[0].source_id, "other");
        other_done.send(()).unwrap();
    }
}
