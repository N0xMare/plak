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
