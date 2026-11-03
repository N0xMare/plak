//! This process's custody: the host identity it claims trees as, and the
//! claims it holds, counted per store and tree so that every holder in the
//! process shares one claim and only the last one releases it.
//!
//! Every hold ends in [`release_custody_hold`], or in
//! [`CustodyHold::forget`] when the record went with the tree's rows. A last
//! share dropped without either keeps the record held by this live process
//! until it exits, so that drop is a defect: debug builds assert, release
//! builds warn.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use super::{CustodyClaim, CustodyError, CustodyHost, HostInstance, TreeCustody};

/// The host identity this process takes custody as, fixed at its first use.
static HOST: OnceLock<CustodyHost> = OnceLock::new();

/// The claims this process holds, keyed by store scope and tree id.
static HOLDS: OnceLock<Mutex<HashMap<(String, String), Held>>> = OnceLock::new();

struct Held {
    claim: CustodyClaim,
    count: usize,
}

fn holds() -> &'static Mutex<HashMap<(String, String), Held>> {
    HOLDS.get_or_init(Mutex::default)
}

/// Names this process's host before its first custody take: its stable host
/// id, the machine it runs on and its binary's build id. The process
/// instance is drawn here.
///
/// # Errors
///
/// Returns the identity already in force when one was fixed first, by an
/// earlier binding or by a take that used the default.
pub fn bind_custody_host(
    host_id: impl Into<String>,
    host_name: impl Into<String>,
    binary: Option<String>,
) -> Result<(), CustodyHost> {
    let (host_id, host_name) = (host_id.into(), host_name.into());
    let mut bound_now = false;
    let bound = HOST.get_or_init(|| {
        bound_now = true;
        CustodyHost {
            host_id: host_id.clone(),
            instance: HostInstance::generate(),
            binary: binary.clone(),
            host_name: host_name.clone(),
        }
    });
    if bound_now
        || (bound.host_id == host_id && bound.host_name == host_name && bound.binary == binary)
    {
        Ok(())
    } else {
        Err(bound.clone())
    }
}

/// The host this process takes custody as. Unbound, it is the local host
/// with no build id, on a machine named `localhost`.
pub fn custody_host() -> &'static CustodyHost {
    HOST.get_or_init(|| CustodyHost {
        host_id: "local".to_owned(),
        instance: HostInstance::generate(),
        binary: None,
        host_name: "localhost".to_owned(),
    })
}

/// One holder's share of this process's claim on one tree in one store.
#[derive(Debug)]
pub struct CustodyHold {
    key: Option<(String, String)>,
    claim: CustodyClaim,
}

impl CustodyHold {
    /// The claim every write under this hold presents.
    #[must_use]
    pub const fn claim(&self) -> &CustodyClaim {
        &self.claim
    }

    /// Forgets this share without writing: the tree's record is gone with
    /// its rows.
    pub fn forget(mut self) {
        if let Some(key) = self.key.take() {
            let _ = give_up(&key);
        }
    }
}

/// Takes one share of `key` out of this process's count; `true` when it was
/// the last one.
fn give_up(key: &(String, String)) -> bool {
    let mut holds = holds().lock().unwrap_or_else(PoisonError::into_inner);
    let Some(held) = holds.get_mut(key) else {
        return true;
    };
    held.count = held.count.saturating_sub(1);
    if held.count > 0 {
        return false;
    }
    holds.remove(key);
    true
}

impl Drop for CustodyHold {
    /// A share dropped without a release: the count goes down, and a last
    /// share leaves the record held by this process until it exits.
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        if !give_up(&key) || std::thread::panicking() {
            return;
        }
        tracing::warn!(
            target: "plak.topology",
            tree_id = %key.1,
            "agent tree custody hold was dropped without its release; this process keeps it until it exits"
        );
        #[cfg(debug_assertions)]
        panic!(
            "the last custody share of {} was dropped without release_custody_hold",
            key.1
        );
    }
}

/// Takes custody of `tree_id` in `store` for this process, or joins the
/// claim this process already holds there.
///
/// # Errors
///
/// [`CustodyError::HostedElsewhere`] while another host process holds it,
/// and any store failure.
pub fn acquire_custody(
    store: &dyn TreeCustody,
    tree_id: &str,
) -> Result<CustodyHold, CustodyError> {
    let key = (store.custody_scope(), tree_id.to_owned());
    let mut holds = holds().lock().unwrap_or_else(PoisonError::into_inner);
    let claim = store.take_custody(tree_id, custody_host(), None)?;
    match holds.get_mut(&key) {
        Some(held) => {
            held.claim = claim.clone();
            held.count += 1;
        }
        None => {
            holds.insert(
                key.clone(),
                Held {
                    claim: claim.clone(),
                    count: 1,
                },
            );
        }
    }
    Ok(CustodyHold {
        key: Some(key),
        claim,
    })
}

/// Gives up one holder's share; the last share releases the record.
///
/// # Errors
///
/// The store's refusal of that release. The share is gone either way, and a
/// record left held names this live process.
pub fn release_custody_hold(
    store: &dyn TreeCustody,
    mut hold: CustodyHold,
) -> Result<(), CustodyError> {
    let Some(key) = hold.key.take() else {
        return Ok(());
    };
    let mut registry = holds().lock().unwrap_or_else(PoisonError::into_inner);
    let last = match registry.get_mut(&key) {
        Some(entry) => {
            entry.count = entry.count.saturating_sub(1);
            entry.count == 0
        }
        None => true,
    };
    if !last {
        return Ok(());
    }
    registry.remove(&key);
    store.release_custody(&hold.claim)
}

/// Runs `work` under a custody claim of `tree_id` in `store`: this process's
/// claim when it holds one, otherwise one taken for this call and released
/// after it.
///
/// # Errors
///
/// The custody refusal, or `work`'s own error.
pub fn with_custody<T, E: From<CustodyError>>(
    store: &dyn TreeCustody,
    tree_id: &str,
    work: impl FnOnce(&CustodyClaim) -> Result<T, E>,
) -> Result<T, E> {
    let hold = acquire_custody(store, tree_id)?;
    let result = work(hold.claim());
    if let Err(error) = release_custody_hold(store, hold) {
        tracing::warn!(
            target: "plak.topology",
            tree_id,
            error = %error,
            "agent tree custody was not released; this process keeps it until it exits"
        );
    }
    result
}
