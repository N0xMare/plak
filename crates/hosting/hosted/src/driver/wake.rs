//! Role-parameterized idle wakes; storage and live ownership stay with adapters.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::agent_tree::{CommandFact, ProductAgentTree};
use crate::agent_tree_protocol::AttemptId;

pub(crate) type Arrivals = BTreeSet<String>;

pub(crate) struct WakeSources {
    pub(crate) running: Vec<String>,
    pub(crate) arrivals: Arrivals,
    pub(crate) children: Vec<String>,
    /// Unread mail a live delivery still holds: it lands as an arrival (or
    /// is delivered) once that delivery returns.
    pub(crate) claimed: Vec<String>,
}

impl WakeSources {
    pub(crate) fn live_work(&self) -> bool {
        !self.running.is_empty() || !self.children.is_empty()
    }

    pub(crate) fn arrived_beyond(&self, carried: &Arrivals) -> bool {
        self.arrivals.iter().any(|id| !carried.contains(id))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum WakePolicy {
    Root,
    Child,
}

impl WakePolicy {
    pub(crate) const fn window(self) -> Duration {
        match self {
            Self::Root => Duration::from_secs(2),
            Self::Child => Duration::from_secs(1),
        }
    }
}

pub(crate) struct WakeCoalescer {
    policy: WakePolicy,
    observed: Arrivals,
    quiet_since: Option<Instant>,
    carried: Arrivals,
}

impl Default for WakeCoalescer {
    fn default() -> Self {
        Self::new(WakePolicy::Root)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum WakeDecision {
    Idle,
    Wait { until: Instant },
    Fire(Arrivals),
}

impl WakeCoalescer {
    pub(crate) fn new(policy: WakePolicy) -> Self {
        Self {
            policy,
            observed: Arrivals::new(),
            quiet_since: None,
            carried: Arrivals::new(),
        }
    }

    pub(crate) fn observe(&mut self, sources: &WakeSources, now: Instant) -> WakeDecision {
        match self.policy {
            WakePolicy::Root => {
                if self.observed != sources.arrivals {
                    self.observed.clone_from(&sources.arrivals);
                    self.quiet_since = Some(now);
                }
                if sources.arrivals.is_empty() {
                    return WakeDecision::Idle;
                }
            }
            WakePolicy::Child => {
                // A child's fixed coalesce is not restarted by later arrivals.
                // Ownership, rather than a second source predicate, fences Start.
                if self.quiet_since.is_none() {
                    if !sources.claimed.is_empty()
                        || (!sources.arrived_beyond(&self.carried) && sources.live_work())
                    {
                        return WakeDecision::Idle;
                    }
                    self.quiet_since = Some(now);
                }
            }
        }
        let Some(since) = self.quiet_since else {
            return WakeDecision::Idle;
        };
        let until = since + self.policy.window();
        if now < until {
            WakeDecision::Wait { until }
        } else {
            WakeDecision::Fire(sources.arrivals.clone())
        }
    }

    /// A bounded wake can leave mail or notices unconsumed; those arrivals
    /// alone must not cause an endless sequence of child wakes.
    pub(crate) fn carried(&mut self, fired: Arrivals) {
        self.carried = fired;
        self.park();
    }

    pub(crate) fn park(&mut self) {
        self.observed.clear();
        self.quiet_since = None;
    }

    pub(crate) fn quiet_for(&self, now: Instant) -> Duration {
        self.quiet_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }
}

pub(crate) enum WakeIdentity<'a> {
    Root {
        item: &'a str,
        revision: u64,
        triggers: &'a [String],
    },
    Child {
        agent: &'a str,
        host_generation: u64,
        attempt: &'a AttemptId,
        bound_turns: u32,
    },
}

impl WakeIdentity<'_> {
    pub(crate) const ROOT_PREFIX: &str = "plak:child-wake:";
    pub(crate) const CHILD_PREFIX: &str = "plak:park-wake:";

    pub(crate) fn command_id(&self) -> String {
        match self {
            Self::Root {
                item,
                revision,
                triggers,
            } => format!(
                "{}{item}.{revision}:{}",
                Self::ROOT_PREFIX,
                triggers
                    .iter()
                    .filter(|id| id.starts_with('p'))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("-")
            ),
            Self::Child {
                agent,
                host_generation,
                attempt,
                bound_turns,
            } => {
                format!(
                    "{}{agent}:{host_generation}:{attempt}:{bound_turns}",
                    Self::CHILD_PREFIX
                )
            }
        }
    }

    pub(crate) fn next_child(
        tree: &ProductAgentTree,
        agent: &str,
    ) -> Result<String, WakeIdentityError> {
        let attempt = tree
            .attempt(agent)
            .ok_or(WakeIdentityError::MissingAttempt)?;
        if attempt.output_json.is_some() || attempt.status.is_stopped() {
            return Err(WakeIdentityError::Stopped);
        }
        if tree.attempt_turns_spent(agent) {
            return Err(WakeIdentityError::Spent);
        }
        let mut count = attempt.usage.turns;
        loop {
            let id = WakeIdentity::Child {
                agent,
                host_generation: tree.host_generation,
                attempt: &attempt.attempt_id,
                bound_turns: count,
            }
            .command_id();
            // Consumption or withdrawal did not spend a turn, but its id
            // cannot name another command on this attachment.
            if !tree.command(&id).is_some_and(|command| {
                command.withdrawn
                    || (command.fact == Some(CommandFact::Accepted) && command.turn_id.is_none())
            }) {
                return Ok(id);
            }
            count = count.checked_add(1).ok_or(WakeIdentityError::Exhausted)?;
        }
    }
}

#[derive(Debug)]
pub(crate) enum WakeIdentityError {
    MissingAttempt,
    Stopped,
    Spent,
    Exhausted,
}

impl std::fmt::Display for WakeIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MissingAttempt => "attempt is missing",
            Self::Stopped => "attempt is already submitted or terminal",
            Self::Spent => "attempt has spent its template max_turns; no wake turn is admitted",
            Self::Exhausted => "wake command identities are exhausted",
        })
    }
}

impl std::error::Error for WakeIdentityError {}

#[cfg(test)]
mod tests;
