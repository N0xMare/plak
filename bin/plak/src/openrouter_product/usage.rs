//! One-way sharing: legacy always fetches, console may join or reuse it.

use crate::home::{
    paths::PlakPaths,
    secrets::{self, KeyStatus},
};
use plakous_hosted::{
    account::AccountRef,
    openrouter::{self, OpenRouterKeyUsageProbe},
    subscription_usage::VerticalObservation,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;
use tokio::sync::watch;

use plakous::subscriptions::USAGE_OBSERVATION_FRESHNESS as FRESHNESS;
type Scope = (std::path::PathBuf, AccountRef);
#[derive(Default)]
struct Registry {
    entries: BTreeMap<Scope, Arc<Entry>>,
    untracked: usize,
    last_untracked: Option<Instant>,
}
struct Entry {
    state: Mutex<State>,
    changed: watch::Sender<()>,
}
#[derive(Default)]
struct State {
    active: usize,
    observation: Option<(KeyFence, Instant, VerticalObservation)>,
}
#[derive(Clone, Eq, PartialEq)]
struct KeyFence {
    status: KeyStatus,
    generation: Option<String>,
}
fn fence(paths: &PlakPaths, account: &AccountRef) -> Result<KeyFence, String> {
    Ok(KeyFence {
        status: secrets::key_status(paths, account.as_str())?,
        generation: super::key_usage_fence(paths, account)?,
    })
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}
fn entry(paths: &PlakPaths, account: &AccountRef) -> Option<Arc<Entry>> {
    let mut registry = lock(registry());
    let scope = (paths.home().to_path_buf(), account.clone());
    if let Some(entry) = registry.entries.get(&scope) {
        return Some(Arc::clone(entry));
    }
    registry.entries.retain(|_, entry| {
        let state = lock(&entry.state);
        Arc::strong_count(entry) > 1
            || state.active > 0
            || state
                .observation
                .as_ref()
                .is_some_and(|(_, at, _)| at.elapsed() < FRESHNESS)
    });
    if registry.entries.len() >= 512 {
        return None;
    }
    let entry = Arc::new(Entry {
        state: Mutex::default(),
        changed: watch::channel(()).0,
    });
    registry.entries.insert(scope, Arc::clone(&entry));
    Some(entry)
}
struct Flight(Option<Arc<Entry>>);
impl Flight {
    fn legacy(entry: Option<Arc<Entry>>) -> Self {
        if let Some(entry) = &entry {
            lock(&entry.state).active += 1;
        } else {
            lock(registry()).untracked += 1;
        }
        Self(entry)
    }
    fn publish(
        &self,
        before: &KeyFence,
        after: Option<&KeyFence>,
        dispatched: Option<String>,
        observation: &VerticalObservation,
    ) {
        if Some(before) == after
            && dispatched == before.generation
            && let Some(entry) = &self.0
        {
            lock(&entry.state).observation =
                Some((before.clone(), Instant::now(), observation.clone()));
        }
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        if let Some(entry) = &self.0 {
            lock(&entry.state).active -= 1;
            entry.changed.send_replace(());
        } else {
            let mut registry = lock(registry());
            registry.untracked -= 1;
            registry.last_untracked = Some(Instant::now());
        }
    }
}

pub(crate) fn legacy(paths: &PlakPaths, account: &AccountRef) -> VerticalObservation {
    let credentials = super::ProductOpenRouterCredentials::shared(paths);
    project(openrouter::probe_openrouter_key_usage(
        credentials.as_ref(),
        account,
    ))
}

struct Capture {
    paths: PlakPaths,
    account: AccountRef,
    flight: Flight,
    before: Option<KeyFence>,
    receipt: super::UsageReceipt,
}
fn prepare_capture(
    paths: &PlakPaths,
    account: &AccountRef,
) -> (Arc<dyn openrouter::OpenRouterCredentials>, Capture) {
    let flight = Flight::legacy(entry(paths, account));
    let before = fence(paths, account).ok();
    let (credentials, receipt) = super::usage_credentials(paths);
    (
        credentials,
        Capture {
            paths: paths.clone(),
            account: account.clone(),
            flight,
            before,
            receipt,
        },
    )
}
pub(super) fn capture(
    paths: &PlakPaths,
    account: &AccountRef,
) -> (
    Arc<dyn openrouter::OpenRouterCredentials>,
    Box<dyn openrouter::KeyUsageObserver>,
) {
    let (credentials, capture) = prepare_capture(paths, account);
    (credentials, Box::new(capture))
}
impl Capture {
    fn publish(&self, observation: &VerticalObservation) {
        if let Some(before) = &self.before {
            self.flight.publish(
                before,
                fence(&self.paths, &self.account).ok().as_ref(),
                self.receipt.generation(),
                observation,
            );
        }
    }
}
impl openrouter::KeyUsageObserver for Capture {
    fn completed(&self, observation: &OpenRouterKeyUsageProbe) {
        self.publish(&project(observation.clone()));
    }
}

#[cfg(any(feature = "web", test))]
pub(crate) mod console;
fn project(probe: OpenRouterKeyUsageProbe) -> VerticalObservation {
    match probe {
        OpenRouterKeyUsageProbe::NotConfigured => VerticalObservation::NotConnected,
        OpenRouterKeyUsageProbe::Ok(usage, label) => {
            VerticalObservation::OpenRouterKey(usage, label)
        }
        OpenRouterKeyUsageProbe::Failed(reason) => VerticalObservation::Unavailable(reason),
        OpenRouterKeyUsageProbe::Rejected { reason, source } => {
            VerticalObservation::RejectedKey { reason, source }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn observe_legacy(
        paths: &PlakPaths,
        account: &AccountRef,
        fetch: impl FnOnce(
            Arc<dyn openrouter::OpenRouterCredentials>,
            &super::super::UsageReceipt,
        ) -> VerticalObservation,
    ) -> VerticalObservation {
        let (credentials, capture) = prepare_capture(paths, account);
        let observation = fetch(credentials, &capture.receipt);
        capture.publish(&observation);
        observation
    }

    #[test]
    fn a_restored_key_cannot_tag_a_different_dispatched_generation_as_current()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = PlakPaths::explicit(root.path().join("home"));
        let account: AccountRef = "openrouter/work".parse()?;
        secrets::set_api_key(&paths, account.as_str(), "synthetic-offline-fixture")?;
        let entry = entry(&paths, &account).ok_or("fixture registry full")?;
        let flight = Flight::legacy(Some(Arc::clone(&entry)));
        let before = fence(&paths, &account)?;
        flight.publish(
            &before,
            Some(&before),
            Some("a-different-dispatched-generation".into()),
            &VerticalObservation::Unavailable("wrong credential's result".into()),
        );
        assert!(lock(&entry.state).observation.is_none());
        flight.publish(
            &before,
            Some(&before),
            before.generation.clone(),
            &VerticalObservation::Unavailable("exact credential's result".into()),
        );
        assert!(lock(&entry.state).observation.is_some());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn console_during_and_after_legacy_refresh_makes_no_extra_get_and_legacy_stays_fresh()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = PlakPaths::explicit(root.path().join("home"));
        let account: AccountRef = "openrouter/work".parse()?;
        secrets::set_api_key(&paths, account.as_str(), "synthetic-offline-fixture")?;
        let gets = Arc::new(AtomicUsize::new(0));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let legacy = {
            let paths = paths.clone();
            let account = account.clone();
            let gets = Arc::clone(&gets);
            std::thread::spawn(move || {
                observe_legacy(&paths, &account, |_, receipt| {
                    *lock(&receipt.dispatched) = super::super::key_usage_fence(&paths, &account)
                        .ok()
                        .flatten();
                    gets.fetch_add(1, Ordering::SeqCst);
                    let _ = started.send(());
                    let _ = finished.blocking_recv();
                    VerticalObservation::Unavailable("offline fixture observation".into())
                })
            })
        };
        ready.await?;
        let mut read = Box::pin(console::read(&paths, &account));
        std::future::poll_fn(|context| {
            assert!(
                read.as_mut().poll(context).is_pending(),
                "console must join the already-running legacy GET"
            );
            std::task::Poll::Ready(())
        })
        .await;
        finish.send(()).map_err(|()| "legacy stopped early")?;
        assert!(
            matches!(read.await,VerticalObservation::Unavailable(reason) if reason=="offline fixture observation")
        );
        legacy.join().map_err(|_| "legacy fixture panicked")?;
        assert!(
            matches!(console::read(&paths,&account).await,VerticalObservation::Unavailable(reason) if reason=="offline fixture observation")
        );
        assert_eq!(gets.load(Ordering::SeqCst), 1);
        observe_legacy(&paths, &account, |_, receipt| {
            *lock(&receipt.dispatched) = super::super::key_usage_fence(&paths, &account)
                .ok()
                .flatten();
            gets.fetch_add(1, Ordering::SeqCst);
            VerticalObservation::Unavailable("fresh legacy observation".into())
        });
        assert_eq!(
            gets.load(Ordering::SeqCst),
            2,
            "legacy never consumes the populated console cache"
        );
        let sibling: AccountRef = "openrouter/other".parse()?;
        assert!(matches!(
            console::read(&paths, &sibling).await,
            VerticalObservation::NotConnected
        ));
        Ok(())
    }
}
