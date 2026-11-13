use plakous_runner_protocol::retention::{self as wire, Job, Owner};
use plakous_runner_protocol::{Request, RunnerFrame};
use plakous_tools::internal::runner::RunnerVerb;
use serde_json::Value;

use super::unknown;

pub(super) fn remote_request(
    verb: RunnerVerb,
    owner: &Owner,
    request: &Request,
    session: Option<&wire::Id>,
    jobs: &[Job],
    numbers: &[u32],
    origin: &wire::Origin,
) -> Result<wire::Call, Box<RunnerFrame>> {
    let payload = &request.operation.payload;
    let id = request.request_id.as_str();
    let fail = |detail: &str| unknown(id.into(), detail);
    let remote = match verb {
        RunnerVerb::ProcessPoll => wire::Request::Poll {
            owner: owner.clone(),
            session: session.ok_or_else(|| fail("unknown job"))?.clone(),
            cursor: payload
                .get("cursor")
                .and_then(Value::as_u64)
                .ok_or_else(|| fail("invalid cursor"))?,
            max_bytes: payload
                .get("max_bytes")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| fail("invalid byte bound"))?
                .min(wire::MAX_BYTES),
            wait_ms: payload
                .get("wait_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(1000),
        },
        RunnerVerb::ProcessWrite => wire::Request::Write {
            owner: owner.clone(),
            session: session.ok_or_else(|| fail("unknown job"))?.clone(),
            bytes: payload
                .get("bytes")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("invalid bytes"))?
                .into(),
            close: payload
                .get("close")
                .and_then(Value::as_bool)
                .ok_or_else(|| fail("invalid close"))?,
        },
        RunnerVerb::ProcessStop | RunnerVerb::JobsStop => wire::Request::Stop {
            owner: owner.clone(),
            session: session.cloned(),
            reason: serde_json::from_value(
                payload
                    .get("reason")
                    .cloned()
                    .ok_or_else(|| fail("missing stop reason"))?,
            )
            .map_err(|_| fail("invalid stop reason"))?,
            grace_ms: payload
                .get("grace_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| fail("invalid grace"))?,
        },
        RunnerVerb::JobsDisown => wire::Request::Disown {
            owner: owner.clone(),
        },
        RunnerVerb::JobsAck => wire::Request::Ack {
            owner: owner.clone(),
            sessions: jobs
                .iter()
                .filter(|job| numbers.contains(&job.number))
                .map(|job| job.session.clone())
                .collect(),
            controls: Vec::new(),
        },
        _ => return Err(fail("retained job operation is unsupported")),
    };
    let mut call = super::call(origin, id, remote).map_err(|error| fail(&error))?;
    if matches!(
        verb,
        RunnerVerb::ProcessWrite
            | RunnerVerb::ProcessStop
            | RunnerVerb::JobsStop
            | RunnerVerb::JobsDisown
    ) {
        call.mutation = Some(super::mutation(request)?);
    }
    Ok(call)
}
