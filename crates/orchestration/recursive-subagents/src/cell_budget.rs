//! Host-side per-cell agent budget for Code Mode cells.
//!
//! Code Mode cells — `native-ptc` (the Codex `exec` wrapper), `js-ptc` (the
//! rquickjs sidecar) and `py-ptc` (Monty) alike — are the pipeline layer. A
//! cell can loop, so an unbounded cell can spawn an unbounded number of
//! children. The bound is enforced **here**, host-side, on the topology tool
//! dispatch entry, and never in Plakous L0: `docs/architecture-canon.md:165`
//! keeps multi-agent topology outside L0, and a
//! counter that recognises `spawn_agent` by name is exactly that. In
//! particular this module is *not* a second home for
//! `reserve_local_composition_nested_slot`'s `sequence > 100` literal
//! (`crates/plakous/agent/src/runtime/body.rs:27437`), which stays L0's.
//!
//! **Keying.** The budget is keyed on the invocation's
//! `ToolCallerLineage::LocalProgrammatic` identity
//! (`crates/plakous/model/src/content.rs:570`), which is what identifies "this
//! cell". It is deliberately **not** keyed on the `operation_id` the topology
//! tool receives (`agent_tree_tools.rs`, `spawn_with`): that is the derived
//! *child* call id (`local_composition_child_call_id`, `body.rs:27449`), which
//! is a fresh digest per nested request and would give every call its own
//! budget.
//!
//! **Why in memory.** A cell cannot outlive the process. One cell is one
//! nested evaluation inside a single in-process tool call driven by an
//! interpreter this host owns; if the host dies the evaluation dies with it,
//! and its `evaluation_operation_id` / `interpreter_generation` pair never
//! emits another nested request. L0's `DurableOperationKind::CompositionChild`
//! record is a replay aid and is not replay-complete,
//! so there is nothing for a persisted
//! counter to be consistent with — persisting it would only create a second,
//! unauthoritative record of an in-process fact.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

use plakous::model::ToolCallerLineage;
use serde_json::{Map, Value};

use crate::epoch::OrchestrationSnapshot;

/// Default `[orchestration] max_agents_per_cell`.
pub const DEFAULT_MAX_AGENTS_PER_CELL: u16 = 8;
/// Appendix ceiling. Config may only lower the bound, never raise it.
pub const CEILING_MAX_AGENTS_PER_CELL: u16 = DEFAULT_MAX_AGENTS_PER_CELL;

/// Cells retained per tree before the oldest is evicted. A cell that has
/// already been evicted has long since stopped emitting nested requests; the
/// bound only keeps a long-lived tree from accumulating dead cell keys.
const MAX_TRACKED_CELLS_PER_TREE: usize = 512;

/// Clamps a configured bound into `1..=CEILING_MAX_AGENTS_PER_CELL`.
///
/// Lower-only: a value above the ceiling is clamped down to it, exactly like
/// the `[orchestration]` bounds at snapshot finalize (`orchestration.rs`).
#[must_use]
pub const fn clamp_max_agents_per_cell(value: u16) -> u16 {
    if value == 0 {
        1
    } else if value > CEILING_MAX_AGENTS_PER_CELL {
        CEILING_MAX_AGENTS_PER_CELL
    } else {
        value
    }
}

/// The identity of one Code Mode cell, as L0 stamps it on every nested call,
/// qualified by the agent whose turn runs the cell.
///
/// `request_sequence`, `child_call_id` and `depth` are excluded on purpose:
/// they vary per nested request within the same cell. `caller_id` is included
/// because evaluation ids are per session and the production `cell_id` is a
/// constant: a root cell and a child cell at the same operation id would
/// otherwise share one counter and over-refuse each other.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CellIdentity {
    caller_id: String,
    evaluation_operation_id: u64,
    interpreter_generation: u64,
    cell_id: String,
}

impl CellIdentity {
    /// Returns the cell identity when the caller is a Code Mode cell.
    ///
    /// `Direct` (a model tool call) and `ProviderProgrammatic` (a hosted
    /// program) are not cells and are never budgeted.
    #[must_use]
    pub fn from_lineage(lineage: &ToolCallerLineage, caller_id: &str) -> Option<Self> {
        match lineage {
            ToolCallerLineage::Direct | ToolCallerLineage::ProviderProgrammatic { .. } => None,
            ToolCallerLineage::LocalProgrammatic {
                evaluation_operation_id,
                interpreter_generation,
                cell_id,
                ..
            } => Some(Self {
                caller_id: caller_id.to_owned(),
                evaluation_operation_id: *evaluation_operation_id,
                interpreter_generation: *interpreter_generation,
                cell_id: cell_id.clone(),
            }),
        }
    }

    /// Model-visible cell identity: stable, and unique across evaluations.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.caller_id, self.evaluation_operation_id, self.interpreter_generation, self.cell_id
        )
    }
}

/// A refused topology admission, rendered to the cell as a normal tool failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellBudgetExhausted {
    /// The cell whose budget ran out, in the model-visible `label()` form.
    pub cell_id: String,
    /// The epoch's per-cell ceiling, as frozen at install.
    pub budget: u16,
    /// Agents this cell has already admitted; never above `budget`.
    pub used: u16,
}

impl CellBudgetExhausted {
    /// The `cell_budget_exhausted` failure body.
    ///
    /// Built field by field rather than with `json!`: this module ships in the
    /// CLI binary, and `tools/checks/check_workspace.py` only approves that macro
    /// for the files that already carry it.
    #[must_use]
    pub fn to_failure_json(&self) -> Value {
        let message = format!(
            "cell {} has used its per-cell agent budget of {} \
             (this cell may still wait on, message, or close the children it already has)",
            self.cell_id, self.budget
        );
        let mut body = Map::new();
        body.insert("code".to_owned(), Value::from("cell_budget_exhausted"));
        body.insert("message".to_owned(), Value::from(message));
        body.insert("cell_id".to_owned(), Value::from(self.cell_id.clone()));
        body.insert("budget".to_owned(), Value::from(self.budget));
        body.insert("used".to_owned(), Value::from(self.used));
        Value::Object(body)
    }
}

/// Per-cell agent admissions for one tree.
#[derive(Debug)]
pub struct CellAgentBudget {
    budget: u16,
    used: Mutex<CellCounters>,
}

#[derive(Debug, Default)]
struct CellCounters {
    counts: HashMap<CellIdentity, u16>,
    order: VecDeque<CellIdentity>,
}

impl CellAgentBudget {
    /// Creates a budget from an already-clamped bound.
    #[must_use]
    pub fn new(budget: u16) -> Self {
        Self {
            budget: clamp_max_agents_per_cell(budget),
            used: Mutex::new(CellCounters::default()),
        }
    }

    /// The budget one epoch snapshot prescribes. The bound is
    /// frozen on the snapshot with the four tree bounds, so a config change
    /// mid-tree does not move a running tree's ceiling; the tree builds one
    /// of these at every `install_snapshot` and hands it to every topology
    /// tool built on it.
    #[must_use]
    pub fn for_snapshot(snapshot: &OrchestrationSnapshot) -> Self {
        Self::new(snapshot.max_agents_per_cell)
    }

    /// The effective bound.
    #[must_use]
    pub const fn budget(&self) -> u16 {
        self.budget
    }

    /// Consumes one agent admission for `cell`.
    ///
    /// Returns the new used count on success. Counted at admission, before
    /// dispatch: a cell that asks for a ninth child is refused whether or not
    /// the eighth spawn went on to succeed downstream, because the bound exists
    /// to stop the loop, not to audit outcomes.
    ///
    /// # Errors
    ///
    /// Returns [`CellBudgetExhausted`] when the cell is already at its bound.
    pub fn admit_agent(&self, cell: &CellIdentity) -> Result<u16, CellBudgetExhausted> {
        let Ok(mut counters) = self.used.lock() else {
            // A poisoned counter must not silently unbound the cell.
            return Err(CellBudgetExhausted {
                cell_id: cell.label(),
                budget: self.budget,
                used: self.budget,
            });
        };
        let used = counters.counts.get(cell).copied().unwrap_or(0);
        if used >= self.budget {
            return Err(CellBudgetExhausted {
                cell_id: cell.label(),
                budget: self.budget,
                used,
            });
        }
        let next = used.saturating_add(1);
        if counters.counts.insert(cell.clone(), next).is_none() {
            counters.order.push_back(cell.clone());
            while counters.order.len() > MAX_TRACKED_CELLS_PER_TREE {
                if let Some(evicted) = counters.order.pop_front() {
                    counters.counts.remove(&evicted);
                }
            }
        }
        Ok(next)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
