//! Only committed terminal driver operations authorize same-owner cleanup.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use plakous_model::{AttemptId, CallId};
use plakous_runner_protocol::{HostFrame, RequestClass, StoppedForeground, UnpromotedProcess};
use plakous_tools::LineOwner;
use plakous_tools::internal::runner::{RunnerCall, RunnerClass, RunnerFailure, RunnerVerb};

use super::{Inner, RunnerClient, lock, wire_owner};

#[derive(Default)]
pub(super) struct Calls {
    calls: Mutex<HashMap<(String, String), Tracked>>,
    gates: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
struct Tracked {
    owner: LineOwner,
    call_key: String,
    settled: bool,
}

impl Calls {
    pub(super) fn gate(&self, line: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = lock(&self.gates);
        gates.retain(|_, gate| gate.strong_count() != 0);
        let gate = gates.entry(line.into()).or_default();
        if let Some(gate) = gate.upgrade() {
            return gate;
        }
        let fresh = Arc::new(tokio::sync::Mutex::new(()));
        *gate = Arc::downgrade(&fresh);
        fresh
    }

    pub(super) fn register(&self, call: &RunnerCall) {
        if call.verb() != RunnerVerb::ProcessStart {
            return;
        }
        let (
            Some(owner),
            RunnerClass::Effect {
                call_id,
                attempt_id,
            },
        ) = (call.owner(), call.class())
        else {
            return;
        };
        let call_key = format!("{call_id}:{attempt_id}");
        lock(&self.calls)
            .entry((owner.line().to_string(), call_key.clone()))
            .or_insert(Tracked {
                owner: *owner,
                call_key,
                settled: false,
            });
    }

    pub(super) fn settled(&self, owner: LineOwner, call: CallId, attempt: AttemptId) -> bool {
        let key = (owner.line().to_string(), format!("{call}:{attempt}"));
        if let Some(tracked) = lock(&self.calls).get_mut(&key)
            && tracked.owner == owner
        {
            tracked.settled = true;
            return true;
        }
        false
    }

    fn settled_on(&self, owner: LineOwner) -> Vec<Tracked> {
        lock(&self.calls)
            .values()
            .filter(|call| call.owner == owner && call.settled)
            .cloned()
            .collect()
    }

    fn forget(&self, call: &Tracked) {
        lock(&self.calls).remove(&(call.owner.line().to_string(), call.call_key.clone()));
    }
}

impl RunnerClient {
    /// Acknowledges a consumed transfer report. Without a diagnostic sink,
    /// callers acknowledge only after recording or presenting the report.
    pub fn acknowledge_stopped_foreground(&self, line: &str, report: &StoppedForeground) {
        self.inner.acknowledge_foreground(line, report);
    }

    /// After journal recovery/recorded stops, cleans only foreground calls
    /// whose owning driver committed terminal operation state. Live operations
    /// remain running even when their process.start key was answered/Acked.
    ///
    /// # Errors
    /// Pending start recovery, ownership or keyed cleanup remains unresolved.
    pub async fn reconcile_settled_foreground(&self) -> Result<(), RunnerFailure> {
        let owners = lock(&self.inner.foreground.calls)
            .values()
            .filter(|call| call.settled)
            .map(|call| call.owner)
            .collect::<HashSet<_>>();
        for owner in owners {
            let gate = self.inner.foreground.gate(&owner.line().to_string());
            let _guard = gate.lock().await;
            self.inner.cleanup_foreground(owner).await?;
        }
        Ok(())
    }
}

impl Inner {
    pub(super) async fn unpromoted(
        &self,
        owner: &plakous_runner_protocol::LineOwner,
    ) -> Result<Vec<UnpromotedProcess>, RunnerFailure> {
        let line = lock(&self.transferred).lines.get(&owner.line).cloned();
        if let Some(line) = line {
            line.lock().await.connection = 0;
        }
        self.transfer(owner).await?;
        let line = lock(&self.transferred).lines.get(&owner.line).cloned();
        let Some(line) = line else {
            return Ok(Vec::new());
        };
        let result = line.lock().await.unpromoted.clone();
        Ok(result)
    }

    pub(super) fn cleanup_foreground(
        &self,
        owner: LineOwner,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), RunnerFailure>> + Send + '_>>
    {
        Box::pin(async move {
            let calls = self.foreground.settled_on(owner);
            if calls.is_empty() {
                return Ok(());
            }
            if lock(&self.state).sender.is_none() {
                return Err(RunnerFailure::Unavailable(
                    "foreground reconciliation awaits the runner".into(),
                ));
            }
            let deadline = tokio::time::Instant::now() + self.windows.effect;
            loop {
                let pending = lock(&self.state).pending.values().any(|pending| {
                pending.request.operation.verb == RunnerVerb::ProcessStart.as_str()
                    && pending.request.owner.as_ref().is_some_and(|pending_owner| pending_owner.line == owner.line().to_string())
                    && matches!(&pending.request.class, RequestClass::EffectCall {call_id, attempt_id} if calls.iter().any(|call| call.call_key == format!("{call_id}:{attempt_id}")))
            });
                if !pending {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(RunnerFailure::OutcomeUnknown(
                        "settled foreground start still awaits keyed recovery".into(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let processes = match self.unpromoted(&wire_owner(&owner)).await {
                Ok(processes) => processes,
                Err(RunnerFailure::StaleOwner) => {
                    for call in &calls {
                        self.foreground.forget(call);
                    }
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            for call in calls {
                if let Some(process) = processes
                    .iter()
                    .find(|process| process.call_key == call.call_key)
                {
                    let result = Box::pin(self.call(RunnerCall::recorded_control(
                        RunnerVerb::ForegroundStop,
                        serde_json::json!({"call_key": process.call_key, "pid": process.pid}),
                        owner,
                        format!("foreground-cleanup:{}:{}", owner.line(), process.call_key),
                    )))
                    .await;
                    match result {
                        Ok(value) => {
                            let reports: Vec<StoppedForeground> = serde_json::from_value(
                                value.get("stopped").cloned().unwrap_or_default(),
                            )
                            .map_err(|_| {
                                RunnerFailure::Unavailable(
                                    "invalid foreground cleanup answer".into(),
                                )
                            })?;
                            self.report_cleanup(&owner.line().to_string(), reports)
                                .await;
                        }
                        Err(RunnerFailure::Spent) => {}
                        Err(error) => return Err(error),
                    }
                }
                self.foreground.forget(&call);
            }
            Ok(())
        })
    }

    pub(super) fn acknowledge_foreground(&self, line: &str, report: &StoppedForeground) {
        self.send(HostFrame::Ack {
            request_id: plakous_runner_protocol::foreground_report_id(line, report),
        });
    }

    pub(super) async fn report_cleanup(&self, line: &str, reports: Vec<StoppedForeground>) {
        let transferred = lock(&self.transferred).lines.get(line).cloned();
        let Some(transferred) = transferred else {
            return;
        };
        let mut transferred = transferred.lock().await;
        let reporter = lock(&self.foreground_reporter).clone();
        for report in reports {
            if let Some(reporter) = &reporter {
                if transferred.reported.insert(report.call_key.clone()) {
                    reporter(line, &report);
                }
                self.acknowledge_foreground(line, &report);
            }
        }
    }
}
