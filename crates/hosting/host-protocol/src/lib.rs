//! The host protocol between a hosted Plakous session and whoever drives it.
//!
//!
//! Three faces of one vocabulary:
//!
//! * [`SessionIntent`] in and [`SessionObservation`] out, with the immutable
//!   [`views`] an observation carries and the [`selection`] enums both share;
//! * the two bounded in-process mailboxes in [`channel`], whose coalescing and
//!   eviction semantics every driver depends on, and the [`limits`] they and
//!   the host agree on;
//! * the `plak agent` JSONL wire: the v1 [`envelope`], the [`topology`]
//!   request/receipt shapes, and the [`jsonl`] projection of every
//!   observation, an exhaustive match so a new variant is a wire decision.
//!
//! This crate holds every type that crosses those seams and nothing that
//! decides what to do about one: dispatch, the tree runtime, the session
//! store, and the presentation projection live above it. An external
//! `plak agent` client depends on this crate, `serde` and `serde_json` —
//! never on the host. A driver with product traffic of its own (settings, an
//! overlay) threads it through the [`ProtocolExtension`] slot; the protocol
//! carries it and never reads it.
//!
//! # Framing
//!
//! One JSON object per line in both directions. Every line carries `v`, and
//! every line a client sends MUST carry `v` equal to [`envelope::SCHEMA_V`],
//! which is `1`. Every outbound object carries `v` and `kind`, and carries
//! `id` when the line answers a command; [`envelope::encode_object`] is the
//! only writer. Key order within the object is not part of the protocol: a
//! client reads fields by name. The presentation major is independent of the
//! product `SQLite` schema and of Plakous.
//!
//! # Versioning
//!
//! **v1 is frozen and growth is additive only.** Unknown additive fields on a
//! known kind are ignored, and an unknown inbound `kind` is a typed diagnostic
//! that leaves the connection up. A new field or a new kind may therefore be
//! added inside v1: a client that ignores what it does not know is unaffected.
//! A removed kind is answered like any unknown kind. Removing or renaming a
//! field, or changing the type or meaning of a field, is a new major. A missing `v` and a wrong `v` are both
//! refused with a named message.
//!
//! # Inbound kinds (client to host)
//!
//! Each carries `v`, `kind`, `id` plus the fields named here.
//!
//! | `kind` | Fields |
//! | --- | --- |
//! | `prompt` | `text`, optional `steer` (only literal `true` counts) |
//! | `interrupt`, `compact`, `clear`, `quit` | — |
//! | `fork` | optional `account` |
//! | `tree`, `resume_tree`, `stop_all` | — |
//! | `interrupt_node`, `close_node` | `agent_id` |
//! | `invoke_tool` | `name` (a [`topology::TOPOLOGY_PRESENTATION_ORDER`] entry), `arguments` (an object), optional `agent_id` |
//!
//! A `prompt` `text` is bounded by [`envelope::MAX_PROMPT_BYTES`].
//!
//! # Outbound kinds (host to client)
//!
//! `hello`, `ready`, `bye`, `error`, `status`, `busy`, `assistant`,
//! `assistant_done`, `tool`, `queue`, `turn`, `tree`, `reasoning`,
//! `approval`, `approval_resolved`, and `steer_ack`. The handshake and refusal
//! lines are built here — [`envelope::hello_line`], [`envelope::ready_line`],
//! [`envelope::bye_line`], [`envelope::diagnostic_line`] — and the rest are
//! projected by the host from its own observations. The subset in
//! [`envelope::REQUIRED_STDOUT_KINDS`] must never be dropped by a sink.
//!
//! # Correlation
//!
//! A command's `id` is echoed on the events it caused; [`envelope::JsonlCorrelation`]
//! is the rule. Turn-scoped events carry the in-flight prompt's id, else the
//! current command's. Command answers carry only the command id. A tree
//! snapshot carries an `id` only when it was asked for.
//!
//! # Topology
//!
//! [`topology`] holds the `invoke_tool` name set, the request shape of every
//! topology tool, the adopt receipt, and the tree, node, attempt, and
//! workspace row vocabularies as they are serialized into the `tree` snapshot
//! and tool receipts. [`topology::TopologyErrorCode`] is the closed refusal
//! vocabulary; its wire spelling is `snake_case`.

pub mod channel;
pub mod envelope;
pub mod intent;
pub mod jsonl;
pub mod limits;
pub mod notice_remedy;
pub mod observation;
pub mod selection;
pub mod status;
pub mod topology;
pub mod views;

pub use channel::{
    IntentAcknowledgement, IntentDelivery, IntentReceiver, IntentSendError, IntentSender,
    ObservationReceiver, ObservationSender, ObservationSink, intent_channel, observation_channel,
};
pub use intent::{
    ApprovalChoice, Core, Draft, DraftImage, Never, ObservationExtension, ProtocolExtension,
    SessionIntent,
};
pub use observation::{
    ContentObservation, ContextCapacityRecovery, ModelRetryNotice, PendingContinuation,
    QueueDrainState, QueueItem, SessionObservation, TurnLifecycle, TurnPhase,
};
pub use selection::{CompositionSelection, ProtocolSelection};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod wire_identity_goldens;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod round_trip_tests;

/// Host facts that determine a node's prompt completion channel, not its tool grants.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromptHostFacts {
    /// True when this node's host starts a new turn on the node's tracked work:
    /// an attached root with continuation enabled, and every child of the
    /// product's agent tree. False for JSONL roots and embedders without a
    /// wake loop.
    pub host_wakes: bool,
}
