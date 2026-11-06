//! Foreground authority ends at a newer transfer or an expired instance lease.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use plakous_runner_protocol::{LineOwner, RunnerFrame, StoppedForeground, TransferOwner};
use plakous_tools::OperationContext;
use plakous_tools::internal::runner::{RunnerVerb, ServeFuture, lost_process_owner};
use serde_json::Value;

use super::{RunnerService, lock};

type Key = (String, String);
type Gate = Arc<tokio::sync::Mutex<()>>;

#[derive(Default)]
pub(super) struct Authority {
    gates: Mutex<HashMap<Key, Gate>>,
    expiring: Mutex<HashSet<Key>>,
    reports: Mutex<HashMap<Key, Vec<StoppedForeground>>>,
}

impl Authority {
    fn gate(&self, host: &str, line: &str) -> Gate {
        Arc::clone(
            lock(&self.gates)
                .entry((host.to_owned(), line.to_owned()))
                .or_default(),
        )
    }

    pub(super) fn pending(&self) -> bool {
        !lock(&self.expiring).is_empty()
    }
}

impl RunnerService {
    pub(super) fn fenced_sessions(
        &self,
        host: &str,
        instance: &str,
        owner: Option<LineOwner>,
        verb: RunnerVerb,
        payload: Value,
        control: OperationContext,
    ) -> ServeFuture {
        let Some(owner) = owner else {
            return Box::pin(std::future::ready(Err(
                plakous_tools::internal::runner::ServeFailure::Unsupported,
            )));
        };
        let service = self.clone();
        let host = host.to_owned();
        let instance = instance.to_owned();
        Box::pin(async move {
            let gate = service.inner.foreground.gate(&host, &owner.line);
            let guard = if matches!(verb, RunnerVerb::ProcessStart | RunnerVerb::ProcessPromote) {
                Some(gate.lock().await)
            } else {
                None
            };
            if guard.is_some() && !lock(&service.inner.tables).held_by(&host, &instance, &owner) {
                return Err(lost_process_owner());
            }
            service
                .sessions(&host, &owner.line, verb, payload, control)
                .await
        })
    }

    async fn stop_line_foreground(&self, host: &str, line: &str) -> Result<(), String> {
        let sessions = lock(&self.inner.lines)
            .get(&(host.to_owned(), line.to_owned()))
            .map(|(sessions, _)| sessions.clone());
        if let Some(sessions) = sessions {
            let stopped = tokio::task::spawn_blocking(move || {
                plakous_tools::internal::stop_foreground_after_owner_loss(&sessions)
            })
            .await
            .map_err(|error| error.to_string())?;
            let reports = stopped
                .stopped
                .into_iter()
                .map(|stopped| StoppedForeground {
                    call_key: stopped.call_key,
                    pid: stopped.pid,
                    ended: format!("{:?}", stopped.status),
                })
                .collect::<Vec<_>>();
            if !reports.is_empty() {
                let mut stored = lock(&self.inner.foreground.reports);
                stored
                    .entry((host.to_owned(), line.to_owned()))
                    .or_default()
                    .extend(reports);
            }
            if let Some(failure) = stopped.failure {
                return Err(failure.to_string());
            }
        }
        Ok(())
    }

    fn foreground_report(&self, host: &str, line: &str) -> Vec<StoppedForeground> {
        lock(&self.inner.foreground.reports)
            .get(&(host.to_owned(), line.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    pub(super) async fn transfer_foreground(
        &self,
        host: &str,
        instance: &str,
        transfer: TransferOwner,
    ) -> RunnerFrame {
        let gate = self.inner.foreground.gate(host, &transfer.to.line);
        let _guard = gate.lock().await;
        if let Some(refusal) = lock(&self.inner.tables).transfer_refusal(
            host,
            instance,
            &transfer.control_id,
            transfer.from.as_ref(),
            &transfer.to,
        ) {
            return refusal;
        }
        let current = lock(&self.inner.tables).owner(host, &transfer.to.line);
        if current.is_some_and(|(owner, adopted)| owner != transfer.to || !adopted)
            && let Err(detail) = self.stop_line_foreground(host, &transfer.to.line).await
        {
            return RunnerFrame::TransferRefused {
                control_id: transfer.control_id,
                detail,
            };
        }
        let stopped = self.foreground_report(host, &transfer.to.line);
        let mut answer = lock(&self.inner.tables).transfer(
            host,
            instance,
            transfer.control_id,
            transfer.from.as_ref(),
            transfer.to,
        );
        if let RunnerFrame::TransferAccepted {
            stopped: report, ..
        } = &mut answer
        {
            *report = stopped;
        }
        answer
    }

    pub(super) fn expire_foreground(&self, host: String, instance: String, released: Instant) {
        let key = (host, instance);
        let (host, instance) = key.clone();
        if !lock(&self.inner.foreground.expiring).insert(key.clone()) {
            return;
        }
        let owners = {
            let mut tables = lock(&self.inner.tables);
            let owners = tables.held_lines(&host, &instance);
            tables.orphan(&host, &instance);
            owners
        };
        let service = self.clone();
        tokio::spawn(async move {
            let mut failed = false;
            for owner in owners {
                let gate = service.inner.foreground.gate(&host, &owner.line);
                let _guard = gate.lock().await;
                if lock(&service.inner.tables).owner(&host, &owner.line)
                    == Some((owner.clone(), false))
                    && service
                        .stop_line_foreground(&host, &owner.line)
                        .await
                        .is_err()
                {
                    failed = true;
                }
            }
            if !failed {
                let mut leases = lock(&service.inner.leases);
                if leases
                    .get(&key)
                    .is_some_and(|lease| lease.connections == 0 && lease.released == Some(released))
                {
                    leases.remove(&key);
                }
            }
            lock(&service.inner.foreground.expiring).remove(&key);
        });
    }
}
