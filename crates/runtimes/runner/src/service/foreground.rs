//! Foreground authority ends at a newer transfer or an expired instance lease.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
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
    gates: Mutex<HashMap<Key, Weak<tokio::sync::Mutex<()>>>>,
    expiring: Mutex<HashSet<Key>>,
    reports: Mutex<HashMap<Key, Reports>>,
}

#[derive(Default)]
struct Reports {
    stopped: Vec<StoppedForeground>,
    delivered: HashSet<String>,
}

impl Authority {
    fn gate(&self, host: &str, line: &str) -> Gate {
        let mut gates = lock(&self.gates);
        gates.retain(|_, gate| gate.strong_count() != 0);
        let gate = gates.entry((host.to_owned(), line.to_owned())).or_default();
        if let Some(gate) = gate.upgrade() {
            return gate;
        }
        let fresh = Arc::new(tokio::sync::Mutex::new(()));
        *gate = Arc::downgrade(&fresh);
        fresh
    }

    pub(super) fn prune(&self) {
        lock(&self.gates).retain(|_, gate| gate.strong_count() != 0);
    }

    pub(super) fn pending(&self) -> bool {
        !lock(&self.expiring).is_empty() || !lock(&self.reports).is_empty()
    }

    pub(super) fn acknowledge(&self, host: &str, instance: &str, receipt: &str) {
        lock(&self.reports).retain(|(report_host, line), reports| {
            if report_host == host && reports.delivered.contains(instance) {
                reports.stopped.retain(|report| {
                    plakous_runner_protocol::foreground_report_id(line, report) != receipt
                });
            }
            !reports.stopped.is_empty()
        });
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
            let guard = if matches!(
                verb,
                RunnerVerb::ProcessStart | RunnerVerb::ProcessPromote | RunnerVerb::ForegroundStop
            ) {
                Some(gate.lock().await)
            } else {
                None
            };
            if guard.is_some()
                && (!service.instance_authority(&host, &instance)
                    || !lock(&service.inner.tables).held_by(&host, &instance, &owner))
            {
                return Err(lost_process_owner());
            }
            if verb == RunnerVerb::ForegroundStop {
                return service
                    .stop_matching_foreground(&host, &instance, &owner.line, payload)
                    .await;
            }
            service
                .sessions(&host, &owner.line, verb, payload, control)
                .await
        })
    }

    async fn stop_matching_foreground(
        &self,
        host: &str,
        instance: &str,
        line: &str,
        payload: Value,
    ) -> Result<Value, plakous_tools::internal::runner::ServeFailure> {
        use plakous_tools::internal::runner::ServeFailure;
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Stop {
            call_key: String,
            pid: u32,
        }
        let stop: Stop = serde_json::from_value(payload).map_err(|_| ServeFailure::Unsupported)?;
        let sessions = lock(&self.inner.lines)
            .get(&(host.to_owned(), line.to_owned()))
            .map(|(sessions, _)| sessions.clone());
        let Some(sessions) = sessions else {
            return Ok(serde_json::json!({"stopped": []}));
        };
        let service = self.clone();
        let (host, instance, line) = (host.to_owned(), instance.to_owned(), line.to_owned());
        tokio::task::spawn_blocking(move || {
            let stopped = plakous_tools::internal::stop_matching_foreground(
                &sessions,
                &stop.call_key,
                stop.pid,
            );
            let reports = stopped
                .stopped
                .into_iter()
                .map(|process| StoppedForeground {
                    call_key: process.call_key,
                    pid: process.pid,
                    ended: format!("{:?}", process.status),
                })
                .collect::<Vec<_>>();
            if !reports.is_empty() {
                let mut stored = lock(&service.inner.foreground.reports);
                let stored = stored.entry((host, line)).or_default();
                stored.delivered.insert(instance);
                for report in &reports {
                    if !stored.stopped.contains(report) {
                        stored.stopped.push(report.clone());
                    }
                }
            }
            if let Some(error) = stopped.failure {
                return Err(plakous_tools::internal::runner::serve_failure(&error));
            }
            Ok(serde_json::json!({"stopped": reports}))
        })
        .await
        .unwrap_or(Err(ServeFailure::Unsupported))
    }

    fn instance_authority(&self, host: &str, instance: &str) -> bool {
        lock(&self.inner.leases)
            .get(&(host.to_owned(), instance.to_owned()))
            .is_some_and(|lease| {
                lease.connections != 0
                    || lease.released.is_some_and(|at| {
                        self.lease_now().duration_since(at) < self.inner.owner_lease
                    })
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
                    .stopped
                    .extend(reports);
            }
            if let Some(failure) = stopped.failure {
                return Err(failure.to_string());
            }
        }
        Ok(())
    }

    fn foreground_report(
        &self,
        host: &str,
        instance: &str,
        owner: &LineOwner,
    ) -> Vec<StoppedForeground> {
        let mut reports = lock(&self.inner.foreground.reports);
        let key = (host.to_owned(), owner.line.clone());
        let Some(report) = reports.get_mut(&key) else {
            return Vec::new();
        };
        if report.stopped.is_empty() {
            reports.remove(&key);
            return Vec::new();
        }
        report.delivered.insert(instance.to_owned());
        report.stopped.clone()
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
        let owner = transfer.to.clone();
        let mut answer = lock(&self.inner.tables).transfer(
            host,
            instance,
            transfer.control_id,
            transfer.from.as_ref(),
            transfer.to,
        );
        if let RunnerFrame::TransferAccepted {
            stopped: report,
            unpromoted,
            ..
        } = &mut answer
        {
            *report = self.foreground_report(host, instance, &owner);
            if let Some((sessions, _)) =
                lock(&self.inner.lines).get(&(host.to_owned(), owner.line.clone()))
            {
                *unpromoted = plakous_tools::internal::live_foreground(sessions)
                    .into_iter()
                    .map(
                        |(call_key, pid)| plakous_runner_protocol::UnpromotedProcess {
                            call_key,
                            pid,
                        },
                    )
                    .collect();
                unpromoted.sort_by(|left, right| {
                    (&left.call_key, left.pid).cmp(&(&right.call_key, right.pid))
                });
            }
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

#[cfg(test)]
mod tests;
