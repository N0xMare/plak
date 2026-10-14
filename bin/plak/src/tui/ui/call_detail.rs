//! On-demand call detail for open transcript items (spec 02 §5.4).
//!
//! Opening an item asks the host once for the call's canonical input and
//! output; the retained preview renders until the answer lands, and stays the
//! only source when the host has nothing better. Canonical answers are cached
//! per (session, agent, call id), so reopening an item or rebuilding the
//! transcript of the same session does not ask again. A `preview` or
//! unavailable answer is only the row's answer while it stays open: closing
//! the item drops it, and reopening asks again.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_host_protocol::views::{CallDetail, CallDetailOutcome, CallDetailSource};

use super::{App, DisplayMode, ScrollBlock, UserAction};
use crate::tui::presentation::ActivityKind;

/// Canonical answers kept; the oldest is dropped first.
const CACHED_DETAILS: usize = 64;

/// An unanswered request is asked again after this long while its item is
/// still open: the observation channel may drop an answer under load.
const REASK_AFTER: Duration = Duration::from_secs(5);

/// A `preview` answer for a call this surface saw end within this long may
/// only mean the host had not committed its canonical history yet, so the
/// call is asked once more.
const COMMIT_RACE: Duration = Duration::from_secs(30);

/// Calls remembered as recently ended; the oldest is dropped first.
const RECENT_ENDS: usize = 32;

/// The host's refusal when its read queue is full (spec 02 §4.3): no answer
/// about the call, so the request stays outstanding and is asked again.
const BUSY_REASON: &str = "busy";

/// One call's address: attached session, child agent (`None` for the
/// attached session itself) and call id.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct DetailKey {
    session: String,
    agent: Option<String>,
    call_id: String,
}

#[derive(Debug, Default)]
pub(super) struct CallDetails {
    cache: HashMap<DetailKey, Arc<CallDetailOutcome>>,
    order: VecDeque<DetailKey>,
    in_flight: HashMap<DetailKey, Ask>,
    /// Calls seen ending, with when, and whether a `preview` answer for
    /// them was already asked again.
    recent_ends: VecDeque<RecentEnd>,
}

/// Where one open item's request stands.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// To be sent on the next loop turn.
    Queued,
    /// Sent and unanswered; asked again after [`REASK_AFTER`].
    Sent(Instant),
    /// Answered without the canonical text it may still get (`busy`, or a
    /// `preview` that may be the commit race): nothing is outstanding, so
    /// reopening the item asks at once; else asked after [`REASK_AFTER`].
    Retry(Instant),
}

#[derive(Debug)]
struct RecentEnd {
    key: DetailKey,
    at: Instant,
    reasked: bool,
}

impl CallDetails {
    fn remember(&mut self, key: DetailKey, outcome: Arc<CallDetailOutcome>) {
        if self.cache.insert(key.clone(), outcome).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CACHED_DETAILS {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
    }

    /// Note that a call of the root or of an agent on screen just ended.
    pub(super) fn note_ended(&mut self, key: DetailKey) {
        let now = Instant::now();
        self.recent_ends
            .retain(|end| end.key != key && now.duration_since(end.at) < COMMIT_RACE);
        self.recent_ends.push_back(RecentEnd {
            key,
            at: now,
            reasked: false,
        });
        while self.recent_ends.len() > RECENT_ENDS {
            self.recent_ends.pop_front();
        }
    }

    /// Whether a `preview` answer for `key` may be the commit race, and
    /// this is the one time it is asked again.
    fn reask_preview(&mut self, key: &DetailKey) -> bool {
        let now = Instant::now();
        self.recent_ends
            .iter_mut()
            .find(|end| end.key == *key && !end.reasked && now.duration_since(end.at) < COMMIT_RACE)
            .is_some_and(|end| {
                end.reasked = true;
                true
            })
    }

    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        self.in_flight.len()
    }

    /// Pretend every sent or retried request went out `by` earlier.
    #[cfg(test)]
    pub(super) fn age_asks(&mut self, by: Duration) {
        for ask in self.in_flight.values_mut() {
            if let Ask::Sent(at) | Ask::Retry(at) = ask {
                *at = at.checked_sub(by).unwrap_or(*at);
            }
        }
    }
}

/// Call ids of the expanded items in one pane's blocks.
fn expanded_call_ids(blocks: &[ScrollBlock]) -> std::collections::HashSet<&str> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ScrollBlock::Tool {
                call_id: Some(id),
                mode: DisplayMode::Expanded,
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn is_canonical(outcome: &CallDetailOutcome) -> bool {
    matches!(
        outcome,
        CallDetailOutcome::Detail {
            source: CallDetailSource::Canonical,
            ..
        }
    )
}

/// What a closed item keeps of its answer: only a canonical one, which no
/// later read can improve on.
pub(super) fn close_item_view(view: &mut crate::tui::blocks::ItemView) {
    if view
        .fetched
        .as_deref()
        .is_some_and(|outcome| !is_canonical(outcome))
    {
        view.fetched = None;
    }
}

impl App {
    /// The address of one of the attached session's own calls; `None`
    /// before a session is attached, when there is nothing to ask.
    pub(super) fn detail_key(&self, call_id: &str) -> Option<DetailKey> {
        Some(DetailKey {
            session: self.session_id.clone()?,
            agent: self.viewed_agent_id().map(str::to_owned),
            call_id: call_id.to_owned(),
        })
    }

    /// Fill an opened item's detail from the cache, or queue one request.
    /// A call still running is not asked for: its canonical history is not
    /// committed yet, and the preview is the honest source until it ends.
    pub(super) fn request_item_detail(&mut self, idx: usize) {
        let Some(ScrollBlock::Tool {
            call_id: Some(call_id),
            kind,
            state,
            mode,
            view,
            ..
        }) = self.pane.blocks.get(idx)
        else {
            return;
        };
        if *kind == ActivityKind::Compact
            || *mode != DisplayMode::Expanded
            || view.fetched.is_some()
            || super::tool_is_running(state)
        {
            return;
        }
        let Some(key) = self.detail_key(call_id) else {
            return;
        };
        if let Some(outcome) = self.call_details.cache.get(&key).cloned() {
            if let Some(ScrollBlock::Tool { view, .. }) = self.pane.blocks.get_mut(idx) {
                view.fetched = Some(outcome);
            }
            self.mark_block_changed(idx);
            return;
        }
        let ask = self
            .call_details
            .in_flight
            .entry(key)
            .or_insert(Ask::Queued);
        if matches!(ask, Ask::Retry(_)) {
            *ask = Ask::Queued;
        }
    }

    /// Send queued requests, and ask again for an answer that never came
    /// while its item is still open.
    pub(crate) fn maybe_request_call_details(
        &mut self,
        actions: Option<&crate::tui::UserActionSender>,
    ) {
        let Some(tx) = actions else {
            return;
        };
        // Called every loop turn: nothing to scan unless a request is open.
        if self.call_details.in_flight.is_empty() {
            return;
        }
        let now = Instant::now();
        let (root, agent) = super::pane::split_panes(&self.pane, self.agent_view.as_ref());
        let root_open = expanded_call_ids(&root.blocks);
        let agent_open = agent.map(|(agent, pane)| (agent, expanded_call_ids(&pane.blocks)));
        self.call_details.in_flight.retain(|key, _| {
            let open = match (key.agent.as_deref(), &agent_open) {
                (None, _) => &root_open,
                (Some(agent), Some((viewed, open))) if agent == *viewed => open,
                (Some(_), _) => return false,
            };
            Some(&key.session) == self.session_id.as_ref() && open.contains(key.call_id.as_str())
        });
        for (key, ask) in &mut self.call_details.in_flight {
            if let Ask::Sent(at) | Ask::Retry(at) = *ask
                && now.duration_since(at) < REASK_AFTER
            {
                continue;
            }
            let _ = tx.send(UserAction::RequestCallDetail {
                call_id: key.call_id.clone(),
                agent_id: key.agent.clone(),
                session_id: key.session.clone(),
            });
            *ask = Ask::Sent(now);
        }
    }

    /// One host answer: every row of that call in the attached session takes
    /// it, and a canonical answer is cached. An answer for another session
    /// (one attached before a resume or clear) is dropped.
    pub(super) fn apply_call_detail(&mut self, detail: CallDetail) {
        if self.session_id.as_deref() != Some(detail.session_id.as_str()) {
            return;
        }
        let key = DetailKey {
            session: detail.session_id,
            agent: detail.agent_id,
            call_id: detail.call_id,
        };
        if !self.call_details.in_flight.contains_key(&key) {
            return;
        }
        let outcome = Arc::new(detail.outcome);
        match &*outcome {
            CallDetailOutcome::Unavailable { reason } if reason == BUSY_REASON => {
                self.call_details
                    .in_flight
                    .insert(key, Ask::Retry(Instant::now()));
                return;
            }
            CallDetailOutcome::Detail {
                source: CallDetailSource::Canonical,
                ..
            } => {
                self.call_details.in_flight.remove(&key);
                self.call_details
                    .remember(key.clone(), Arc::clone(&outcome));
            }
            CallDetailOutcome::Detail {
                source: CallDetailSource::Preview,
                ..
            } if self.call_details.reask_preview(&key) => {
                // Shown now, asked again after REASK_AFTER.
                self.call_details
                    .in_flight
                    .insert(key.clone(), Ask::Retry(Instant::now()));
            }
            _ => {
                self.call_details.in_flight.remove(&key);
            }
        }
        // A root answer runs with the root pane on screen; an agent's lands
        // only in that agent's open view.
        if key.agent.is_some() && key.agent.as_deref() != self.viewed_agent_id() {
            return;
        }
        for idx in 0..self.pane.blocks.len() {
            if let ScrollBlock::Tool {
                call_id: Some(id),
                view,
                ..
            } = &mut self.pane.blocks[idx]
                && *id == key.call_id
            {
                view.fetched = Some(Arc::clone(&outcome));
                self.mark_block_changed(idx);
            }
        }
    }
}
