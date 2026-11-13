//! The new native peer presents one job port. Foreign encodings end at the
//! frozen carrier; only this build's codec is used on its native connection.

use std::collections::BTreeMap;
use std::time::Duration;

use plakous_runner_protocol::retention as wire;
use plakous_runner_protocol::{Request, RequestClass, RunnerFrame};
use plakous_tools::OperationContext;
use plakous_tools::internal::runner::{RunnerVerb, operation_outcome_unknown};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use super::{RunnerService, call, lock, retention, scope};
use crate::service::answer_frame;

mod controls;

pub(super) fn unknown(id: String, detail: impl Into<String>) -> Box<RunnerFrame> {
    Box::new(answer_frame(
        id,
        Err(operation_outcome_unknown(&detail.into())),
    ))
}

impl RunnerService {
    pub(in crate::service) fn relay_jobs(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        request: &Request,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
    ) -> bool {
        let Some(owner) = &request.owner else {
            return false;
        };
        let Some(context) = self.retention_context() else {
            return false;
        };
        let entries = context.registry.entries(host, Some(&owner.line));
        if entries.is_empty() {
            return false;
        }
        let Some(verb) = RunnerVerb::parse(&request.operation.verb) else {
            return false;
        };
        let session = request
            .operation
            .payload
            .get("session")
            .and_then(Value::as_str);
        let remote_session = session.is_some_and(|session| {
            entries
                .iter()
                .any(|entry| entry.allocations.iter().any(|id| id.as_str() == session))
        });
        let aggregate = matches!(
            verb,
            RunnerVerb::JobsSnapshot
                | RunnerVerb::JobsWatch
                | RunnerVerb::JobsStop
                | RunnerVerb::JobsDisown
                | RunnerVerb::JobsAck
        );
        if !aggregate && !remote_session {
            return false;
        }
        let (host, instance, request, service, sender) = (
            host.to_owned(),
            instance.to_owned(),
            request.clone(),
            self.clone(),
            sender.clone(),
        );
        tokio::spawn(async move {
            let line = request
                .owner
                .as_ref()
                .map(|owner| owner.line.clone())
                .unwrap_or_default();
            let gate = service.inner.foreground.gate(&host, &line);
            let _guard = gate.lock().await;
            let Some(owner) = request.owner.as_ref() else {
                return;
            };
            if !service.delegated_owner(&host, &instance, owner) {
                let _ = sender.send(RunnerFrame::Refused {
                    request_id: Some(request.request_id),
                    refusal: plakous_runner_protocol::Refusal::OwnerNotTransferred,
                });
                return;
            }
            let result = if verb == RunnerVerb::JobsWatch {
                Ok(service.relay_watch(&host, &instance, connection, owner.clone()))
            } else {
                service
                    .relay_job_call(&host, &instance, &request, verb)
                    .await
            };
            let frame = match result {
                Ok(payload) => answer_frame(request.request_id, Ok(payload)),
                Err(frame) => *frame,
            };
            let _ = sender.send(frame);
        });
        true
    }

    async fn retained_jobs(
        &self,
        host: &str,
        instance: &str,
        line: &str,
    ) -> Result<Vec<wire::Job>, String> {
        let context = self
            .retention_context()
            .ok_or("missing retention registry")?;
        let scope = scope(host, instance)?;
        let mut jobs = BTreeMap::new();
        for entry in context.registry.entries(host, Some(line)) {
            if entry.confirmed && entry.inventory.jobs.is_empty() {
                continue;
            }
            if entry.inventory.lost {
                for job in entry.inventory.jobs {
                    jobs.insert(job.session.clone(), job);
                }
                continue;
            }
            let current = match self
                .refresh_retained(&scope, &entry.inventory.origin, &entry.to)
                .await
            {
                Ok(jobs) => jobs,
                Err(error) if crate::retention::confirmed_loss(&error) => {
                    context
                        .registry
                        .lose(&scope, &entry.inventory.origin, &entry.to)
                        .map_err(|error| error.to_string())?;
                    context
                        .registry
                        .entries(host, Some(line))
                        .into_iter()
                        .find(|changed| changed.inventory.origin == entry.inventory.origin)
                        .map_or_else(Vec::new, |changed| changed.inventory.jobs)
                }
                // A temporary disconnected response is not a terminal fact.
                // Keep the last snapshot charged until the original responds
                // or its endpoint/instance is known lost.
                Err(_) => entry.inventory.jobs,
            };
            for job in current {
                jobs.insert(job.session.clone(), job);
            }
        }
        Ok(jobs.into_values().collect())
    }

    async fn relay_snapshot(
        &self,
        host: &str,
        instance: &str,
        line: &str,
    ) -> Result<Value, String> {
        let mut jobs = BTreeMap::new();
        let local = lock(&self.inner.lines)
            .get(&(host.into(), line.into()))
            .map(|(sessions, _)| sessions.clone());
        if let Some(sessions) = local {
            for job in sessions.jobs() {
                let terminal = sessions
                    .session_for_number(job.number)
                    .is_some_and(|id| sessions.joined_terminal_status(id).is_some());
                let job = retention::job(job, terminal).map_err(|_| "invalid local job")?;
                jobs.insert(job.session.clone(), job);
            }
        }
        for job in self.retained_jobs(host, instance, line).await? {
            jobs.insert(job.session.clone(), job);
        }
        let jobs = jobs
            .values()
            .map(job_view)
            .collect::<Result<Vec<_>, String>>()?;
        Ok(json!({"instance":self.hello().instance,"jobs":jobs}))
    }

    fn relay_watch(
        &self,
        host: &str,
        instance: &str,
        connection: u64,
        owner: plakous_runner_protocol::LineOwner,
    ) -> Value {
        use std::sync::atomic::Ordering;
        let (reader, writer) = tokio::io::duplex(64 * 1024);
        let service = self.clone();
        let (host_owned, instance_owned) = (host.to_owned(), instance.to_owned());
        tokio::spawn(async move {
            service
                .relay_watch_pump(&host_owned, &instance_owned, &owner, writer)
                .await;
        });
        let stream = self.inner.next_stream.fetch_add(1, Ordering::Relaxed);
        self.carry(
            host,
            connection,
            stream,
            Box::pin(tokio::io::sink()),
            Box::pin(reader),
            None,
        );
        plakous_tools::internal::runner::service_spawned(stream)
    }

    pub(in crate::service) async fn relay_watch_pump(
        &self,
        host: &str,
        instance: &str,
        owner: &plakous_runner_protocol::LineOwner,
        mut writer: tokio::io::DuplexStream,
    ) {
        let mut last = Vec::new();
        let mut closed = [0u8];
        loop {
            if !self.delegated_owner(host, instance, owner) {
                return;
            }
            let snapshot = tokio::select! {
                _ = writer.read(&mut closed) => return,
                snapshot = self.relay_snapshot(host, instance, &owner.line) => snapshot,
            };
            let Ok(snapshot) = snapshot else { return };
            let Ok(mut bytes) = serde_json::to_vec(&snapshot) else {
                return;
            };
            bytes.push(b'\n');
            if bytes != last {
                if writer.write_all(&bytes).await.is_err() {
                    return;
                }
                last = bytes;
            }
            tokio::select! {
                _ = writer.read(&mut closed) => return,
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    async fn relay_job_call(
        &self,
        host: &str,
        instance: &str,
        request: &Request,
        verb: RunnerVerb,
    ) -> Result<Value, Box<RunnerFrame>> {
        let id = request.request_id.clone();
        let owner = request
            .owner
            .as_ref()
            .ok_or_else(|| unknown(id.clone(), "missing owner"))?;
        if verb == RunnerVerb::JobsSnapshot {
            return self
                .relay_snapshot(host, instance, &owner.line)
                .await
                .map_err(|error| unknown(id, error));
        }
        self.relay_control(host, instance, request, verb).await
    }

    async fn local_job_call(
        &self,
        host: &str,
        instance: &str,
        request: &Request,
        verb: RunnerVerb,
        payload: Value,
    ) -> Result<Value, Box<RunnerFrame>> {
        let mut local = request.clone();
        local.operation.payload = payload.clone();
        let key =
            match lock(&self.inner.tables).admit(host, instance, &local, std::time::Instant::now())
            {
                crate::state::Admission::Answer(frame) => {
                    return match frame {
                        RunnerFrame::Result { payload, .. } => Ok(payload),
                        other => Err(Box::new(other)),
                    };
                }
                crate::state::Admission::Run(key) => key,
            };
        let owner = request
            .owner
            .as_ref()
            .ok_or_else(|| unknown(request.request_id.clone(), "missing owner"))?;
        let result = self
            .sessions(
                host,
                &owner.line,
                verb,
                payload,
                OperationContext::new(plakous_model::CancellationToken::new()),
            )
            .await;
        let frame = answer_frame(request.request_id.clone(), result);
        let frame = if let Some(key) = key {
            lock(&self.inner.tables)
                .answer(host, &key, frame, std::time::Instant::now())
                .0
        } else {
            frame
        };
        match frame {
            RunnerFrame::Result { payload, .. } => Ok(payload),
            other => Err(Box::new(other)),
        }
    }
}

pub(super) fn job_view(job: &wire::Job) -> Result<Value, String> {
    let started = plakous_model::Timestamp::new(
        job.started_at_ms / 1000,
        u32::try_from(job.started_at_ms % 1000).unwrap_or(0) * 1_000_000,
    )
    .map_err(|_| "invalid job start")?;
    Ok(
        json!({"number":job.number,"command":job.command,"started_at":started,"owner":job.attribution,"elapsed_ms":job.elapsed_ms,"status":job.status,"output":job.output,"session_id":job.session,"owner_generation":job.owner_generation}),
    )
}

fn mutation(request: &Request) -> Result<wire::Mutation, Box<RunnerFrame>> {
    let key = match &request.class {
        RequestClass::OwnerControl { control_id } => wire::Key::Control {
            control_id: retention::id(control_id.clone())
                .map_err(|_| unknown(request.request_id.clone(), "invalid control id"))?,
        },
        RequestClass::EffectCall {
            call_id,
            attempt_id,
        } => wire::Key::Effect {
            call_id: retention::id(call_id.clone())
                .map_err(|_| unknown(request.request_id.clone(), "invalid call id"))?,
            attempt_id: retention::id(attempt_id.clone())
                .map_err(|_| unknown(request.request_id.clone(), "invalid attempt id"))?,
        },
        _ => {
            return Err(unknown(
                request.request_id.clone(),
                "job control requires a stable control key",
            ));
        }
    };
    Ok(wire::Mutation {
        key,
        digest: retention::id(request.digest.clone())
            .map_err(|_| unknown(request.request_id.clone(), "invalid operation digest"))?,
        replay: request.replay,
        age_ms: request.age_ms,
    })
}
