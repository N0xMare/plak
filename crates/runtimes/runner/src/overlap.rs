//! Pin-backed same-machine consent checks, serialized in both admission orders.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::PlacementFailure;

/// Pairing evidence, retained while its runner is offline. Old pins without a
/// verifiable machine id require re-pair, never a fallback bind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerPin {
    /// Stable runner authority.
    pub runner_id: String,
    /// Stable physical-machine identity.
    pub machine_id: String,
    /// Canonical effective roots from the serving daemon at pairing/bind.
    pub roots: Vec<String>,
}

#[derive(Default)]
struct Guarded {
    pins: BTreeMap<String, RunnerPin>,
    local: BTreeSet<PathBuf>,
}

/// A host-wide guard, seeded from all non-deleted local records and minted
/// copies, not just resident sessions. Callers retain admission serialization
/// through their product commit by using `with_local_admission`.
pub struct MachineOverlapGuard {
    machine_id: Option<String>,
    guarded: Mutex<Guarded>,
}

impl MachineOverlapGuard {
    /// Creates a guard with durable local occupancy. Local opens never depend
    /// on a remote probe or on knowing the host machine identity.
    #[must_use]
    pub fn new(machine_id: Option<String>, local: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            machine_id,
            guarded: Mutex::new(Guarded {
                local: local.into_iter().collect(),
                pins: BTreeMap::new(),
            }),
        }
    }

    /// Restores/reconfirms a pin. Same-machine roots cannot intersect another
    /// authority's admitted roots, local records or minted copies.
    ///
    /// # Errors
    /// Unknown identity or overlapping roots; no pin changes on refusal.
    pub fn admit_pin(&self, pin: RunnerPin) -> Result<(), PlacementFailure> {
        if pin.machine_id.is_empty()
            || pin.runner_id.is_empty()
            || self.machine_id.is_none()
            || pin.roots.iter().any(|root| {
                !Path::new(root).is_absolute()
                    || Path::new(root).components().any(|part| {
                        matches!(
                            part,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
            })
        {
            return Err(PlacementFailure::RunnerNotPaired);
        }
        let mut guarded = self.guarded.lock().unwrap_or_else(PoisonError::into_inner);
        for root in &pin.roots {
            if self.machine_id.as_ref() == Some(&pin.machine_id) {
                for local in &guarded.local {
                    if overlaps(Path::new(root), local) {
                        return Err(conflict(&pin.machine_id, "local", root));
                    }
                }
            }
            for other in guarded.pins.values().filter(|other| {
                other.runner_id != pin.runner_id && other.machine_id == pin.machine_id
            }) {
                if other
                    .roots
                    .iter()
                    .any(|other| overlaps(Path::new(root), Path::new(other)))
                {
                    return Err(conflict(&pin.machine_id, &other.runner_id, root));
                }
            }
        }
        guarded.pins.insert(pin.runner_id.clone(), pin);
        Ok(())
    }

    /// Checks an about-to-be-created canonical local workspace and holds the
    /// same lock used for pin admission through `commit`. Only a successful
    /// commit publishes occupancy. Unknown host machine id never blocks local.
    ///
    /// # Errors
    /// Overlap or the caller's typed commit failure; neither changes occupancy.
    pub fn with_local_admission<T, E>(
        &self,
        path: PathBuf,
        commit: impl FnOnce() -> Result<T, E>,
    ) -> Result<Result<T, E>, PlacementFailure> {
        let mut guarded = self.guarded.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(machine) = &self.machine_id {
            for pin in guarded
                .pins
                .values()
                .filter(|pin| &pin.machine_id == machine)
            {
                if pin
                    .roots
                    .iter()
                    .any(|root| overlaps(&path, Path::new(root)))
                {
                    return Err(conflict(machine, &pin.runner_id, &path.to_string_lossy()));
                }
            }
        }
        let result = commit();
        if result.is_ok() {
            guarded.local.insert(path);
        }
        Ok(result)
    }
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
fn conflict(machine: &str, runner: &str, path: &str) -> PlacementFailure {
    PlacementFailure::RunnerRootsOverlap {
        machine_id: machine.into(),
        conflicting_runner: runner.into(),
        path: path.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(machine: &str, runner: &str, root: &str) -> RunnerPin {
        RunnerPin {
            machine_id: machine.into(),
            runner_id: runner.into(),
            roots: vec![root.into()],
        }
    }

    #[test]
    fn both_admission_orders_guard_dormant_roots_and_minted_copies() {
        let guard = MachineOverlapGuard::new(
            Some("machine".into()),
            [
                PathBuf::from("/workspace/dormant"),
                PathBuf::from("/copies/child"),
            ],
        );
        assert!(
            guard
                .admit_pin(pin("machine", "remote", "/workspace"))
                .is_err()
        );
        assert!(
            guard
                .admit_pin(pin("machine", "remote", "/copies/child/sub"))
                .is_err()
        );
        guard
            .admit_pin(pin("machine", "remote", "/remote"))
            .unwrap();
        let mut committed = false;
        assert!(
            guard
                .with_local_admission(PathBuf::from("/remote/sub"), || {
                    committed = true;
                    Ok::<_, ()>(())
                })
                .is_err()
        );
        assert!(!committed);
        assert!(
            guard
                .with_local_admission(PathBuf::from("/local"), || Ok::<_, ()>(()))
                .unwrap()
                .is_ok()
        );
        assert!(
            guard
                .admit_pin(pin("machine", "second", "/local/sub"))
                .is_err()
        );
        assert!(
            guard
                .admit_pin(pin("machine", "second", "/remote"))
                .is_err()
        );
        guard
            .admit_pin(pin("other-machine", "second", "/remote"))
            .unwrap();
    }

    #[test]
    fn unknown_machine_refuses_remote_not_local_and_failed_save_adds_no_occupancy() {
        let unknown = MachineOverlapGuard::new(None, []);
        assert_eq!(
            unknown.admit_pin(pin("machine", "remote", "/remote")),
            Err(PlacementFailure::RunnerNotPaired)
        );
        assert!(
            unknown
                .with_local_admission(PathBuf::from("/remote"), || Ok::<_, ()>(()))
                .unwrap()
                .is_ok()
        );
        let guard = MachineOverlapGuard::new(Some("machine".into()), []);
        assert!(
            guard
                .with_local_admission(PathBuf::from("/workspace"), || Err::<(), _>(()))
                .unwrap()
                .is_err()
        );
        guard
            .admit_pin(pin("machine", "remote", "/workspace"))
            .unwrap();
    }
}
