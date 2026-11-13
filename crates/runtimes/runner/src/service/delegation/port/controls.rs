use plakous_runner_protocol::retention::{self as wire, Answer};
use plakous_runner_protocol::{Request, RunnerFrame};
use plakous_tools::internal::runner::RunnerVerb;
use serde_json::{Value, json};

use super::{RunnerService, call, mutation, retention, scope, unknown};
use crate::service::refused;

mod ack;
mod encoding;

impl RunnerService {
    pub(super) async fn relay_control(
        &self,
        host: &str,
        instance: &str,
        request: &Request,
        verb: RunnerVerb,
    ) -> Result<Value, Box<RunnerFrame>> {
        let fail = |detail: &str| unknown(request.request_id.clone(), detail);
        let context = self
            .retention_context()
            .ok_or_else(|| fail("missing retention registry"))?;
        let owner = request
            .owner
            .as_ref()
            .ok_or_else(|| fail("missing owner"))?;
        let scope = scope(host, instance).map_err(|error| fail(&error))?;
        let selected = retention::owner(owner).map_err(|_| fail("invalid owner"))?;
        let payload = &request.operation.payload;
        let session = payload.get("session").and_then(Value::as_str);
        let (numbers, local_numbers) = self
            .relay_ack_numbers(host, instance, request, verb)
            .await?;
        let entries = context.registry.entries(host, Some(&owner.line));
        let mut response = Value::Null;
        let aggregate =
            session.is_none() && matches!(verb, RunnerVerb::JobsStop | RunnerVerb::JobsDisown);
        let mut uncertain = None;
        for entry in &entries {
            let selected_session = session
                .and_then(|session| entry.allocations.iter().find(|id| id.as_str() == session));
            if session.is_some() && selected_session.is_none() {
                continue;
            }
            if entry.confirmed
                && entry.inventory.jobs.is_empty()
                && session.is_none()
                && !request.replay
            {
                continue;
            }
            if entry.inventory.lost {
                if verb == RunnerVerb::JobsAck {
                    context
                        .registry
                        .consume_lost(&scope, &entry.inventory.origin, &selected, &numbers)
                        .map_err(|error| fail(&error.to_string()))?;
                    self.account_retained();
                    continue;
                }
                let error = fail(
                    "original worker was lost; no PID adoption or replacement control is permitted",
                );
                if aggregate {
                    uncertain = Some(error);
                    continue;
                }
                return Err(error);
            }
            let remote = encoding::remote_request(
                verb,
                &selected,
                request,
                selected_session,
                &entry.inventory.jobs,
                &numbers,
                &entry.inventory.origin,
            )?;
            let mut failed = false;
            for part in acknowledgement_parts(remote) {
                let result = self
                    .relay_part(&scope, &entry.inventory.origin, request, &part)
                    .await;
                match result {
                    Ok(value) => response = value,
                    Err(error) if aggregate => {
                        uncertain = Some(error);
                        failed = true;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            if !failed && verb != RunnerVerb::ProcessPoll {
                // A later observation failure cannot erase a proven control
                // result. Read-only maintenance refreshes the cached inventory.
                let _ = self
                    .refresh_retained(&scope, &entry.inventory.origin, &selected)
                    .await;
            }
        }
        if session.is_none() {
            match self
                .relay_local_part(host, instance, request, verb, local_numbers)
                .await
            {
                Err(error) if aggregate => uncertain = Some(error),
                Err(error) => return Err(error),
                Ok(()) => {}
            }
        }
        match uncertain {
            Some(error) => Err(error),
            None => Ok(response),
        }
    }

    async fn relay_local_part(
        &self,
        host: &str,
        instance: &str,
        request: &Request,
        verb: RunnerVerb,
        numbers: Vec<u32>,
    ) -> Result<(), Box<RunnerFrame>> {
        if verb == RunnerVerb::JobsAck && numbers.is_empty() {
            return Ok(());
        }
        let payload = if verb == RunnerVerb::JobsAck {
            json!({"numbers":numbers})
        } else {
            request.operation.payload.clone()
        };
        // Every worker retains the same key separately. Replay never dispatches
        // an unseen local control after a receiver crash.
        self.local_job_call(host, instance, request, verb, payload)
            .await?;
        Ok(())
    }

    async fn relay_part(
        &self,
        scope: &wire::Bind,
        origin: &wire::Origin,
        request: &Request,
        part: &wire::Call,
    ) -> Result<Value, Box<RunnerFrame>> {
        let fail = |detail| unknown(request.request_id.clone(), detail);
        let context = self
            .retention_context()
            .ok_or_else(|| fail("missing retention registry".to_owned()))?;
        if part.mutation.is_some() {
            let owner = request
                .owner
                .as_ref()
                .ok_or_else(|| fail("missing owner".into()))?;
            let owner = retention::owner(owner).map_err(|_| fail("invalid owner".into()))?;
            context
                .registry
                .control_receipt(
                    scope,
                    origin,
                    &owner,
                    &part.id,
                    crate::retention::registry::ControlReceipt::Sent,
                )
                .map_err(|error| fail(error.to_string()))?;
        }
        let answer = context
            .registry
            .call(scope, origin, part)
            .await
            .map_err(|error| {
                fail(format!(
                    "original worker unavailable; dispatched control may have taken effect: {error}"
                ))
            })?;
        let response = native_answer(request, answer)?;
        if let wire::Request::Ack {
            owner, sessions, ..
        } = &part.request
        {
            context
                .registry
                .acknowledged(scope, origin, owner, sessions)
                .map_err(|error| fail(error.to_string()))?;
            self.account_retained();
        }
        Ok(response)
    }

    pub(in crate::service) fn relay_acknowledgement(
        &self,
        host: &str,
        instance: &str,
        request_id: &str,
    ) {
        let Some(context) = self.retention_context() else {
            return;
        };
        let Ok(scope) = scope(host, instance) else {
            return;
        };
        let (host, instance, request_id) =
            (host.to_owned(), instance.to_owned(), request_id.to_owned());
        tokio::spawn(async move {
            for entry in context
                .registry
                .control_targets(&host, &instance, &request_id)
            {
                let request = wire::Request::Ack {
                    owner: entry.to.clone(),
                    sessions: Vec::new(),
                    controls: match retention::id(request_id.clone()) {
                        Ok(id) => vec![id],
                        Err(_) => return,
                    },
                };
                let Ok(call) = call(&entry.inventory.origin, &request_id, request) else {
                    continue;
                };
                // Acknowledgement has no effect to replay; failure leaves the
                // original receipt retained for explicit same-key recovery.
                if matches!(
                    context
                        .registry
                        .call(&scope, &entry.inventory.origin, &call)
                        .await,
                    Ok(Answer::Ok {})
                ) {
                    let _ = context.registry.control_receipt(
                        &scope,
                        &entry.inventory.origin,
                        &entry.to,
                        &call.id,
                        crate::retention::registry::ControlReceipt::Acknowledged,
                    );
                }
            }
        });
    }
}

fn acknowledgement_parts(call: wire::Call) -> Vec<wire::Call> {
    let wire::Request::Ack {
        owner,
        sessions,
        controls,
    } = &call.request
    else {
        return vec![call];
    };
    if sessions.len() <= wire::PAGE_SIZE {
        return vec![call];
    }
    sessions
        .chunks(wire::PAGE_SIZE)
        .map(|sessions| wire::Call {
            request: wire::Request::Ack {
                owner: owner.clone(),
                sessions: sessions.to_vec(),
                controls: controls.clone(),
            },
            ..call.clone()
        })
        .collect()
}

fn native_answer(request: &Request, answer: Answer) -> Result<Value, Box<RunnerFrame>> {
    let id = request.request_id.clone();
    match answer {
        Answer::Ok {} => Ok(Value::Null),
        Answer::Stopped { status } => {
            serde_json::to_value(status).map_err(|_| unknown(id, "invalid retained stop outcome"))
        }
        Answer::Output {
            mut records,
            mut next_cursor,
            mut more_output,
            skipped_bytes,
            status,
        } => {
            if let Some(limit) = request
                .operation
                .payload
                .get("max_records")
                .and_then(Value::as_u64)
                .and_then(|limit| usize::try_from(limit).ok())
                && records.len() > limit
                && limit > 0
            {
                next_cursor = records[limit].cursor;
                records.truncate(limit);
                more_output = true;
            }
            Ok(
                json!({"records":records,"next_cursor":next_cursor,"more_output":more_output,"skipped_bytes":skipped_bytes,"status":status}),
            )
        }
        Answer::Refused {
            reason: wire::Refusal::NotSeen,
        } => Err(Box::new(RunnerFrame::NotSeen { request_id: id })),
        Answer::Refused {
            reason: wire::Refusal::Spent,
        } => Err(Box::new(RunnerFrame::Spent { request_id: id })),
        Answer::Refused {
            reason: wire::Refusal::ConflictingControl,
        } => Err(Box::new(RunnerFrame::StaleRequest { request_id: id })),
        Answer::Refused {
            reason: wire::Refusal::StaleOwner,
        } => Err(Box::new(RunnerFrame::Refused {
            request_id: Some(id),
            refusal: plakous_runner_protocol::Refusal::OwnerNotTransferred,
        })),
        other => Err(unknown(
            id,
            format!("original worker did not prove a job outcome: {other:?}"),
        )),
    }
}
