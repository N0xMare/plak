//! Who holds a session another process has open (spec 02 §4.6): a session's
//! head branch is held under a runtime lease whose holder record names the
//! process, proven running by its process lock (the runtime store's holder
//! proof), and a tree with a hub is held by its custody record. A session
//! whose open is refused because another live process holds its lease is
//! refused to every face, typed `tree_hosted_elsewhere` and naming that
//! process, and a running `plak host` is named as such.

use std::io::Read as _;
use std::sync::Arc;

use plakous::sessions::SessionLocator;
use plakous::sessions::holder::BranchHolder;
use plakous_hosted::runtime::LeaseHolder;

use crate::home::paths::PlakPaths;

/// The process that holds `locator`'s branch in this home's runtime store,
/// unless it is this process or is proven gone.
#[must_use]
pub fn runtime_holder(paths: &PlakPaths, locator: SessionLocator) -> Option<BranchHolder> {
    plakous::sessions::holder::sqlite_branch_holder(&paths.runtime_database(), locator)
}

/// The process id the `plak host` of this home's state wrote into its
/// `host.lock`, read without taking the lock: a host starting now must not
/// find it taken. The pid names the running host only when the caller has
/// proven that process alive.
fn host_lock_pid(paths: &PlakPaths) -> Option<u32> {
    let mut holder = String::new();
    std::fs::File::open(paths.host_lock())
        .ok()?
        .read_to_string(&mut holder)
        .ok()?;
    holder.split_whitespace().next()?.parse().ok()
}

/// Names the live process that holds session `id`'s head branch when an
/// open of it is refused for that branch's lease (`HostConfig::lease_holder`):
/// the refusal is then `tree_hosted_elsewhere`, naming that process, and a
/// running `plak host` as such. A holder whose lease lapsed is taken over
/// under the lease law (spec 02 §2.6) rather than refused, so only a refused
/// open asks.
#[must_use]
pub fn lease_holder(paths: &PlakPaths, id: &str) -> LeaseHolder {
    let paths = paths.clone();
    let id = id.to_owned();
    Arc::new(move |locator| held_elsewhere(&paths, &id, locator))
}

fn held_elsewhere(paths: &PlakPaths, id: &str, locator: SessionLocator) -> Option<String> {
    // The runtime holder's lock proved its process alive.
    let holder = runtime_holder(paths, locator)?;
    let remedy = if host_lock_pid(paths) == Some(holder.pid) {
        "it is plak host, which serves this home's sessions: release the session there \
         (release_session from its web app, or wait for its idle release) or stop the host"
    } else {
        "use that process, or end it first"
    };
    Some(format!(
        "tree_hosted_elsewhere: session {id} is open in another plak process (pid {} on {}); {remedy}",
        holder.pid, holder.host_name
    ))
}
