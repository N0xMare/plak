//! The runner's owner table and op table (spec 01 §8.4): which owner of each
//! line it accepts, and what each effect-call key has done. Pure state; the
//! service drives it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use plakous_runner_protocol::{LineOwner, Refusal, Request, RequestClass, RunnerFrame};

/// How long an answered or tombstoned key is kept without an acknowledgement.
pub(crate) const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// One effect-call key: the line, its generation, the call and its attempt.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OpKey {
    line: String,
    generation: u64,
    call_id: String,
    attempt_id: String,
}

#[derive(Debug)]
enum OpState {
    /// Running under this request id; its answer goes to the host's current
    /// connection.
    Running,
    /// Ran; the answer is kept until the host acknowledges it.
    Answered(Box<RunnerFrame>),
    /// A replay found the key unseen: nothing ran, and nothing will.
    Tombstone,
    /// Answered and acknowledged: the key is spent.
    Spent,
}

#[derive(Debug)]
struct Op {
    digest: String,
    request_id: String,
    state: OpState,
    at: Instant,
}

/// What to do with one request.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    /// Run it; for an effect call, under this key.
    Run(Option<OpKey>),
    /// Answer at once with this frame; nothing runs.
    Answer(RunnerFrame),
}

/// The owner and op tables of one runner instance, for every host.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    owners: HashMap<(String, String), LineOwner>,
    ops: HashMap<(String, OpKey), Op>,
}

impl Tables {
    /// Records an owner change. The new owner must be the line's accepted
    /// owner or a newer one; the accepted owner again is a retry and is
    /// accepted.
    pub(crate) fn transfer(
        &mut self,
        host: &str,
        control_id: String,
        to: LineOwner,
    ) -> RunnerFrame {
        let line = (host.to_owned(), to.line.clone());
        if let Some(current) = self.owners.get(&line)
            && !to.is_at_least(current)
        {
            return RunnerFrame::StaleOwner {
                request_id: None,
                control_id: Some(control_id),
                current: current.clone(),
            };
        }
        self.owners.insert(line, to);
        RunnerFrame::TransferAccepted { control_id }
    }

    /// Admits one request, at most once per effect-call key. A key already
    /// held is answered from the table whatever its owner, since the call it
    /// names may have run; the owner fence applies only to work that has not.
    pub(crate) fn admit(&mut self, host: &str, request: &Request, now: Instant) -> Admission {
        self.ops.retain(|_, op| {
            matches!(op.state, OpState::Running) || now.duration_since(op.at) < RETENTION
        });
        let id = request.request_id.clone();
        let key = match (&request.class, &request.owner) {
            (
                RequestClass::EffectCall {
                    call_id,
                    attempt_id,
                },
                Some(owner),
            ) => Some(OpKey {
                line: owner.line.clone(),
                generation: owner.generation,
                call_id: call_id.clone(),
                attempt_id: attempt_id.clone(),
            }),
            (RequestClass::EffectCall { .. }, None) => {
                return Admission::Answer(RunnerFrame::Refused {
                    request_id: Some(id),
                    refusal: Refusal::Malformed {
                        detail: "an effect call names its owner".into(),
                    },
                });
            }
            _ => None,
        };
        if let Some(key) = &key
            && let Some(op) = self.ops.get_mut(&(host.to_owned(), key.clone()))
        {
            if op.digest != request.digest {
                return Admission::Answer(RunnerFrame::StaleRequest { request_id: id });
            }
            return Admission::Answer(match &op.state {
                OpState::Running => {
                    op.request_id.clone_from(&id);
                    RunnerFrame::InProgress { request_id: id }
                }
                OpState::Answered(answer) => {
                    op.request_id.clone_from(&id);
                    with_request_id((**answer).clone(), id)
                }
                OpState::Tombstone => RunnerFrame::NotSeen { request_id: id },
                OpState::Spent => RunnerFrame::Spent { request_id: id },
            });
        }
        if let Some(owner) = &request.owner {
            match self.owners.get(&(host.to_owned(), owner.line.clone())) {
                Some(current) if current == owner => {}
                Some(current) if !owner.is_newer_than(current) => {
                    return Admission::Answer(RunnerFrame::StaleOwner {
                        request_id: Some(id),
                        control_id: None,
                        current: current.clone(),
                    });
                }
                _ => {
                    return Admission::Answer(RunnerFrame::Refused {
                        request_id: Some(id),
                        refusal: Refusal::OwnerNotTransferred,
                    });
                }
            }
        }
        let Some(key) = key else {
            return Admission::Run(None);
        };
        let state = if request.replay {
            OpState::Tombstone
        } else {
            OpState::Running
        };
        self.ops.insert(
            (host.to_owned(), key.clone()),
            Op {
                digest: request.digest.clone(),
                request_id: id.clone(),
                state,
                at: now,
            },
        );
        if request.replay {
            Admission::Answer(RunnerFrame::NotSeen { request_id: id })
        } else {
            Admission::Run(Some(key))
        }
    }

    /// The key a running or answered request id names, if it is keyed.
    pub(crate) fn key_for(&self, host: &str, request_id: &str) -> Option<OpKey> {
        self.ops
            .iter()
            .find(|((op_host, _), op)| op_host == host && op.request_id == request_id)
            .map(|((_, key), _)| key.clone())
    }

    /// Records a keyed request's answer and returns it addressed to the
    /// request id the host is waiting under now.
    pub(crate) fn answer(
        &mut self,
        host: &str,
        key: &OpKey,
        answer: RunnerFrame,
        now: Instant,
    ) -> RunnerFrame {
        let Some(op) = self.ops.get_mut(&(host.to_owned(), key.clone())) else {
            return answer;
        };
        let answer = with_request_id(answer, op.request_id.clone());
        op.state = OpState::Answered(Box::new(answer.clone()));
        op.at = now;
        answer
    }

    /// The host recorded `request_id`'s answer: its key is spent.
    pub(crate) fn acknowledge(&mut self, host: &str, request_id: &str) {
        for ((op_host, _), op) in &mut self.ops {
            if op_host == host
                && op.request_id == request_id
                && matches!(op.state, OpState::Answered(_))
            {
                op.state = OpState::Spent;
            }
        }
    }
}

fn with_request_id(frame: RunnerFrame, id: String) -> RunnerFrame {
    match frame {
        RunnerFrame::Result { payload, .. } => RunnerFrame::Result {
            request_id: id,
            payload,
        },
        RunnerFrame::Error { error, .. } => RunnerFrame::Error {
            request_id: id,
            error,
        },
        other => other,
    }
}
