//! One durable agent tree per root session.
//!
//! Nodes, grants, attempts, sealed candidates, workspace rows, mail, the
//! operation ledger, the epoch and its frozen roster, the per-cell budget, the
//! ten tool executors, the process hub, and the tree's own projection.
//!
//! It runs **no** inference loop and never opens a session. What a session is,
//! how a child binds to one, what a turn costs, where the rows are persisted,
//! and what the operator sees are all the host's, reached through the ports
//! here and nowhere else: [`tree::TreeStore`] for durability,
//! [`tree::TopologyObserver`] for telemetry, [`hub::LiveChildHandle`] for a
//! live child turn, and `tools`' binder, deliverer, and canceller for a spawn's
//! session lifecycle. The tree's own subprocess needs — minting a copy,
//! sealing a candidate, landing an adopt — go to `plakous-workspace`.
//!
//! # The model, as docs/spec/03 states it
//!
//! A **tree** is one root session's whole delegation. It has one root node,
//! one durable id, and a `revision` that advances on every mutation and is the
//! store's compare-and-swap token.
//!
//! An **epoch** is the tree's frozen bind authority: the root bind, the five
//! `[orchestration]` bounds, the working set of template cards, the product's
//! account roster, and a digest over all of it. It is frozen at genesis, so
//! editing `config.toml` or a template file mid-tree changes nothing until a
//! successor epoch installs — which only a quiescent tree admits.
//!
//! A **node** is one agent: its brief, the template card it was spawned from,
//! the bind that card resolved to, and the checkout it works in. A node's
//! authority is its **grant**, compiled from the card and its depth; that is
//! what decides which of the ten tools it is even offered.
//!
//! An **attempt** is one run of one node, and it — not the node — carries
//! status, usage, the submitted result, and the turn token a `submit_result`
//! must match. A `followup` opens a fresh attempt on the same node, so a
//! node's history is the attempts naming it.
//!
//! A **workspace** is a checkout: the launch checkout itself, or a copy minted
//! for one node. `read_only` shares the caller's; `execute` and `mutate` get
//! their own. A copy is minted from a named base and can be discarded again.
//!
//! A **candidate** is a sealed snapshot of what one attempt's copy changed,
//! pinned outside the worktree so discarding the copy cannot orphan it. A copy
//! that never diverged from its base seals nothing. Adopting a candidate lands
//! it onto the caller's target; an unadopted one pins its workspace against
//! discard, so work is never silently thrown away.
//!
//! Two rules run through all of it. **The tree never runs a subprocess:** a
//! mutation under the tree mutex queues what it owes the VCS as [`tree::Effects`]
//! or a [`tree::MintTicket`], the caller runs it with the lock released, and
//! re-locks once to record the answer. And **every idempotent call is
//! ledgered:** a repeated `operation_id` replays the recorded response instead
//! of acting twice, so a retried spawn cannot mint a second child.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::map_unwrap_or,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::cast_possible_truncation,
    clippy::format_push_string,
    clippy::assigning_clones,
    clippy::manual_let_else,
    clippy::unused_self,
    reason = "the set this crate actually trips"
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "unit tests"
    )
)]

pub mod call_facts;
pub mod cell_budget;
pub mod epoch;
pub mod hub;
pub mod tools;
pub mod tree;

/// The wire vocabulary the tree speaks: request shapes, node and attempt
/// statuses, tiers, workspace modes, the caps, and every topology error code.
///
/// Re-exported rather than re-declared so an external `plak agent` client can
/// depend on `plakous-host-protocol` alone and still name everything a tool
/// call carries.
pub mod protocol {
    pub use plakous_host_protocol::topology::*;
}
