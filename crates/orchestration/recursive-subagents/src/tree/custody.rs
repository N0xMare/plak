//! Tree custody: one host process per tree, as a fenced claim beside the
//! tree's rows (spec 03 §14).
//!
//! A claim names the host process (its stable host id, its typed instance,
//! its binary and machine) and the fence its take minted. Every take that
//! changes the holder raises the fence; a release keeps the fence and the
//! last holder; a claim proves only while the stored holder and fence are
//! exactly its own and the record is held, so a superseded claim can never
//! act again. Another host's claim is taken over only with [`HolderGone`],
//! which only a sealed [`CustodyLiveness`] can mint. The transitions live
//! here, so every store applies the same law; a store only reads and writes
//! the record atomically.

use std::hash::{BuildHasher as _, Hasher as _};

use super::TreeStoreError;

mod custodian;
mod liveness;
pub use custodian::{
    CustodyHold, acquire_custody, bind_custody_host, custody_host, holder_runs_here, machine_name,
    process_instance, release_custody_hold, with_custody,
};

/// One host process image: its process id and a nonce drawn at start, so a
/// reused process id is still another instance. Text form `<pid>-<16 hex>`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HostInstance {
    pid: u32,
    nonce: u64,
}

impl HostInstance {
    /// A fresh identity for this process.
    #[must_use]
    pub fn generate() -> Self {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
        );
        Self {
            pid: std::process::id(),
            nonce: hasher.finish(),
        }
    }

    /// Admits exactly the text form [`Self::generate`] writes.
    ///
    /// # Errors
    ///
    /// Refuses anything else.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || "custody host instance is not <pid>-<16 hex>".to_owned();
        let (pid, nonce) = text.split_once('-').ok_or_else(invalid)?;
        let canonical_pid = !pid.is_empty()
            && pid.bytes().all(|byte| byte.is_ascii_digit())
            && (pid == "0" || !pid.starts_with('0'));
        let canonical_nonce = nonce.len() == 16
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !canonical_pid || !canonical_nonce {
            return Err(invalid());
        }
        Ok(Self {
            pid: pid.parse().map_err(|_| invalid())?,
            nonce: u64::from_str_radix(nonce, 16).map_err(|_| invalid())?,
        })
    }

    /// The instance's process id.
    #[must_use]
    pub const fn pid(self) -> u32 {
        self.pid
    }
}

impl std::fmt::Display for HostInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}-{:016x}", self.pid, self.nonce)
    }
}

/// One host process, as custody names it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CustodyHost {
    /// The host's stable identity, the same across its restarts.
    pub host_id: String,
    /// This process image.
    pub instance: HostInstance,
    /// The holding binary's build id, when it knows one.
    pub binary: Option<String>,
    /// The machine the process runs on, so liveness can tell a holder on
    /// this machine from one elsewhere.
    pub host_name: String,
}

/// A custody record's holder: the last host process that took it, and when.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustodyHolding {
    /// The holding host process.
    pub host: CustodyHost,
    /// Unix milliseconds the holder's take committed.
    pub claimed_at_ms: u64,
    /// Unix milliseconds of the holder's last renewal, or its take.
    pub renewed_at_ms: u64,
}

/// A tree's stored custody: the fence the last holder-changing take minted,
/// that holder, and whether it has released.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustodyRecord {
    /// The fence the last holder-changing take minted, from 1.
    pub fence: u64,
    /// The last holder, kept after it releases.
    pub holding: CustodyHolding,
    /// Whether that holder released.
    pub released: bool,
}

/// The proof a holder carries: the tree, itself, and the fence its take
/// minted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustodyClaim {
    /// The tree in custody.
    pub tree_id: String,
    /// The holder this claim names.
    pub host: CustodyHost,
    /// The fence the take minted.
    pub fence: u64,
}

pub(super) mod sealed {
    pub trait Sealed {}
}

/// The host's proof that a custody holder's process is gone (spec 03 §14).
///
/// Sealed: only this crate's liveness module implements it, so
/// [`HolderGone`] cannot be minted from outside.
pub trait CustodyLiveness: sealed::Sealed {
    /// Whether `holder`'s process is proven gone.
    fn holder_gone(&self, holder: &CustodyHost) -> bool;
}

/// Proof that exactly this claim's holder is gone: the only way to take a
/// tree another host holds without its release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HolderGone(CustodyClaim);

impl HolderGone {
    /// The proof for `claim` when `liveness` proves its holder gone.
    #[must_use]
    pub fn prove(claim: CustodyClaim, liveness: &dyn CustodyLiveness) -> Option<Self> {
        liveness.holder_gone(&claim.host).then_some(Self(claim))
    }

    /// The claim proven gone.
    #[must_use]
    pub const fn claim(&self) -> &CustodyClaim {
        &self.0
    }
}

/// The typed refusal for a tree another host process holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeHostedElsewhere {
    /// The tree asked for.
    pub tree_id: String,
    /// The host process that holds it, with its binary id.
    pub holder: CustodyHost,
    /// The holder's fence.
    pub fence: u64,
}

impl TreeHostedElsewhere {
    /// The stable refusal code.
    pub const CODE: &'static str = "tree_hosted_elsewhere";

    /// The holding binary's build id, when it recorded one.
    #[must_use]
    pub fn binary(&self) -> Option<&str> {
        self.holder.binary.as_deref()
    }

    /// The holder's claim, exactly as a [`HolderGone`] must name it.
    #[must_use]
    pub fn holder_claim(&self) -> CustodyClaim {
        CustodyClaim {
            tree_id: self.tree_id.clone(),
            host: self.holder.clone(),
            fence: self.fence,
        }
    }
}

impl std::fmt::Display for TreeHostedElsewhere {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} ({}): agent tree {} is hosted by {} on {} (instance {}); use that host or stop it first",
            Self::CODE,
            self.binary().unwrap_or("unknown build"),
            self.tree_id,
            self.holder.host_id,
            self.holder.host_name,
            self.holder.instance
        )
    }
}

impl std::error::Error for TreeHostedElsewhere {}

/// Why a custody operation did not succeed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CustodyError {
    /// Another host process holds the tree.
    HostedElsewhere(TreeHostedElsewhere),
    /// This claim does not hold the tree: it was released, or superseded by
    /// a later take.
    NotHeld {
        /// The tree asked for.
        tree_id: String,
        /// The fence the claim carried.
        fence: u64,
    },
    /// The store could not read or write the record.
    Store(TreeStoreError),
}

impl std::fmt::Display for CustodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostedElsewhere(refusal) => refusal.fmt(formatter),
            Self::NotHeld { tree_id, fence } => write!(
                formatter,
                "custody of agent tree {tree_id} at fence {fence} is no longer held"
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CustodyError {}

impl From<TreeStoreError> for CustodyError {
    fn from(error: TreeStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<CustodyError> for TreeStoreError {
    fn from(error: CustodyError) -> Self {
        match error {
            CustodyError::HostedElsewhere(refusal) => Self::hosted_elsewhere(refusal),
            CustodyError::NotHeld { .. } => Self::not_in_custody(error.to_string()),
            CustodyError::Store(error) => error,
        }
    }
}

/// Custody of trees in one store. Each call reads and writes the record in
/// one atomic step. A tree deletion leaves the record; tree ids are never
/// reused.
pub trait TreeCustody: Send + Sync {
    /// The identity of the record set these calls read and write: two
    /// handles of one store answer the same scope, two stores never do.
    fn custody_scope(&self) -> String;

    /// The directory the store's host processes keep their liveness locks
    /// in (`custody-hosts` beside the database), or `None` for a store only
    /// one process can reach, where no holder can be proven gone.
    fn liveness_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// The stored custody of `tree_id`, or `None` when no take ever ran.
    ///
    /// # Errors
    ///
    /// Refuses an unreachable store and a record that does not decode.
    fn custody(&self, tree_id: &str) -> Result<Option<CustodyRecord>, TreeStoreError>;

    /// The host instances that hold at least one record that is not
    /// released, which liveness sweeps never remove the lock of.
    ///
    /// # Errors
    ///
    /// Refuses an unreachable store and a record that does not decode.
    fn held_custody_instances(
        &self,
    ) -> Result<std::collections::BTreeSet<HostInstance>, TreeStoreError>;

    /// Takes custody for `host`: a new or released record is claimed under
    /// the next fence; a record `host` already holds is returned as is; a
    /// record another host holds is taken only with `takeover` proving
    /// exactly that holder and fence gone.
    ///
    /// # Errors
    ///
    /// [`CustodyError::HostedElsewhere`] while another host holds it.
    fn take_custody(
        &self,
        tree_id: &str,
        host: &CustodyHost,
        takeover: Option<&HolderGone>,
    ) -> Result<CustodyClaim, CustodyError>;

    /// Proves `claim` and records the renewal time.
    ///
    /// # Errors
    ///
    /// Refuses a claim that does not hold the tree.
    fn renew_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError>;

    /// Releases `claim`, keeping its fence and holder. Repeating the release
    /// of the last holder's claim succeeds, so an unacknowledged release can
    /// be retried; any other claim is not held.
    ///
    /// # Errors
    ///
    /// Refuses a claim that is not the last holder's.
    fn release_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError>;

    /// Proves that `claim` still holds the tree, without writing.
    ///
    /// # Errors
    ///
    /// Refuses a claim that does not hold the tree.
    fn prove_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError>;

    /// The operator's break-glass: releases the record whoever holds it,
    /// keeping its fence and holder, so the next take supersedes that holder
    /// and every write it still offers is refused. Returns the record as it
    /// was held, or `None` when no take ran or it is already released.
    ///
    /// # Errors
    ///
    /// Refuses an unreachable store.
    fn break_custody(&self, tree_id: &str) -> Result<Option<CustodyRecord>, CustodyError>;
}

/// What a take does to the stored record.
#[derive(Debug)]
pub enum CustodyTake {
    /// The caller already holds it; nothing is written.
    Held(CustodyClaim),
    /// Write this record; the caller now holds `claim`.
    Claimed {
        /// The record to store.
        record: CustodyRecord,
        /// The new claim.
        claim: CustodyClaim,
    },
}

impl CustodyRecord {
    /// The take law over the stored record `current`.
    ///
    /// # Errors
    ///
    /// Refuses a tree another host holds, absent an exact takeover, and an
    /// empty host identity.
    pub fn take(
        current: Option<&Self>,
        tree_id: &str,
        host: &CustodyHost,
        takeover: Option<&HolderGone>,
        now_ms: u64,
    ) -> Result<CustodyTake, CustodyError> {
        if host.host_id.is_empty()
            || host.host_name.is_empty()
            || host.binary.as_deref().is_some_and(str::is_empty)
        {
            return Err(CustodyError::Store(TreeStoreError::unavailable(
                "agent tree custody host identity is empty",
            )));
        }
        let claim_at = |fence| CustodyClaim {
            tree_id: tree_id.to_owned(),
            host: host.clone(),
            fence,
        };
        let fence = match current {
            None => 0,
            Some(record) if record.released => record.fence,
            Some(record) if record.holding.host == *host => {
                return Ok(CustodyTake::Held(claim_at(record.fence)));
            }
            Some(record) => {
                let refusal = record.hosted_elsewhere(tree_id);
                if takeover.is_none_or(|gone| *gone.claim() != refusal.holder_claim()) {
                    return Err(CustodyError::HostedElsewhere(refusal));
                }
                record.fence
            }
        };
        let fence = fence
            .checked_add(1)
            .ok_or_else(|| TreeStoreError::corrupt("agent tree custody fence is exhausted"))?;
        Ok(CustodyTake::Claimed {
            record: Self {
                fence,
                holding: CustodyHolding {
                    host: host.clone(),
                    claimed_at_ms: now_ms,
                    renewed_at_ms: now_ms,
                },
                released: false,
            },
            claim: claim_at(fence),
        })
    }

    /// The record after `claim` renews at `now_ms`.
    ///
    /// # Errors
    ///
    /// Refuses a claim that does not hold the tree.
    pub fn renew(
        current: Option<&Self>,
        claim: &CustodyClaim,
        now_ms: u64,
    ) -> Result<Self, CustodyError> {
        let mut renewed = Self::prove(current, claim)?.clone();
        renewed.holding.renewed_at_ms = now_ms;
        Ok(renewed)
    }

    /// The record after `claim` releases, or `None` when the last holder's
    /// claim is already released and nothing needs writing.
    ///
    /// # Errors
    ///
    /// Refuses a claim that is not the last holder's.
    pub fn release(
        current: Option<&Self>,
        claim: &CustodyClaim,
    ) -> Result<Option<Self>, CustodyError> {
        if let Some(record) = current
            && record.released
        {
            if record.is_claim(claim) {
                return Ok(None);
            }
            return Err(not_held(claim));
        }
        let mut released = Self::prove(current, claim)?.clone();
        released.released = true;
        Ok(Some(released))
    }

    /// The record after an operator break-glass release of `current`, or
    /// `None` when there is nothing held to release.
    #[must_use]
    pub fn broken(current: Option<&Self>) -> Option<Self> {
        current
            .filter(|record| !record.released)
            .map(|record| Self {
                released: true,
                ..record.clone()
            })
    }

    /// The held record `claim` proves against `current`.
    ///
    /// # Errors
    ///
    /// [`CustodyError::HostedElsewhere`] when another host holds the tree,
    /// [`CustodyError::NotHeld`] when it is released or `claim` is stale.
    pub fn prove<'a>(
        current: Option<&'a Self>,
        claim: &CustodyClaim,
    ) -> Result<&'a Self, CustodyError> {
        let Some(record) = current else {
            return Err(not_held(claim));
        };
        if record.released {
            return Err(not_held(claim));
        }
        if record.holding.host != claim.host {
            return Err(CustodyError::HostedElsewhere(
                record.hosted_elsewhere(&claim.tree_id),
            ));
        }
        if record.fence != claim.fence {
            return Err(not_held(claim));
        }
        Ok(record)
    }

    fn is_claim(&self, claim: &CustodyClaim) -> bool {
        self.holding.host == claim.host && self.fence == claim.fence
    }

    fn hosted_elsewhere(&self, tree_id: &str) -> TreeHostedElsewhere {
        TreeHostedElsewhere {
            tree_id: tree_id.to_owned(),
            holder: self.holding.host.clone(),
            fence: self.fence,
        }
    }
}

fn not_held(claim: &CustodyClaim) -> CustodyError {
    CustodyError::NotHeld {
        tree_id: claim.tree_id.clone(),
        fence: claim.fence,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
