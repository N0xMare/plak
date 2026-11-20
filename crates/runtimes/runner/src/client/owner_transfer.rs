//! Ordinary owner transfer and its connection-local cache.
use super::{
    HashMap, Inner, LineOwner, LineTransfer, RunnerFailure, RunnerFrame, Transferred, lock,
};
use plakous_runner_protocol::{StoppedForeground, UnpromotedProcess};
use std::sync::Arc;

impl Inner {
    pub(super) fn refuse_inspection(
        &self,
        id: &str,
        refusal: &plakous_runner_protocol::Refusal,
    ) -> bool {
        if let Some(waiter) = lock(&self.state).questions.remove(id) {
            let _ = waiter.send(Err(RunnerFailure::Unavailable(format!(
                "the runner refused the inspection: {refusal:?}"
            ))));
            return true;
        }
        false
    }

    /// Sends `owner`'s transfer before its first call, unless it is the
    /// owner last transferred for its line. A transfer from an owner the
    /// runner no longer accepts is answered with the one it does; a newer
    /// `owner` (a fresher lease) transfers again from that one, once.
    pub(super) async fn transfer(
        &self,
        owner: &LineOwner,
    ) -> Result<Vec<StoppedForeground>, RunnerFailure> {
        if self.qualification {
            return Err(RunnerFailure::NotSent(
                "qualification never transfers an owner".into(),
            ));
        }
        let (line, connection) = self.transfer_slot(owner);
        let mut transferred = line.lock().await;
        if transferred.owner.as_ref() == Some(owner) && transferred.connection == connection {
            return Ok(Vec::new());
        }
        let mut from = transferred.owner.clone();
        for _ in 0..2 {
            match self.send_transfer(from.clone(), owner).await? {
                RunnerFrame::TransferAccepted {
                    mut stopped,
                    unpromoted,
                    ..
                } => {
                    self.accept_transfer(
                        &mut transferred,
                        owner,
                        &mut stopped,
                        unpromoted,
                        connection,
                    );
                    return Ok(stopped);
                }
                RunnerFrame::StaleOwner { current, .. }
                    if owner.is_newer_than(&current) && from.as_ref() != Some(&current) =>
                {
                    from = Some(current);
                }
                RunnerFrame::TransferRefused { detail, .. } => {
                    return Err(RunnerFailure::Unavailable(detail));
                }
                _ => return Err(RunnerFailure::StaleOwner),
            }
        }
        Err(RunnerFailure::StaleOwner)
    }

    fn transfer_slot(&self, owner: &LineOwner) -> (Arc<tokio::sync::Mutex<LineTransfer>>, u64) {
        let (instance, connection) = {
            let state = lock(&self.state);
            (state.instance.clone(), state.connection)
        };
        let line = {
            let mut transferred = lock(&self.transferred);
            if transferred.instance != instance {
                *transferred = Transferred {
                    instance,
                    lines: HashMap::new(),
                };
            }
            Arc::clone(transferred.lines.entry(owner.line.clone()).or_default())
        };
        (line, connection)
    }

    fn accept_transfer(
        &self,
        transferred: &mut LineTransfer,
        owner: &LineOwner,
        stopped: &mut Vec<StoppedForeground>,
        unpromoted: Vec<UnpromotedProcess>,
        connection: u64,
    ) {
        let reporter = lock(&self.foreground_reporter).clone();
        if let Some(reporter) = reporter {
            let reports = stopped.clone();
            transferred
                .reported
                .retain(|key| reports.iter().any(|report| &report.call_key == key));
            stopped.retain(|process| transferred.reported.insert(process.call_key.clone()));
            for process in stopped.iter() {
                reporter(&owner.line, process);
            }
            for process in &reports {
                self.acknowledge_foreground(&owner.line, process);
            }
        }
        transferred.owner = Some(owner.clone());
        transferred.unpromoted = unpromoted;
        transferred.connection = connection;
    }
}
