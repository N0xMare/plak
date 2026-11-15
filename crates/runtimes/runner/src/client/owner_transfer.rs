//! Exact ownership-move CAS shares the ordinary connection and transfer cache.
use super::{
    HashMap, Inner, LineOwner, LineTransfer, RunnerClient, RunnerFailure, RunnerFrame, Transferred,
    lock, wire_owner,
};
use plakous_runner_protocol::{StoppedForeground, UnpromotedProcess};
use std::sync::Arc;

/// Unsupported host-move result. A mismatching witness is never adopted implicitly.
#[doc(hidden)]
#[derive(Debug)]
pub enum ExactOwnerTransfer {
    /// The requested owner was accepted, or its lost acknowledgement was recovered.
    Accepted(Vec<StoppedForeground>),
    /// The runner's actual owner; nothing was transferred or stopped.
    WitnessMismatch(LineOwner),
}

impl RunnerClient {
    /// Forgets only ephemeral witnesses for these exact lines of this host instance.
    ///
    /// # Errors
    /// Refuses non-normal binds, invalid line sets, lost reads or malformed replies.
    #[doc(hidden)]
    pub async fn forget_owner_witnesses(&self, lines: &[String]) -> Result<(), RunnerFailure> {
        if self.inner.qualification || self.inner.bind.inspect || !valid_floor_query(lines) {
            return Err(RunnerFailure::NotSent(
                "witness release requires a normal bind and 1..=256 unique line IDs".into(),
            ));
        }
        let (waiter, answer) = tokio::sync::oneshot::channel();
        let request_id = {
            let mut state = lock(&self.inner.state);
            let id = state
                .witness_releases
                .iter()
                .find(|(id, held)| held.as_slice() == lines && !state.questions.contains_key(*id))
                .map_or_else(
                    || self.inner.next_id("forget-witnesses"),
                    |(id, _)| id.clone(),
                );
            state.witness_releases.insert(id.clone(), lines.to_vec());
            state.questions.insert(id.clone(), waiter);
            id
        };
        let _question = FloorQuestion {
            inner: &self.inner,
            id: &request_id,
        };
        if !self
            .inner
            .send(plakous_runner_protocol::HostFrame::ForgetOwnerWitnesses {
                request_id: request_id.clone(),
                lines: lines.to_vec(),
            })
        {
            return Err(RunnerFailure::ReadInterrupted(
                "witness release was not sent".into(),
            ));
        }
        match tokio::time::timeout(self.inner.windows.read, answer).await {
            Ok(Ok(reply)) => match reply {
                Ok(super::InspectionAnswer::OwnerWitnessesForgotten) => Ok(()),
                Ok(
                    super::InspectionAnswer::Lines(_)
                    | super::InspectionAnswer::OwnerFloors(_)
                    | super::InspectionAnswer::JobTerminals { .. },
                ) => {
                    lock(&self.inner.state).witness_releases.remove(&request_id);
                    Err(RunnerFailure::ReadInterrupted(
                        "witness release reply was malformed".into(),
                    ))
                }
                Err(error) => Err(error),
            },
            Ok(Err(_)) | Err(_) => Err(RunnerFailure::ReadInterrupted(
                "witness release reply was lost".into(),
            )),
        }
    }

    /// Reads durable floors of explicitly named lines without taking authority.
    ///
    /// # Errors
    /// Refuses inspection/qualification, invalid requests, lost reads or malformed replies.
    #[doc(hidden)]
    pub async fn owner_floors(
        &self,
        lines: &[String],
    ) -> Result<Vec<plakous_runner_protocol::OwnerFloor>, RunnerFailure> {
        if self.inner.qualification || self.inner.bind.inspect || !valid_floor_query(lines) {
            return Err(RunnerFailure::NotSent(
                "owner-floor reads require a normal bind and 1..=256 unique line IDs".into(),
            ));
        }
        let request_id = self.inner.next_id("floors");
        let (waiter, answer) = tokio::sync::oneshot::channel();
        lock(&self.inner.state)
            .questions
            .insert(request_id.clone(), waiter);
        let _question = FloorQuestion {
            inner: &self.inner,
            id: &request_id,
        };
        let sent = self
            .inner
            .send(plakous_runner_protocol::HostFrame::OwnerFloors {
                request_id: request_id.clone(),
                lines: lines.to_vec(),
            });
        let reply = if sent {
            tokio::time::timeout(self.inner.windows.read, answer)
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        match reply {
            Some(Ok(super::InspectionAnswer::OwnerFloors(floors)))
                if floors.len() == lines.len()
                    && floors
                        .iter()
                        .map(|floor| floor.retained.len())
                        .sum::<usize>()
                        <= plakous_runner_protocol::OWNER_FLOOR_QUERY_LIMIT
                    && floors.iter().zip(lines).all(|(floor, line)| {
                        &floor.line == line
                            && floor.owner.as_ref().is_none_or(|owner| &owner.line == line)
                            && floor.retained.iter().all(|owner| &owner.line == line)
                    }) =>
            {
                Ok(floors)
            }
            Some(Err(error)) => Err(error),
            _ => Err(RunnerFailure::ReadInterrupted(
                "owner-floor read was lost or malformed".into(),
            )),
        }
    }

    /// Executes one exact same-line CAS, with no retry from a foreign witness.
    /// The host persists its intended owner before calling and checks its receipt on retry.
    ///
    /// # Errors
    /// Refuses qualification mode, a closed connection or a failed transfer.
    #[doc(hidden)]
    pub async fn transfer_owner_exact(
        &self,
        from: Option<&plakous_tools::LineOwner>,
        to: &plakous_tools::LineOwner,
    ) -> Result<ExactOwnerTransfer, RunnerFailure> {
        self.inner
            .transfer_exact(from.map(wire_owner), &wire_owner(to))
            .await
    }
}

struct FloorQuestion<'a> {
    inner: &'a Inner,
    id: &'a str,
}

impl Drop for FloorQuestion<'_> {
    fn drop(&mut self) {
        lock(&self.inner.state).questions.remove(self.id);
    }
}

pub(crate) fn valid_floor_query(lines: &[String]) -> bool {
    !lines.is_empty()
        && lines.len() <= plakous_runner_protocol::OWNER_FLOOR_QUERY_LIMIT
        && lines.iter().all(|line| {
            !line.is_empty()
                && line.len() <= 128
                && line.bytes().all(|byte| byte.is_ascii_graphic())
        })
        && lines
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == lines.len()
}

impl Inner {
    pub(super) fn refuse_inspection(
        &self,
        id: &str,
        refusal: &plakous_runner_protocol::Refusal,
    ) -> bool {
        let mut state = lock(&self.state);
        let release = state.witness_releases.remove(id).is_some();
        if let Some(waiter) = state.questions.remove(id) {
            let _ = waiter.send(Err(RunnerFailure::Unavailable(format!(
                "the runner refused the inspection: {refusal:?}"
            ))));
            return true;
        }
        release
    }

    pub(super) fn witnesses_forgotten(&self, request_id: &str) {
        let mut state = lock(&self.state);
        state.witness_releases.remove(request_id);
        if let Some(waiter) = state.questions.remove(request_id) {
            let _ = waiter.send(Ok(super::InspectionAnswer::OwnerWitnessesForgotten));
        }
    }

    pub(super) fn retry_witness_releases(&self) {
        let state = lock(&self.state);
        if let Some(sender) = &state.sender {
            for (request_id, lines) in &state.witness_releases {
                let _ = sender.send(plakous_runner_protocol::HostFrame::ForgetOwnerWitnesses {
                    request_id: request_id.clone(),
                    lines: lines.clone(),
                });
            }
        }
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

    async fn transfer_exact(
        &self,
        from: Option<LineOwner>,
        owner: &LineOwner,
    ) -> Result<ExactOwnerTransfer, RunnerFailure> {
        if self.qualification || self.bind.inspect {
            return Err(RunnerFailure::NotSent(
                "inspection and qualification never transfer an owner".into(),
            ));
        }
        let (line, connection) = self.transfer_slot(owner);
        let mut transferred = line.lock().await;
        match self.send_transfer(from, owner).await? {
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
                Ok(ExactOwnerTransfer::Accepted(stopped))
            }
            RunnerFrame::StaleOwner { current, .. } => {
                Ok(ExactOwnerTransfer::WitnessMismatch(current))
            }
            RunnerFrame::TransferRefused { detail, .. } => Err(RunnerFailure::Unavailable(detail)),
            _ => Err(RunnerFailure::StaleOwner),
        }
    }
}
