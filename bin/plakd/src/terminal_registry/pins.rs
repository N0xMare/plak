//! Open reservations and VCS admission share one exclusion through the real future.

use super::{
    Arc, Entry, Mutex, Path, PathBuf, PoisonError, ReclamationAdmission, ResidencyHold, State,
    TerminalError, TerminalPin, TerminalRegistry, TerminalRow, TerminalStatus, VcsError, overlaps,
};

struct Reclaiming {
    state: Arc<Mutex<State>>,
    key: (String, PathBuf),
}

impl Drop for Reclaiming {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = state.reclaiming.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                state.reclaiming.remove(&self.key);
            }
        }
    }
}

pub(super) fn physical(runner: &str, path: &Path) -> Result<PathBuf, TerminalError> {
    super::placement::physical(runner, path).map_err(|_| TerminalError::Unavailable)
}

impl TerminalRegistry {
    pub(super) fn admit_local(
        &self,
        runner: &str,
        path: &Path,
    ) -> Result<Box<dyn ReclamationAdmission>, VcsError> {
        let unavailable = || VcsError::mint_unavailable("operator terminal retains workspace");
        let path = physical(runner, path).map_err(|_| unavailable())?;
        let mut state = self.lock();
        if state.entries.values().any(|entry| {
            entry.pinned
                && entry.pin.runner == runner
                && (overlaps(&entry.pin.workspace, &path)
                    || physical(runner, &entry.pin.workspace)
                        .map_or(true, |pin| overlaps(&pin, &path)))
        }) {
            return Err(unavailable());
        }
        let key = (runner.to_owned(), path);
        *state.reclaiming.entry(key.clone()).or_default() += 1;
        Ok(Box::new(Reclaiming {
            state: self.state.clone(),
            key,
        }))
    }
}

impl TerminalRegistry {
    pub(crate) fn retain_holds(
        &self,
        session: &str,
        count: &plakous_hosted::host_service::AttachmentCount,
    ) -> Result<(), String> {
        let mut state = self.lock();
        state.residents.insert(session.to_owned(), count.clone());
        for entry in state
            .entries
            .values_mut()
            .filter(|entry| entry.pinned && entry.pin.key.session.as_str() == session)
        {
            if entry.hold.is_none() {
                entry.hold = Some(count.hold().ok_or("terminal session releasing")?);
            }
        }
        Ok(())
    }

    pub(super) fn reserve(
        &self,
        pin: TerminalPin,
        hold: ResidencyHold,
    ) -> Result<String, TerminalError> {
        let identity = pin.identity().map_err(|_| TerminalError::Conflict)?;
        let mut state = self.lock();
        Self::collect_receipts(&mut state);
        if state.closing || state.entries.contains_key(&identity) {
            return Err(TerminalError::Busy);
        }
        let live: Vec<_> = state.entries.values().filter(|e| e.pinned).collect();
        if live.len() >= 16
            || live
                .iter()
                .filter(|e| e.pin.namespace == pin.namespace)
                .count()
                >= 4
            || live
                .iter()
                .filter(|e| e.pin.key.session == pin.key.session)
                .count()
                >= 2
        {
            return Err(TerminalError::Refused(super::OperatorRefusal::Quota));
        }
        let key = Self::receipt_key(&pin.generation, &pin.key)?;
        if let Some(fences) = state.fences.get(pin.namespace.principal.as_str()) {
            if let Some(prior) = fences.get(&key) {
                return Err(if prior == &pin.digest {
                    TerminalError::Lost
                } else {
                    TerminalError::Conflict
                });
            }
            if fences.len() >= 4096 {
                return Err(TerminalError::Refused(super::OperatorRefusal::Quota));
            }
        }
        if state
            .reclaiming
            .keys()
            .any(|(runner, path)| runner == &pin.runner && overlaps(path, &pin.workspace))
        {
            return Err(TerminalError::Busy);
        }
        state.entries.insert(
            identity.clone(),
            Entry {
                pin,
                pinned: true,
                lost: false,
                row: None,
                hold: Some(hold),
                completed: 0,
            },
        );
        Ok(identity)
    }

    pub(super) fn observe(&self, identity: &str, row: TerminalRow) -> Result<(), TerminalError> {
        let exited = matches!(row.status, TerminalStatus::Exited | TerminalStatus::Lost);
        {
            let mut state = self.lock();
            let entry = state.entries.get_mut(identity).ok_or(TerminalError::Lost)?;
            if row.key != entry.pin.key || row.next.instance.as_str() != entry.pin.instance {
                entry.lost = true;
                return Err(TerminalError::Lost);
            }
            entry.row = Some(row);
            entry.lost = entry.pin.host_generation != self.generation
                || entry.pin.generation != self.runner_generation;
        }
        if exited {
            self.release_proven(identity)?;
        }
        Ok(())
    }
}
