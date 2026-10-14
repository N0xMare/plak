//! `/reload`: the installed-build probe runs off the UI thread.
//!
//! Asking the installed `plak` its state key spawns a process that may take
//! seconds on a first exec; the loop keeps painting and handing input while
//! it runs, then hands off (or prints the refusal) when the answer lands.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use super::{App, ScrollBlock};

pub(super) const RELOAD_CHECKING: &str = "checking installed build…";

/// What the loop does with a settled probe.
#[derive(Debug)]
pub(super) enum ReloadOutcome {
    HandOff(Option<crate::carry::ReloadTarget>),
    Refused,
}

impl App {
    /// Starts `probe` on its own thread unless one is already running.
    pub(super) fn start_reload_probe(
        &mut self,
        probe: impl FnOnce() -> Option<crate::carry::ReloadTarget> + Send + 'static,
    ) {
        if self.reload_probe.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("plak-reload-probe".to_owned())
            .spawn(move || {
                let _ = sender.send(probe());
            });
        if spawned.is_err() {
            self.push_block(ScrollBlock::System(
                "reload refused: could not start the installed-build check".to_owned(),
            ));
            self.mark_dirty();
            return;
        }
        self.reload_probe = Some(receiver);
        self.push_block(ScrollBlock::System(RELOAD_CHECKING.to_owned()));
        self.mark_dirty();
    }

    /// The probe's answer once it lands; `None` while it is still running.
    /// A refusal, including one for state that changed while it ran, is a
    /// transcript note and leaves the session open.
    pub(super) fn poll_reload_probe(&mut self) -> Option<ReloadOutcome> {
        let receiver: &Receiver<Option<crate::carry::ReloadTarget>> = self.reload_probe.as_ref()?;
        let target = match receiver.try_recv() {
            Ok(target) => target,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.reload_probe = None;
                self.push_block(ScrollBlock::System(
                    "reload refused: the installed-build check failed".to_owned(),
                ));
                self.mark_dirty();
                return Some(ReloadOutcome::Refused);
            }
        };
        self.reload_probe = None;
        self.mark_dirty();
        let refusal = self.reload_refusal().map(str::to_owned).or_else(|| {
            target
                .as_ref()
                .and_then(|target| target.refusal(&crate::home::state::state_key()))
        });
        if let Some(reason) = refusal {
            self.push_block(ScrollBlock::System(reason));
            return Some(ReloadOutcome::Refused);
        }
        Some(ReloadOutcome::HandOff(target))
    }
}
