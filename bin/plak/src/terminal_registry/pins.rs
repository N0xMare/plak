//! Open reservations and VCS admission share one exclusion through the real future.

use super::{
    Arc, Entry, Mutex, Path, PathBuf, PoisonError, ReclamationAdmission, ReclamationFence,
    ResidencyHold, State, TerminalError, TerminalPin, TerminalRegistry, TerminalRow,
    TerminalStatus, VcsError, overlaps,
};

struct Reclaiming {
    _file: crate::store::terminal_pins::TerminalPinGuard,
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
    crate::store::terminal_pins::physical(runner, path).map_err(|_| TerminalError::Unavailable)
}

impl ReclamationFence for TerminalRegistry {
    fn admit(&self, runner: &str, path: &Path) -> Result<Box<dyn ReclamationAdmission>, VcsError> {
        let unavailable = || VcsError::mint_unavailable("operator terminal retains workspace");
        let path = physical(runner, path).map_err(|_| unavailable())?;
        let file = crate::store::terminal_pins::TerminalPinGuard::take(&self.paths)
            .map_err(|_| unavailable())?;
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
            _file: file,
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
        for entry in self
            .lock()
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
        let _file = crate::store::terminal_pins::TerminalPinGuard::take(&self.paths)
            .map_err(|_| TerminalError::Busy)?;
        let mut state = self.lock();
        if state.closing || state.entries.len() >= 1024 || state.entries.contains_key(&identity) {
            return Err(TerminalError::Busy);
        }
        if state
            .reclaiming
            .keys()
            .any(|(runner, path)| runner == &pin.runner && overlaps(path, &pin.workspace))
        {
            return Err(TerminalError::Busy);
        }
        // The FULL commit precedes spawn. Failure leaves no admitted child effect.
        self.store()?
            .reserve_terminal(&pin)
            .map_err(|_| TerminalError::Storage)?;
        state.entries.insert(
            identity.clone(),
            Entry {
                pin,
                pinned: true,
                lost: false,
                row: None,
                hold: Some(hold),
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
