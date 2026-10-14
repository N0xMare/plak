//! Account/profile-scoped in-process single-flight coordination.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, Weak};

use thiserror::Error;

use crate::{AccountBinding, SubscriptionIntegrationProfileId};

/// A reusable in-process account/profile single-flight registry.
///
/// Guards are owning and may be held across provider-owned asynchronous work.
/// Acquiring a guard is a blocking operation and should not run on an async
/// executor worker when contention is possible.
#[derive(Clone, Default)]
pub struct AuthSingleFlight {
    inner: Arc<Registry>,
}

impl AuthSingleFlight {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits until no operation is active for this exact profile/account pair.
    ///
    /// The returned owning guard keeps the flight active until it is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`SingleFlightError::CoordinationFailed`] if synchronization
    /// state was poisoned by a panic.
    pub fn enter(
        &self,
        profile_id: &SubscriptionIntegrationProfileId,
        account_binding: &AccountBinding,
    ) -> Result<AuthSingleFlightGuard, SingleFlightError> {
        let key = ScopeKey {
            profile_id: profile_id.clone(),
            account_binding: account_binding.clone(),
        };
        let entry = {
            let mut entries = self
                .inner
                .entries
                .lock()
                .map_err(|_| SingleFlightError::CoordinationFailed)?;
            entries.retain(|_, weak| weak.strong_count() > 0);
            if let Some(entry) = entries.get(&key).and_then(Weak::upgrade) {
                entry
            } else {
                let entry = Arc::new(FlightEntry::default());
                entries.insert(key, Arc::downgrade(&entry));
                entry
            }
        };

        let mut active = entry
            .active
            .lock()
            .map_err(|_| SingleFlightError::CoordinationFailed)?;
        while *active {
            active = entry
                .ready
                .wait(active)
                .map_err(|_| SingleFlightError::CoordinationFailed)?;
        }
        *active = true;
        drop(active);
        Ok(AuthSingleFlightGuard { entry })
    }

    /// Runs a synchronous operation as the only in-process flight for a scope.
    ///
    /// # Errors
    ///
    /// Returns coordination failures directly and closure failures as
    /// [`SingleFlightRunError::Operation`].
    pub fn run<T, E>(
        &self,
        profile_id: &SubscriptionIntegrationProfileId,
        account_binding: &AccountBinding,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, SingleFlightRunError<E>> {
        let _guard = self
            .enter(profile_id, account_binding)
            .map_err(SingleFlightRunError::Coordination)?;
        operation().map_err(SingleFlightRunError::Operation)
    }
}

impl fmt::Debug for AuthSingleFlight {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthSingleFlight { .. }")
    }
}

/// An owning account/profile single-flight guard.
pub struct AuthSingleFlightGuard {
    entry: Arc<FlightEntry>,
}

impl fmt::Debug for AuthSingleFlightGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthSingleFlightGuard { scope: <redacted> }")
    }
}

impl Drop for AuthSingleFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.entry.active.lock() {
            *active = false;
            self.entry.ready.notify_one();
        }
    }
}

/// In-process single-flight coordination failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SingleFlightError {
    /// A previous panic poisoned synchronization state.
    #[error("authentication single-flight coordination failed")]
    CoordinationFailed,
}

/// Failure from [`AuthSingleFlight::run`].
#[derive(Error)]
pub enum SingleFlightRunError<E> {
    /// Single-flight coordination could not be established.
    #[error(transparent)]
    Coordination(#[from] SingleFlightError),
    /// The protected operation failed.
    #[error("single-flight operation failed")]
    Operation(E),
}

impl<E> fmt::Debug for SingleFlightRunError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordination(error) => {
                formatter.debug_tuple("Coordination").field(error).finish()
            }
            Self::Operation(_) => formatter.write_str("Operation(<redacted error>)"),
        }
    }
}

#[derive(Eq, Hash, PartialEq)]
struct ScopeKey {
    profile_id: SubscriptionIntegrationProfileId,
    account_binding: AccountBinding,
}

#[derive(Default)]
struct Registry {
    entries: Mutex<HashMap<ScopeKey, Weak<FlightEntry>>>,
}

#[derive(Default)]
struct FlightEntry {
    active: Mutex<bool>,
    ready: Condvar,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn same_scope_is_single_flight() -> Result<(), Box<dyn std::error::Error>> {
        let flights = AuthSingleFlight::new();
        let profile = SubscriptionIntegrationProfileId::new("profile")?;
        let account = AccountBinding::new("account")?;
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(5));
        let mut threads = Vec::new();

        for _ in 0..4 {
            let flights = flights.clone();
            let profile = profile.clone();
            let account = account.clone();
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || -> Result<(), SingleFlightError> {
                barrier.wait();
                let _guard = flights.enter(&profile, &account)?;
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(3));
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }));
        }

        barrier.wait();
        for thread in threads {
            thread
                .join()
                .map_err(|_| "single-flight test thread panicked")??;
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// Two accounts under one profile are two flights.
    ///
    /// Entering both on this thread proves nothing on its own: if they shared a
    /// flight the second `enter` would block forever and the test would hang
    /// rather than fail. The second entry runs on its own thread and reports
    /// back within a bound, so a shared flight is a failure with a message.
    #[test]
    fn different_accounts_do_not_share_a_flight() -> Result<(), Box<dyn std::error::Error>> {
        let flights = AuthSingleFlight::new();
        let profile = SubscriptionIntegrationProfileId::new("profile")?;
        let first = AccountBinding::new("first")?;
        let second = AccountBinding::new("second")?;
        let held = flights.enter(&profile, &first)?;

        let (tx, rx) = std::sync::mpsc::channel();
        let entered = {
            let flights = flights.clone();
            let profile = profile.clone();
            let second = second.clone();
            thread::spawn(move || {
                let guard = flights.enter(&profile, &second);
                let _ = tx.send(guard.is_ok());
                drop(guard);
            })
        };
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(true),
            "a second account must not wait on the first account's flight"
        );
        entered
            .join()
            .map_err(|_| "single-flight test thread panicked")?;

        // The same account, however, does wait: the flight is scoped, not off.
        let (tx, rx) = std::sync::mpsc::channel();
        let contended = {
            let flights = flights.clone();
            let profile = profile.clone();
            let first = first.clone();
            thread::spawn(move || {
                let guard = flights.enter(&profile, &first);
                let _ = tx.send(guard.is_ok());
                drop(guard);
            })
        };
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "the same account must wait while the first guard is held"
        );
        drop(held);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true));
        contended
            .join()
            .map_err(|_| "single-flight test thread panicked")?;
        Ok(())
    }
}
