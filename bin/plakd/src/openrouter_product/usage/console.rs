//! Console-only consumers of the common exact-fenced observer state.

use super::{
    AccountRef, Arc, FRESHNESS, Flight, PlakPaths, VerticalObservation, entry, fence, lock,
    openrouter, project, registry,
};

pub(crate) async fn read<Permit>(
    paths: &PlakPaths,
    account: &AccountRef,
    before_get: impl Future<Output = Permit>,
) -> VerticalObservation {
    let Some(entry) = entry(paths, account) else {
        return unavailable("provider observation registry is busy");
    };
    loop {
        let mut changed = entry.changed.subscribe();
        let Ok(before) = fence(paths, account) else {
            return unavailable("key status could not be read");
        };
        let registry_busy = {
            let registry = lock(registry());
            registry.untracked > 0
                || registry
                    .last_untracked
                    .is_some_and(|at| at.elapsed() < FRESHNESS)
        };
        let decision = {
            let mut state = lock(&entry.state);
            if state.active > 0 {
                None
            } else {
                if let Some((fence, at, observation)) = &state.observation
                    && fence == &before
                    && at.elapsed() < FRESHNESS
                {
                    return observation.clone();
                }
                if registry_busy {
                    return unavailable("provider observation registry is busy");
                }
                state.active += 1;
                Some(Flight(Some(Arc::clone(&entry))))
            }
        };
        let Some(flight) = decision else {
            let _ = changed.changed().await;
            continue;
        };
        let credentials = super::super::ProductOpenRouterCredentials::shared(paths);
        let _permit = before_get.await;
        // The console owns the shared provider deadline; legacy callers keep
        // their existing timeout policy and never consume this cache.
        let observation =
            project(openrouter::read_openrouter_key_usage(credentials.as_ref(), account).await);
        let after = fence(paths, account).ok();
        if after.as_ref() != Some(&before) {
            return unavailable("account changed during allowance read");
        }
        let cached = {
            let state = lock(&entry.state);
            state
                .observation
                .as_ref()
                .filter(|(fence, at, _)| fence == &before && at.elapsed() < FRESHNESS)
                .map(|(_, _, observation)| observation.clone())
        };
        if let Some(observation) = cached {
            return observation;
        }
        if before.generation.is_none() {
            flight.publish(&before, after.as_ref(), None, &observation);
            return observation;
        }
        return unavailable("allowance result could not be fenced; retry");
    }
}
fn unavailable(reason: &str) -> VerticalObservation {
    VerticalObservation::Unavailable(reason.to_owned())
}
