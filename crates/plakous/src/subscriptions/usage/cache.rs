//! One-way reuse of exact-fenced observations; legacy reads never look here.

use super::{NativeProvider, SubscriptionUsageRead};
use crate::subscriptions::SubscriptionPaths;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;
use tokio::sync::{Mutex as AsyncMutex, watch};

pub(super) use crate::subscriptions::USAGE_OBSERVATION_FRESHNESS as FRESHNESS;
const MAX_SCOPES: usize = 512;
pub(super) type Revision = (Option<String>, Option<u64>);

#[derive(Hash, Eq, PartialEq)]
struct Scope(PathBuf, NativeProvider, Option<String>);

#[derive(Default)]
struct Registry {
    entries: HashMap<Scope, Arc<Entry>>,
    untracked: usize,
    last_untracked: Option<Instant>,
}

pub(super) struct Entry {
    pub console: AsyncMutex<()>,
    state: Mutex<State>,
    changed: watch::Sender<()>,
}

#[derive(Default)]
struct State {
    active: usize,
    observation: Option<(Revision, Instant, SubscriptionUsageRead)>,
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

pub(super) fn entry(paths: &SubscriptionPaths, provider: NativeProvider) -> Option<Arc<Entry>> {
    let mut registry = lock(registry());
    let scope = Scope(
        paths.state_root.clone(),
        provider,
        paths.named.as_ref().map(|slot| slot.name.clone()),
    );
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
    if registry.entries.len() >= MAX_SCOPES {
        return None;
    }
    let entry = Arc::new(Entry {
        console: AsyncMutex::new(()),
        state: Mutex::default(),
        changed: watch::channel(()).0,
    });
    registry.entries.insert(scope, Arc::clone(&entry));
    Some(entry)
}

impl Entry {
    pub fn pending(&self) -> Option<watch::Receiver<()>> {
        let receiver = self.changed.subscribe();
        (lock(&self.state).active > 0).then_some(receiver)
    }

    pub fn cached(&self, revision: &Revision) -> Option<SubscriptionUsageRead> {
        lock(&self.state)
            .observation
            .as_ref()
            .filter(|(recorded, at, _)| recorded == revision && at.elapsed() < FRESHNESS)
            .map(|(_, _, observation)| observation.clone())
    }
}

pub(super) fn can_fetch() -> bool {
    let registry = lock(registry());
    registry.untracked == 0
        && registry
            .last_untracked
            .is_none_or(|at| at.elapsed() >= FRESHNESS)
}

pub(super) struct Flight(Option<Arc<Entry>>);

impl Flight {
    pub fn start(paths: &SubscriptionPaths, provider: NativeProvider) -> Self {
        let entry = entry(paths, provider);
        if let Some(entry) = &entry {
            lock(&entry.state).active += 1;
        } else {
            // Saturating the bounded observation registry must not alter a
            // legacy read. Console misses refuse until untracked work ages.
            lock(registry()).untracked += 1;
        }
        Self(entry)
    }

    pub fn publish(&self, revision: Revision, observation: &SubscriptionUsageRead) {
        if let Some(entry) = &self.0 {
            lock(&entry.state).observation = Some((revision, Instant::now(), observation.clone()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn only_exact_fences_within_thirty_seconds_are_reusable()
    -> Result<(), Box<dyn std::error::Error>> {
        let entry = Entry {
            console: AsyncMutex::new(()),
            state: Mutex::default(),
            changed: watch::channel(()).0,
        };
        let observation = SubscriptionUsageRead::Unavailable {
            observed_at: crate::subscriptions::Timestamp::new(0, 0)?,
            reason: crate::subscriptions::SubscriptionUsageUnavailable::NotConnected,
        };
        let revision = (Some("receipt".into()), Some(7));
        lock(&entry.state).observation = Some((
            revision.clone(),
            Instant::now()
                .checked_sub(Duration::from_secs(29))
                .ok_or("clock range")?,
            observation.clone(),
        ));
        assert_eq!(entry.cached(&revision), Some(observation));
        assert!(entry.cached(&(Some("other".into()), Some(7))).is_none());
        assert!(entry.cached(&(Some("receipt".into()), Some(8))).is_none());
        lock(&entry.state)
            .observation
            .as_mut()
            .ok_or("observation")?
            .1 = Instant::now().checked_sub(FRESHNESS).ok_or("clock range")?;
        assert!(entry.cached(&revision).is_none());
        Ok(())
    }
}
