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
    durable: Option<DurableAdmission>,
}

/// Fresh durable occupancy and pairing evidence, read under the shared lock.
pub struct AdmissionSnapshot {
    /// Every non-deleted local root and minted local copy.
    pub local: BTreeSet<PathBuf>,
    /// Configured pairing pins, including offline runners.
    pub pins: Vec<RunnerPin>,
}

struct DurableAdmission {
    lock: PathBuf,
    read: Box<dyn Fn() -> Result<AdmissionSnapshot, PlacementFailure> + Send + Sync>,
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
            durable: None,
        }
    }

    /// Shares pair/bind and local-create serialization across home processes.
    /// The snapshot reader must not acquire this lock or perform effects.
    #[must_use]
    pub fn with_admission_store(
        mut self,
        lock: PathBuf,
        read: impl Fn() -> Result<AdmissionSnapshot, PlacementFailure> + Send + Sync + 'static,
    ) -> Self {
        self.durable = Some(DurableAdmission {
            lock,
            read: Box::new(read),
        });
        self
    }

    fn refresh(&self, guarded: &mut Guarded) -> Result<Option<std::fs::File>, PlacementFailure> {
        use std::os::unix::fs::OpenOptionsExt;
        let Some(durable) = &self.durable else {
            return Ok(None);
        };
        let failure = || PlacementFailure::RunnerUnreachable {
            phase: crate::ProbePhase::Reconcile,
            timed_out: false,
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&durable.lock)
            .map_err(|_| failure())?;
        file.lock().map_err(|_| failure())?;
        let snapshot = (durable.read)()?;
        guarded.local = snapshot.local;
        guarded.pins = snapshot
            .pins
            .into_iter()
            .map(|pin| (pin.runner_id.clone(), pin))
            .collect();
        Ok(Some(file))
    }

    /// Restores/reconfirms a pin. Same-machine roots cannot intersect another
    /// authority's admitted roots, local records or minted copies.
    ///
    /// # Errors
    /// Unknown identity or overlapping roots; no pin changes on refusal.
    pub fn admit_pin(&self, pin: RunnerPin) -> Result<(), PlacementFailure> {
        self.with_pin_admission(pin, || Ok(()))
    }

    /// Checks a pin and holds cross-process admission ownership through its
    /// durable commit. A failed commit does not publish new pin evidence.
    ///
    /// # Errors
    /// Unknown machine evidence, overlap, storage failure or commit refusal.
    pub fn with_pin_admission(
        &self,
        pin: RunnerPin,
        commit: impl FnOnce() -> Result<(), PlacementFailure>,
    ) -> Result<(), PlacementFailure> {
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
        let _admission = self.refresh(&mut guarded)?;
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
        commit()?;
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
        let _admission = self.refresh(&mut guarded)?;
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
#[allow(clippy::unwrap_used, reason = "isolated admission fixtures")]
mod tests {
    use super::*;

    #[test]
    fn independent_home_process_guards_serialize_snapshot_and_commit() {
        let directory = tempfile::tempdir().unwrap();
        let lock_path = directory.path().join("admission.lock");
        let snapshot = std::sync::Arc::new(Mutex::new(BTreeSet::<PathBuf>::new()));
        let guard = || {
            let snapshot = snapshot.clone();
            MachineOverlapGuard::new(Some("machine".into()), []).with_admission_store(
                lock_path.clone(),
                move || {
                    Ok(AdmissionSnapshot {
                        local: snapshot.lock().unwrap().clone(),
                        pins: Vec::new(),
                    })
                },
            )
        };
        let first = guard();
        let second = guard();
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let commit = std::thread::spawn(move || {
            first
                .with_local_admission(PathBuf::from("/workspace"), || {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    snapshot.lock().unwrap().insert(PathBuf::from("/workspace"));
                    Ok::<_, ()>(())
                })
                .unwrap()
                .unwrap();
        });
        waiting.recv().unwrap();
        let competing = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        assert!(
            competing.try_lock().is_err(),
            "the product commit must retain cross-process admission ownership"
        );
        let pair = std::thread::spawn(move || {
            second.admit_pin(pin("machine", "remote", "/workspace/sub"))
        });
        release.send(()).unwrap();
        commit.join().unwrap();
        assert!(
            matches!(
                pair.join().unwrap(),
                Err(PlacementFailure::RunnerRootsOverlap { .. })
            ),
            "the later pair must re-read the newly committed durable occupancy"
        );
    }

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
