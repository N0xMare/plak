//! Job-only adapters into the original workers. No generic runner dispatch is
//! reachable here; the native operation journal is reused only for job controls.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plakous_runner_protocol::retention::{self as wire, Answer, Refusal};
use plakous_runner_protocol::{LineOwner, Operation, Request, RequestClass, RunnerFrame};
use plakous_tools::internal::runner::{RunnerVerb, hex, serve_sessions, unhex};
use plakous_tools::{
    JobStatus, JobStopReason, JoinedProcessOutputCursor, JoinedProcessSessionId,
    JoinedProcessSessions, JoinedProcessStatus, OperationContext,
};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::{RunnerService, lock};
use crate::state::Admission;

const TERMINAL_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Default)]
pub(super) struct State {
    terminals: Mutex<HashMap<(String, String), TerminalClock>>,
    pub(super) context: Mutex<Option<std::sync::Arc<Context>>>,
    maintenance: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.maintenance).take() {
            task.abort();
        }
    }
}

pub(super) struct Context {
    pub(super) registry: crate::retention::registry::Registry,
    pub(super) origin: wire::Origin,
}

#[derive(Default)]
struct TerminalClock {
    seen: HashSet<[u8; 16]>,
    last: Option<Instant>,
}

pub(crate) fn id(text: impl Into<String>) -> Result<wire::Id, Refusal> {
    wire::Id::try_from(text.into()).map_err(|_| Refusal::Malformed)
}

pub(crate) use crate::retention::native_owner;

pub(crate) fn owner(owner: &LineOwner) -> Result<wire::Owner, Refusal> {
    Ok(wire::Owner {
        line: id(owner.line.clone())?,
        generation: owner.generation,
        branch: id(owner.branch.clone())?,
        fence: owner.fence,
    })
}

pub(crate) fn job(view: plakous_tools::JobView, terminal: bool) -> Result<wire::Job, Refusal> {
    let status = match view.status {
        JobStatus::Running => wire::Status::Running {},
        JobStatus::Exited(code) => wire::Status::Exited { code },
        JobStatus::Stopped(reason) => wire::Status::Stopped {
            reason: stop_reason(reason),
        },
        JobStatus::Released => wire::Status::Released {},
        JobStatus::OutcomeUnknown => wire::Status::OutcomeUnknown {},
    };
    Ok(wire::Job {
        session: id(hex(&view.session_id))?,
        number: view.number,
        command: view.command,
        started_at_ms: view
            .started_at
            .seconds()
            .saturating_mul(1000)
            .saturating_add(u64::from(view.started_at.nanoseconds() / 1_000_000)),
        attribution: view.owner,
        elapsed_ms: u64::try_from(view.elapsed.as_millis()).unwrap_or(u64::MAX),
        status,
        terminal,
        output: view.output,
        owner_generation: view.owner_generation,
    })
}

fn stop_reason(reason: JobStopReason) -> wire::StopReason {
    match reason {
        JobStopReason::Model => wire::StopReason::Model,
        JobStopReason::User => wire::StopReason::User,
        JobStopReason::SessionExit => wire::StopReason::SessionExit,
        JobStopReason::AttemptEnded => wire::StopReason::AttemptEnded,
        JobStopReason::HardCap => wire::StopReason::HardCap,
    }
}

fn process_status(status: JoinedProcessStatus) -> wire::ProcessStatus {
    match status {
        JoinedProcessStatus::Running => wire::ProcessStatus::Running {},
        JoinedProcessStatus::Exited(code) => wire::ProcessStatus::Exited { code },
        JoinedProcessStatus::Released => wire::ProcessStatus::Released {},
        JoinedProcessStatus::Terminated => wire::ProcessStatus::Terminated {},
        JoinedProcessStatus::Cancelled => wire::ProcessStatus::Cancelled {},
        JoinedProcessStatus::TimedOut => wire::ProcessStatus::TimedOut {},
        JoinedProcessStatus::OutputLimit => wire::ProcessStatus::OutputLimit {},
        JoinedProcessStatus::Failed => wire::ProcessStatus::Failed {},
        JoinedProcessStatus::CleanupUnresolved => wire::ProcessStatus::CleanupUnresolved {},
        _ => wire::ProcessStatus::OutcomeUnknown {},
    }
}

fn session(
    sessions: &JoinedProcessSessions,
    text: &wire::Id,
) -> Result<JoinedProcessSessionId, Refusal> {
    if text.as_str().len() != 32 || unhex(text.as_str()).is_err() {
        return Err(Refusal::Malformed);
    }
    let job = sessions
        .jobs()
        .into_iter()
        .find(|job| hex(&job.session_id) == text.as_str())
        .ok_or(Refusal::UnknownJob)?;
    sessions
        .session_for_number(job.number)
        .ok_or(Refusal::UnknownJob)
}

fn sequence(jobs: &[wire::Job]) -> Result<u64, Refusal> {
    let bytes = serde_json::to_vec(jobs).map_err(|_| Refusal::Malformed)?;
    let digest = Sha256::digest(bytes);
    let mut first = [0; 8];
    first.copy_from_slice(&digest[..8]);
    Ok(u64::from_be_bytes(first))
}

impl RunnerService {
    pub(crate) fn retention_directory(
        &self,
        directory: &std::path::Path,
        origin: wire::Origin,
    ) -> std::io::Result<()> {
        let registry = crate::retention::registry::Registry::open(directory)?;
        *lock(&self.inner.retention.context) =
            Some(std::sync::Arc::new(Context { registry, origin }));
        let task = self.retention_maintenance();
        if let Some(old) = lock(&self.inner.retention.maintenance).replace(task) {
            old.abort();
        }
        Ok(())
    }

    pub(super) fn stop_retention_maintenance(&self) {
        if let Some(task) = lock(&self.inner.retention.maintenance).take() {
            task.abort();
        }
    }

    pub(super) fn retention_context(&self) -> Option<std::sync::Arc<Context>> {
        lock(&self.inner.retention.context).clone()
    }
    pub(crate) async fn retention_call(
        &self,
        scope: &wire::Bind,
        call: wire::Call,
        origin: &wire::Origin,
    ) -> Answer {
        match self.retention_answer(scope, &call, origin).await {
            Ok(answer) => answer,
            Err(reason) => Answer::Refused { reason },
        }
    }

    async fn retention_answer(
        &self,
        scope: &wire::Bind,
        call: &wire::Call,
        origin: &wire::Origin,
    ) -> Result<Answer, Refusal> {
        use wire::Request as R;
        self.retention_scope(scope, call, origin)?;
        let host = scope.host_id.as_str();
        let instance = scope.host_instance.as_str();
        let selected = match &call.request {
            R::Lines { after } => return self.retention_lines(host, origin, *after),
            R::Transfer { to, .. } => to,
            R::JobsSnapshot { owner, .. }
            | R::JobsWatch { owner, .. }
            | R::Read { owner, .. }
            | R::Poll { owner, .. }
            | R::Write { owner, .. }
            | R::Stop { owner, .. }
            | R::Disown { owner }
            | R::Ack { owner, .. } => owner,
        };
        let selected = native_owner(selected);
        let gate = self.inner.foreground.gate(host, &selected.line);
        let _guard = gate.lock().await;
        if matches!(&call.request, R::Transfer { .. }) {
            return self.retention_transfer(scope, call, &selected);
        }
        if !lock(&self.inner.tables).retained_by(host, instance, &selected) {
            return Err(Refusal::StaleOwner);
        }
        let found = lock(&self.inner.lines)
            .get(&(host.into(), selected.line.clone()))
            .cloned();
        let Some(found) = found else {
            return match &call.request {
                R::JobsSnapshot { .. } => Ok(Answer::Jobs {
                    jobs: Vec::new(),
                    next: None,
                    sequence: 0,
                    next_number: 1,
                }),
                R::JobsWatch { wait_ms, .. } if *wait_ms <= 1000 => Ok(Answer::Jobs {
                    jobs: Vec::new(),
                    next: None,
                    sequence: 0,
                    next_number: 1,
                }),
                R::Ack {
                    sessions, controls, ..
                } if sessions.is_empty() && controls.len() <= wire::PAGE_SIZE => {
                    for receipt in controls {
                        lock(&self.inner.tables).acknowledge_retained(
                            host,
                            instance,
                            &selected.line,
                            receipt.as_str(),
                        );
                    }
                    Ok(Answer::Ok {})
                }
                _ => Err(Refusal::UnknownJob),
            };
        };
        match &call.request {
            R::Write { .. } | R::Stop { .. } | R::Disown { .. } => {
                self.retention_mutation(scope, call, origin, selected, found)
                    .await
            }
            R::Ack { .. } => self.retention_ack(scope, call, &selected, &found.0),
            _ => observe(&found.0, &call.request).await,
        }
    }

    fn retention_scope(
        &self,
        scope: &wire::Bind,
        call: &wire::Call,
        origin: &wire::Origin,
    ) -> Result<(), Refusal> {
        use wire::Request as R;
        if !wire::admits(scope.version) {
            return Err(Refusal::IncompatibleVersion);
        }
        if call.instance != origin.instance {
            return Err(Refusal::DrainerLost);
        }
        let host = scope.host_id.as_str();
        if self
            .inner
            .hosts_allowed
            .as_ref()
            .is_some_and(|allow| !allow(host))
            || self
                .inner
                .home_local
                .as_ref()
                .is_some_and(|local| local != host)
        {
            return Err(Refusal::StaleOwner);
        }
        let mutates = matches!(
            &call.request,
            R::Write { .. } | R::Stop { .. } | R::Disown { .. }
        );
        if mutates != call.mutation.is_some() {
            return Err(Refusal::Malformed);
        }
        Ok(())
    }

    fn retention_transfer(
        &self,
        scope: &wire::Bind,
        call: &wire::Call,
        selected: &LineOwner,
    ) -> Result<Answer, Refusal> {
        use wire::Request as R;
        let host = scope.host_id.as_str();
        let instance = scope.host_instance.as_str();
        let selected = selected.clone();
        if let R::Transfer { from, .. } = &call.request {
            let from = native_owner(from);
            if let Some(refusal) = lock(&self.inner.tables).transfer_refusal(
                host,
                instance,
                call.id.as_str(),
                Some(&from),
                &selected,
            ) {
                return journal_answer(refusal, false);
            }
            // The host must settle turn-owned work through native authority
            // before surrender. A job-only transfer cannot stop foreground
            // work or strand its native-only terminal reports behind the ban.
            let unsettled = lock(&self.inner.lines)
                .get(&(host.into(), selected.line.clone()))
                .is_some_and(|(sessions, _)| {
                    !plakous_tools::internal::live_foreground(sessions).is_empty()
                });
            if unsettled || self.inner.foreground.has_reports(host, &selected.line) {
                return Err(Refusal::UnsettledForeground);
            }
            let answer = lock(&self.inner.tables).transfer_retained(
                host,
                instance,
                call.id.as_str().into(),
                &from,
                selected.clone(),
            );
            journal_answer(answer, false)?;
            let sessions = lock(&self.inner.lines)
                .get(&(host.into(), selected.line.clone()))
                .map(|(sessions, _)| sessions.clone());
            return Ok(Answer::Transferred {
                next_number: sessions
                    .as_ref()
                    .map_or(1, plakous_tools::internal::next_job_number),
                has_jobs: sessions.is_some_and(|sessions| !sessions.jobs().is_empty()),
            });
        }
        Err(Refusal::Malformed)
    }

    fn retention_ack(
        &self,
        scope: &wire::Bind,
        call: &wire::Call,
        selected: &LineOwner,
        sessions: &JoinedProcessSessions,
    ) -> Result<Answer, Refusal> {
        let host = scope.host_id.as_str();
        let instance = scope.host_instance.as_str();
        let wire::Request::Ack {
            sessions: ids,
            controls,
            ..
        } = &call.request
        else {
            return Err(Refusal::Malformed);
        };

        if ids.len() > wire::PAGE_SIZE || controls.len() > wire::PAGE_SIZE {
            return Err(Refusal::Malformed);
        }
        let mut numbers = Vec::new();
        for id in ids {
            let id = match session(sessions, id) {
                Ok(id) => id,
                Err(Refusal::UnknownJob) => continue,
                Err(error) => return Err(error),
            };
            // Repeated acknowledgement of an already consumed row is harmless.
            if !sessions.retains_session(id) {
                continue;
            }
            if !sessions.is_promoted(id) || sessions.joined_terminal_status(id).is_none() {
                return Err(Refusal::UnknownJob);
            }
            numbers.push(JoinedProcessSessions::job_number(id));
        }
        plakous_tools::internal::consume_jobs(sessions, &numbers);
        for receipt in controls {
            lock(&self.inner.tables).acknowledge_retained(
                host,
                instance,
                &selected.line,
                receipt.as_str(),
            );
        }
        Ok(Answer::Ok {})
    }

    async fn retention_mutation(
        &self,
        scope: &wire::Bind,
        call: &wire::Call,
        origin: &wire::Origin,
        selected: LineOwner,
        found: (JoinedProcessSessions, plakous_tools::ExecutionEnvironment),
    ) -> Result<Answer, Refusal> {
        let host = scope.host_id.as_str();
        let instance = scope.host_instance.as_str();
        let (sessions, environment) = found;

        let (verb, payload, stop) = mutation(&call.request)?;
        let meta = call.mutation.as_ref().ok_or(Refusal::Malformed)?;
        let class = match &meta.key {
            wire::Key::Control { control_id } => RequestClass::OwnerControl {
                control_id: control_id.as_str().into(),
            },
            wire::Key::Effect {
                call_id,
                attempt_id,
            } => RequestClass::EffectCall {
                call_id: call_id.as_str().into(),
                attempt_id: attempt_id.as_str().into(),
            },
        };
        let request = Request {
            request_id: call.id.as_str().into(),
            class,
            owner: Some(selected),
            deadline_ms: None,
            digest: meta.digest.as_str().into(),
            replay: meta.replay,
            age_ms: meta.age_ms,
            operation: Operation {
                verb: verb.as_str().into(),
                payload: payload.clone(),
            },
        };
        if !lock(&self.inner.tables).recorded_control(host, &request) {
            let selected = match &call.request {
                wire::Request::Write { session, .. } => Some(session),
                wire::Request::Stop { session, .. } => session.as_ref(),
                _ => None,
            };
            if let Some(selected) = selected
                && !sessions.is_promoted(session(&sessions, selected)?)
            {
                return Err(Refusal::UnknownJob);
            }
        }
        let key =
            match lock(&self.inner.tables).admit_retained(host, instance, &request, Instant::now())
            {
                Admission::Answer(answer) => return journal_answer(answer, stop),
                Admission::Run(key) => key.ok_or(Refusal::Malformed)?,
            };
        let answer = serve_sessions(
            &environment,
            &sessions,
            origin.instance.as_str(),
            verb.as_str(),
            payload,
            OperationContext::new(plakous_model::CancellationToken::new()),
            self.inner.home_local.is_some(),
        )
        .await;
        let answer = super::answer_frame(call.id.as_str().into(), answer);
        let (answer, _) = lock(&self.inner.tables).answer(host, &key, answer, Instant::now());
        journal_answer(answer, stop)
    }

    fn retention_lines(
        &self,
        host: &str,
        origin: &wire::Origin,
        after: usize,
    ) -> Result<Answer, Refusal> {
        let owners = lock(&self.inner.tables).retention_owners(host);
        let mut lines = Vec::new();
        for current in owners {
            let native_disabled = lock(&self.inner.tables).retention_only(host, &current.line);
            let sessions = lock(&self.inner.lines)
                .get(&(host.to_owned(), current.line.clone()))
                .map(|(sessions, _)| sessions.clone());
            let current = owner(&current)?;
            let Some(sessions) = sessions else {
                lines.push(wire::Line {
                    owner: current,
                    origin: origin.clone(),
                    jobs: Vec::new(),
                    next_number: 1,
                    lost: false,
                    native_disabled,
                });
                continue;
            };
            let next_number = plakous_tools::internal::next_job_number(&sessions);
            let jobs = sessions.jobs();
            if jobs.is_empty() {
                lines.push(wire::Line {
                    owner: current.clone(),
                    origin: origin.clone(),
                    jobs: Vec::new(),
                    next_number,
                    lost: false,
                    native_disabled,
                });
            }
            for view in jobs {
                lines.push(wire::Line {
                    owner: current.clone(),
                    origin: origin.clone(),
                    jobs: vec![job(
                        view.clone(),
                        sessions
                            .session_for_number(view.number)
                            .is_some_and(|id| sessions.joined_terminal_status(id).is_some()),
                    )?],
                    next_number,
                    lost: false,
                    native_disabled,
                });
            }
        }
        if let Some(context) = self.retention_context() {
            lines.extend(context.registry.inventory(host));
        }
        lines.sort_by(|a, b| {
            (&a.owner.line, a.jobs.first().map(|job| job.number))
                .cmp(&(&b.owner.line, b.jobs.first().map(|job| job.number)))
        });
        let (lines, next) = page(lines, after)?;
        Ok(Answer::Lines { lines, next })
    }

    pub(crate) fn retention_keeps_alive(&self, now: Instant) -> bool {
        let held: Vec<_> = lock(&self.inner.lines)
            .iter()
            .map(|(key, (sessions, _))| (key.clone(), sessions.clone()))
            .collect();
        let mut needed = false;
        for ((host, line), sessions) in held {
            if !lock(&self.inner.tables).retention_only(&host, &line) {
                continue;
            }
            let jobs = sessions.jobs();
            let terminal_times = plakous_tools::internal::job_terminal_times(&sessions);
            let mut times = lock(&self.inner.retention.terminals);
            let clock = times.entry((host, line)).or_default();
            let mut unsettled = false;
            for job in &jobs {
                let terminal = sessions
                    .session_for_number(job.number)
                    .is_some_and(|id| sessions.joined_terminal_status(id).is_some());
                if !terminal {
                    needed = true;
                    unsettled = true;
                } else if clock.seen.insert(job.session_id) {
                    let observed = terminal_times
                        .iter()
                        .find(|(id, _)| *id == job.session_id)
                        .map_or(now, |(_, instant)| *instant);
                    clock.last = Some(clock.last.map_or(observed, |last| last.max(observed)));
                }
            }
            let expired = !unsettled
                && clock
                    .last
                    .is_some_and(|last| now.saturating_duration_since(last) >= TERMINAL_RETENTION);
            drop(times);
            if !expired && sessions.has_unreported_terminal_job() {
                needed = true;
            }
            if expired {
                let terminal: Vec<_> = jobs
                    .iter()
                    .filter(|job| job.status != JobStatus::Running)
                    .map(|job| job.number)
                    .collect();
                plakous_tools::internal::consume_jobs(&sessions, &terminal);
            }
        }
        needed
    }
}

async fn observe(
    sessions: &JoinedProcessSessions,
    request: &wire::Request,
) -> Result<Answer, Refusal> {
    use wire::Request as R;
    match request {
        R::JobsSnapshot { after, .. } => snapshot(sessions, *after),
        R::JobsWatch {
            sequence: last,
            wait_ms,
            ..
        } => {
            if *wait_ms > 1000 {
                return Err(Refusal::Malformed);
            }
            let until = Instant::now() + Duration::from_millis(*wait_ms);
            loop {
                let answer = snapshot(sessions, 0)?;
                if !matches!(&answer, Answer::Jobs { sequence, .. } if sequence == last)
                    || Instant::now() >= until
                {
                    return Ok(answer);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        R::Read {
            session,
            cursor,
            max_bytes,
            ..
        } => read(sessions, session, *cursor, *max_bytes, 0).await,
        R::Poll {
            session,
            cursor,
            max_bytes,
            wait_ms,
            ..
        } => read(sessions, session, *cursor, *max_bytes, *wait_ms).await,
        _ => Err(Refusal::Malformed),
    }
}

fn snapshot(sessions: &JoinedProcessSessions, after: usize) -> Result<Answer, Refusal> {
    let mut jobs = sessions
        .jobs()
        .into_iter()
        .map(|view| {
            let terminal = sessions
                .session_for_number(view.number)
                .is_some_and(|id| sessions.joined_terminal_status(id).is_some());
            job(view, terminal)
        })
        .collect::<Result<Vec<_>, _>>()?;
    jobs.sort_by_key(|job| job.number);
    let sequence = sequence(&jobs)?;
    let (jobs, next) = page(jobs, after)?;
    Ok(Answer::Jobs {
        jobs,
        next,
        sequence,
        next_number: plakous_tools::internal::next_job_number(sessions),
    })
}

fn page<T: serde::Serialize>(
    rows: Vec<T>,
    after: usize,
) -> Result<(Vec<T>, Option<usize>), Refusal> {
    let total = rows.len();
    let mut bytes = 0;
    let mut selected = Vec::new();
    for row in rows.into_iter().skip(after).take(wire::PAGE_SIZE) {
        let encoded = serde_json::to_vec(&row)
            .map_err(|_| Refusal::Malformed)?
            .len();
        if encoded > wire::MAX_FRAME_BYTES / 2 {
            return Err(Refusal::Malformed);
        }
        if bytes + encoded > wire::MAX_FRAME_BYTES / 2 {
            break;
        }
        bytes += encoded;
        selected.push(row);
    }
    let through = after.saturating_add(selected.len());
    Ok((selected, (through < total).then_some(through)))
}

async fn read(
    sessions: &JoinedProcessSessions,
    text: &wire::Id,
    cursor: u64,
    max: usize,
    wait: u64,
) -> Result<Answer, Refusal> {
    if max == 0 || max > wire::MAX_BYTES || wait > 1000 {
        return Err(Refusal::Malformed);
    }
    let id = session(sessions, text)?;
    if !sessions.is_promoted(id) {
        return Err(Refusal::UnknownJob);
    }
    let until = Instant::now() + Duration::from_millis(wait);
    loop {
        let output = sessions
            .poll_from(
                id,
                JoinedProcessOutputCursor::new(cursor),
                max,
                wire::PAGE_SIZE,
            )
            .map_err(|_| Refusal::OutcomeUnknown)?;
        if !output.records().is_empty() || output.status().is_terminal() || Instant::now() >= until
        {
            let records = output
                .records()
                .iter()
                .map(|record| wire::Record {
                    sequence: record.sequence(),
                    cursor: record.cursor().get(),
                    stream: match record.stream() {
                        plakous_tools::JoinedProcessOutputStream::Stdout => "stdout",
                        plakous_tools::JoinedProcessOutputStream::Stderr => "stderr",
                        plakous_tools::JoinedProcessOutputStream::Terminal => "terminal",
                    }
                    .into(),
                    bytes: hex(record.bytes()),
                })
                .collect();
            return Ok(Answer::Output {
                records,
                next_cursor: output.next_cursor().get(),
                more_output: output.more_output(),
                skipped_bytes: output.skipped_bytes(),
                status: process_status(output.status()),
            });
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn mutation(request: &wire::Request) -> Result<(RunnerVerb, serde_json::Value, bool), Refusal> {
    use wire::Request as R;
    match request {
        R::Write {
            session,
            bytes,
            close,
            ..
        } => {
            if bytes.len() > wire::MAX_BYTES * 2 || unhex(bytes).is_err() {
                return Err(Refusal::Malformed);
            }
            Ok((
                RunnerVerb::ProcessWrite,
                json!({"session":session.as_str(),"bytes":bytes,"close":close}),
                false,
            ))
        }
        R::Stop {
            session,
            reason,
            grace_ms,
            ..
        } => {
            if *grace_ms > 5000 {
                return Err(Refusal::Malformed);
            }
            Ok((
                if session.is_some() {
                    RunnerVerb::ProcessStop
                } else {
                    RunnerVerb::JobsStop
                },
                json!({"session":session.as_ref().map(wire::Id::as_str),"reason":reason,"grace_ms":grace_ms}),
                session.is_some(),
            ))
        }
        R::Disown { .. } => Ok((RunnerVerb::JobsDisown, serde_json::Value::Null, false)),
        _ => Err(Refusal::Malformed),
    }
}

fn journal_answer(answer: RunnerFrame, stopped: bool) -> Result<Answer, Refusal> {
    match answer {
        RunnerFrame::Result { payload, .. } if stopped => Ok(Answer::Stopped {
            status: serde_json::from_value(payload).map_err(|_| Refusal::OutcomeUnknown)?,
        }),
        RunnerFrame::TransferAccepted { .. } | RunnerFrame::Result { .. } => Ok(Answer::Ok {}),
        RunnerFrame::StaleOwner { .. } | RunnerFrame::Refused { .. } => Err(Refusal::StaleOwner),
        RunnerFrame::TransferRefused { .. } => Err(Refusal::Persistence),
        RunnerFrame::StaleRequest { .. } => Err(Refusal::ConflictingControl),
        RunnerFrame::NotSeen { .. } => Err(Refusal::NotSeen),
        RunnerFrame::Spent { .. } => Err(Refusal::Spent),
        _ => Err(Refusal::OutcomeUnknown),
    }
}

#[cfg(test)]
mod tests;
