//! The session ledger of each child node: the usage of the turns this
//! process projected for the node's session, as the root's own ledger
//! counts its turns since the session opened here.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use plakous::agent::TurnUsage;
use plakous::model::SessionId;
use plakous_host_protocol::views::presentation::SessionLedgerView;

use crate::agent_tree::{NodeDetach, ProjectedChildTurnUsage, TopologyObserver};

#[derive(Clone, Copy, Default)]
struct Totals {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    model_calls: u64,
    reporting_model_calls: u64,
}

/// Every child session's ledger in this process, keyed by its runtime
/// session.
static LEDGERS: LazyLock<Mutex<HashMap<SessionId, Totals>>> = LazyLock::new(Mutex::default);

fn record(session: SessionId, usage: TurnUsage) {
    let mut ledgers = LEDGERS.lock().unwrap_or_else(PoisonError::into_inner);
    let totals = ledgers.entry(session).or_default();
    let model = usage.model();
    totals.input_tokens = totals.input_tokens.saturating_add(model.input_tokens());
    totals.cached_input_tokens = totals
        .cached_input_tokens
        .saturating_add(model.cached_input_tokens());
    totals.output_tokens = totals.output_tokens.saturating_add(model.output_tokens());
    totals.reasoning_tokens = totals
        .reasoning_tokens
        .saturating_add(model.reasoning_tokens());
    totals.model_calls = totals.model_calls.saturating_add(usage.model_operations());
    totals.reporting_model_calls = totals
        .reporting_model_calls
        .saturating_add(usage.model_operations_reporting_usage());
}

/// The ledger of the child session `session`; an empty ledger when this
/// process projected none of its turns. A child's turn time is not the
/// host's to measure, so it has none.
pub(crate) fn ledger(session: SessionId) -> SessionLedgerView {
    let totals = LEDGERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&session)
        .copied()
        .unwrap_or_default();
    crate::subscription_usage::project_session_ledger(
        totals.input_tokens,
        totals.cached_input_tokens,
        totals.output_tokens,
        totals.reasoning_tokens,
        totals.model_calls,
        totals.reporting_model_calls,
        None,
    )
}

/// The tree's observer: it keeps each child's ledger and passes every
/// report on to the product's observer, when there is one.
pub(crate) struct LedgerObserver(pub(crate) Option<Arc<dyn TopologyObserver>>);

impl TopologyObserver for LedgerObserver {
    fn turn_usage(&self, terminal: ProjectedChildTurnUsage) {
        record(terminal.session_id, terminal.usage);
        if let Some(inner) = &self.0 {
            inner.turn_usage(terminal);
        }
    }

    fn node_spawned(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        template: &str,
    ) {
        if let Some(inner) = &self.0 {
            inner.node_spawned(tree_id, node_id, parent_node_id, template);
        }
    }

    fn node_detached(
        &self,
        tree_id: &str,
        node_id: &str,
        parent_node_id: Option<&str>,
        status: NodeDetach,
    ) {
        if let Some(inner) = &self.0 {
            inner.node_detached(tree_id, node_id, parent_node_id, status);
        }
    }

    fn epoch_advanced(&self, tree_id: &str) {
        if let Some(inner) = &self.0 {
            inner.epoch_advanced(tree_id);
        }
    }
}
