//! The runner's owner table and op table (spec 01 §8.4): which owner of each
//! line it accepts, and what each effect-call and owner-control key has done.
//! Observations are fenced but never recorded. Pure state; the service drives
//! it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_runner_protocol::{LineOwner, Refusal, Request, RequestClass, RunnerFrame};

use crate::floors::{Floor, OwnerFloors};

/// How long an answered or tombstoned key is kept without an acknowledgement.
pub(crate) const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// How much a replay's age, measured on the host's wall clock, may have been
/// shortened by that clock stepping back: a replay this close to
/// [`RETENTION`] is already expired.
pub(crate) const CLOCK_MARGIN: Duration = Duration::from_secs(60 * 60);

/// One recorded key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum OpKey {
    /// An effect call: the line, its generation, the call, its attempt and
    /// the verb, since one call may run several operations (a start, then
    /// writes to what it started).
    Effect {
        line: String,
        generation: u64,
        call_id: String,
        attempt_id: String,
        verb: String,
    },
    /// An owner control of the line's processes.
    Control { line: String, control_id: String },
    /// A tree operation's step, by the tree, the operation and the verb.
    Tree {
        tree_id: String,
        operation_id: String,
        verb: String,
    },
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
    /// The request id and the host instance waiting for the key's answer:
    /// the last ones that sent it.
    request_id: String,
    instance: String,
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

/// The owner a line accepts, the host instance holding it, and whether that
/// instance holds it now.
#[derive(Clone, Debug)]
struct Owned {
    owner: LineOwner,
    /// False once the holder's owner lease expired or the runner restarted,
    /// until a transfer re-adopts the line: by the holder, of the same owner
    /// again, or by any instance of its host, of a newer one. Calls of an
    /// orphaned line are refused until then.
    adopted: bool,
    /// The host instance whose transfer was last accepted for the line: the
    /// only one whose effect calls and controls it accepts, and whose own
    /// lease orphans it. After a restart it is the holder its floor
    /// recorded, so the floor owner is accepted again only from that
    /// instance.
    holder: String,
}

/// The owner and op tables of one runner instance, for every host.
#[derive(Default)]
pub(crate) struct Tables {
    owners: HashMap<(String, String), Owned>,
    ops: HashMap<(String, OpKey), Op>,
    /// Where accepted owners are recorded before they are accepted, so a
    /// restart never accepts an owner older than one it accepted before.
    floors: Option<Arc<dyn OwnerFloors>>,
}

impl Tables {
    /// Tables whose owners start from `floors`' record.
    pub(crate) fn with_floors(floors: Arc<dyn OwnerFloors>) -> Self {
        let owners = floors
            .load()
            .into_iter()
            .map(|(host, Floor { owner, holder })| {
                (
                    (host, owner.line.clone()),
                    Owned {
                        owner,
                        adopted: false,
                        holder,
                    },
                )
            })
            .collect();
        Self {
            owners,
            ops: HashMap::new(),
            floors: Some(floors),
        }
    }

    /// Records an owner change from `instance` of `host`, which becomes the
    /// line's holder. The new owner must be the line's accepted owner or a
    /// newer one. The accepted owner again (a retry, or re-adopting an
    /// orphan) is accepted only from the line's holder: another instance
    /// takes the line over only with a newer owner, which only the holder of
    /// the line's head lease can present. A newer owner is accepted only
    /// from the accepted owner (`from`, a compare-and-swap) or onto a line
    /// with none, and is recorded first when floors are kept.
    pub(crate) fn transfer(
        &mut self,
        host: &str,
        instance: &str,
        control_id: String,
        from: Option<&LineOwner>,
        to: LineOwner,
    ) -> RunnerFrame {
        let line = (host.to_owned(), to.line.clone());
        let current = self.owners.get(&line).map(|owned| owned.owner.clone());
        if let Some(refusal) = self.transfer_refusal(host, instance, &control_id, from, &to) {
            return refusal;
        }
        if current.as_ref() != Some(&to)
            && let Some(floors) = &self.floors
            && let Err(error) = floors.record(
                host,
                &Floor {
                    owner: to.clone(),
                    holder: instance.to_owned(),
                },
            )
        {
            return RunnerFrame::TransferRefused {
                control_id,
                detail: format!("the owner could not be recorded: {error}"),
            };
        }
        self.owners.insert(
            line,
            Owned {
                owner: to,
                adopted: true,
                holder: instance.to_owned(),
            },
        );
        RunnerFrame::TransferAccepted {
            control_id,
            stopped: Vec::new(),
            unpromoted: Vec::new(),
        }
    }

    pub(crate) fn transfer_refusal(
        &self,
        host: &str,
        instance: &str,
        control_id: &str,
        from: Option<&LineOwner>,
        to: &LineOwner,
    ) -> Option<RunnerFrame> {
        let line = (host.to_owned(), to.line.clone());
        let current = self.owners.get(&line).map(|owned| owned.owner.clone());
        if let Some(from) = from
            && from.line != to.line
        {
            return Some(RunnerFrame::TransferRefused {
                control_id: control_id.to_owned(),
                detail: "a transfer names one line".into(),
            });
        }
        let held_elsewhere = self
            .owners
            .get(&line)
            .is_some_and(|owned| owned.holder != instance);
        match &current {
            Some(current) if current == to && !held_elsewhere => {}
            Some(current)
                if current == to || !to.is_newer_than(current) || from != Some(current) =>
            {
                return Some(RunnerFrame::StaleOwner {
                    request_id: None,
                    control_id: Some(control_id.to_owned()),
                    current: current.clone(),
                });
            }
            // With no accepted owner there is nothing to fence.
            Some(_) | None => {}
        }
        None
    }

    /// Marks every line `instance` of `host` holds as no longer held: its
    /// owner lease expired. Other instances' lines stay held. An orphaned
    /// line keeps its owner: it is not free to take.
    pub(crate) fn orphan(&mut self, host: &str, instance: &str) {
        for ((owner_host, _), owned) in &mut self.owners {
            if owner_host == host && owned.holder == instance {
                owned.adopted = false;
            }
        }
    }

    pub(crate) fn held_lines(&self, host: &str, instance: &str) -> Vec<LineOwner> {
        self.owners
            .iter()
            .filter(|((owned_host, _), owned)| owned_host == host && owned.holder == instance)
            .map(|(_, owned)| owned.owner.clone())
            .collect()
    }

    pub(crate) fn held_by(&self, host: &str, instance: &str, owner: &LineOwner) -> bool {
        self.owners
            .get(&(host.to_owned(), owner.line.clone()))
            .is_some_and(|owned| owned.adopted && owned.holder == instance && owned.owner == *owner)
    }

    /// The accepted owner of `host`'s `line` and whether it holds it.
    pub(crate) fn owner(&self, host: &str, line: &str) -> Option<(LineOwner, bool)> {
        self.owners
            .get(&(host.to_owned(), line.to_owned()))
            .map(|owned| (owned.owner.clone(), owned.adopted))
    }

    /// Admits one request from `instance` of `host`, at most once per
    /// effect-call key. A key already held is answered from the table
    /// whatever its owner, since the call it names may have run; the owner
    /// fence applies only to work that has not. A held key of a line is
    /// answered, and readdressed, only to the instance that last sent it or
    /// to the line's holder (a successor that transferred the line first):
    /// another process of the host would otherwise take the answer, the
    /// result and the acknowledgement from the live holder.
    pub(crate) fn admit(
        &mut self,
        host: &str,
        instance: &str,
        request: &Request,
        now: Instant,
    ) -> Admission {
        self.expire(now);
        let id = request.request_id.clone();
        let key = match op_key(request) {
            Keyed::Key(key) => Some(key),
            Keyed::Unkeyed => None,
            Keyed::Ownerless => {
                return Admission::Answer(RunnerFrame::Refused {
                    request_id: Some(id),
                    refusal: Refusal::Malformed {
                        detail: "an effect call, observation or control names its owner".into(),
                    },
                });
            }
        };
        let holder = self.holder(host, request).map(str::to_owned);
        if let Some(key) = &key
            && let Some(op) = self.ops.get_mut(&(host.to_owned(), key.clone()))
        {
            if request.owner.is_some()
                && op.instance != instance
                && holder.as_deref() != Some(instance)
            {
                return Admission::Answer(RunnerFrame::Refused {
                    request_id: Some(id),
                    refusal: Refusal::OwnerNotTransferred,
                });
            }
            if op.digest != request.digest {
                return Admission::Answer(RunnerFrame::StaleRequest { request_id: id });
            }
            return Admission::Answer(match &op.state {
                OpState::Running => {
                    op.request_id.clone_from(&id);
                    instance.clone_into(&mut op.instance);
                    RunnerFrame::InProgress { request_id: id }
                }
                OpState::Answered(answer) => {
                    op.request_id.clone_from(&id);
                    instance.clone_into(&mut op.instance);
                    with_request_id((**answer).clone(), id)
                }
                OpState::Tombstone => RunnerFrame::NotSeen { request_id: id },
                OpState::Spent => RunnerFrame::Spent { request_id: id },
            });
        }
        // A key this old may have run and been forgotten: an unseen key
        // proves nothing. The age comes from the host's wall clock, so a
        // margin covers a step back.
        if key.is_some()
            && request.replay
            && request
                .age_ms
                .is_some_and(|age| Duration::from_millis(age) + CLOCK_MARGIN >= RETENTION)
        {
            return Admission::Answer(RunnerFrame::Expired { request_id: id });
        }
        if let Some(refusal) = self.fence(host, instance, request) {
            return Admission::Answer(refusal);
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
                instance: instance.to_owned(),
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

    /// The instance holding the line `request` names, if it names one.
    fn holder(&self, host: &str, request: &Request) -> Option<&str> {
        let owner = request.owner.as_ref()?;
        self.owners
            .get(&(host.to_owned(), owner.line.clone()))
            .map(|owned| owned.holder.as_str())
    }

    /// The refusal of a request whose owner is not the line's accepted
    /// owner holding it: an older one is stale, and a newer one, or the
    /// accepted one of an orphaned line, is transferred first.
    fn fence(&self, host: &str, instance: &str, request: &Request) -> Option<RunnerFrame> {
        let owner = request.owner.as_ref()?;
        let id = request.request_id.clone();
        let keyed = matches!(
            request.class,
            RequestClass::EffectCall { .. } | RequestClass::OwnerControl { .. }
        );
        let current = self.owners.get(&(host.to_owned(), owner.line.clone()));
        // A replay of an unseen key tombstones it, so only the line's holder
        // may send one: its own key again, or its predecessor's under the
        // older owner it took the line from. Another instance's tombstone
        // would refuse the holder's own first send of that key.
        if keyed && request.replay {
            return match current {
                Some(current) if current.holder == instance => None,
                _ => Some(RunnerFrame::Refused {
                    request_id: Some(id),
                    refusal: Refusal::OwnerNotTransferred,
                }),
            };
        }
        // An effect call or control that would run is accepted only from the
        // line's holder: another instance of the host transfers first, which
        // needs a newer owner. An observation has no effect, so it is not
        // held to the holder.
        match current {
            Some(current)
                if current.owner == *owner
                    && current.adopted
                    && keyed
                    && current.holder != instance =>
            {
                Some(RunnerFrame::Refused {
                    request_id: Some(id),
                    refusal: Refusal::OwnerNotTransferred,
                })
            }
            Some(current) if current.owner == *owner && current.adopted => None,
            Some(current) if current.owner == *owner => Some(RunnerFrame::Refused {
                request_id: Some(id),
                refusal: Refusal::OwnerNotTransferred,
            }),
            Some(current) if !owner.is_newer_than(&current.owner) => {
                Some(RunnerFrame::StaleOwner {
                    request_id: Some(id),
                    control_id: None,
                    current: current.owner.clone(),
                })
            }
            _ => Some(RunnerFrame::Refused {
                request_id: Some(id),
                refusal: Refusal::OwnerNotTransferred,
            }),
        }
    }

    fn expire(&mut self, now: Instant) {
        self.ops.retain(|_, op| {
            matches!(op.state, OpState::Running) || now.duration_since(op.at) < RETENTION
        });
    }

    /// The key a running or answered request id of `instance` names, if it
    /// is keyed.
    pub(crate) fn key_for(&self, host: &str, instance: &str, request_id: &str) -> Option<OpKey> {
        self.ops
            .iter()
            .find(|((op_host, _), op)| {
                op_host == host && op.instance == instance && op.request_id == request_id
            })
            .map(|((_, key), _)| key.clone())
    }

    /// Records a keyed request's answer and returns it addressed to the
    /// request id and host instance waiting for it now.
    pub(crate) fn answer(
        &mut self,
        host: &str,
        key: &OpKey,
        answer: RunnerFrame,
        now: Instant,
    ) -> (RunnerFrame, Option<String>) {
        let Some(op) = self.ops.get_mut(&(host.to_owned(), key.clone())) else {
            return (answer, None);
        };
        let answer = with_request_id(answer, op.request_id.clone());
        op.state = OpState::Answered(Box::new(answer.clone()));
        op.at = now;
        (answer, Some(op.instance.clone()))
    }

    /// `instance` recorded `request_id`'s answer: its key is spent. Another
    /// instance's acknowledgement under the same id spends nothing.
    pub(crate) fn acknowledge(&mut self, host: &str, instance: &str, request_id: &str) {
        for ((op_host, _), op) in &mut self.ops {
            if op_host == host
                && op.instance == instance
                && op.request_id == request_id
                && matches!(op.state, OpState::Answered(_))
            {
                op.state = OpState::Spent;
            }
        }
    }

    /// Whether a key still holds an answer or tombstone the host has not
    /// acknowledged, or is running.
    pub(crate) fn holds_unacknowledged(&mut self, now: Instant) -> bool {
        self.expire(now);
        self.ops.values().any(|op| {
            matches!(
                op.state,
                OpState::Running | OpState::Answered(_) | OpState::Tombstone
            )
        })
    }
}

/// How a request is keyed.
enum Keyed {
    /// It runs at most once under this key.
    Key(OpKey),
    /// It is never recorded.
    Unkeyed,
    /// It needs an owner and names none: an effect call, a control or an
    /// observation.
    Ownerless,
}

/// The key a request runs at most once under: an effect call's or an owner
/// control's.
fn op_key(request: &Request) -> Keyed {
    match (&request.class, &request.owner) {
        (
            RequestClass::EffectCall {
                call_id,
                attempt_id,
            },
            Some(owner),
        ) => Keyed::Key(OpKey::Effect {
            line: owner.line.clone(),
            generation: owner.generation,
            call_id: call_id.clone(),
            attempt_id: attempt_id.clone(),
            verb: request.operation.verb.clone(),
        }),
        (RequestClass::OwnerControl { control_id }, Some(owner)) => Keyed::Key(OpKey::Control {
            line: owner.line.clone(),
            control_id: control_id.clone(),
        }),
        // A tree operation is the tree's, not a line's: no owner fences it.
        (
            RequestClass::TreeOperation {
                tree_id,
                operation_id,
            },
            _,
        ) => Keyed::Key(OpKey::Tree {
            tree_id: tree_id.clone(),
            operation_id: operation_id.clone(),
            verb: request.operation.verb.clone(),
        }),
        (
            RequestClass::EffectCall { .. }
            | RequestClass::OwnerControl { .. }
            | RequestClass::ProcessObservation { .. },
            None,
        ) => Keyed::Ownerless,
        _ => Keyed::Unkeyed,
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
