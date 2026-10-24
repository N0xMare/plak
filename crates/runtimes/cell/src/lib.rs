//! The cell protocol between Plak and an isolated interpreter worker, and the
//! parent-side client and session that speak it.
//!
//! This crate links no interpreter and no runtime authority: workers live in
//! their own crates (`plakous-js-runtime`), and the driver-facing adapter in
//! the `plakous` facade.

mod client;
mod frame;
mod process_resources;
mod protocol;
mod session;

pub use client::{
    COMMAND_QUEUE_ITEMS, CellClient, CellEvent, ChildProcessUsage, MAX_QUEUED_BYTES,
    NestedDeliveryOutcome, WORKER_HELLO_TIMEOUT, WORKER_SHUTDOWN_SETTLE_TIMEOUT,
};
pub use frame::{
    MAX_FRAME_BYTES, encode_frame, read_frame, read_frame_blocking, write_frame_blocking,
};
pub use process_resources::{
    ChildReapBounds, ChildReapEscalation, ProcessResourceUsage, children_rusage_delta,
    reap_child_pid_bounded, sample_children_rusage, sample_self_rusage,
};
pub use protocol::{
    CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellValue, DeliveryVerdict, EngineConfig,
    HostFrame, LossClass, MAX_CELL_VALUE_DEPTH, OutputItem, QUICKJS_WORKER, ResourceUsage,
    WorkerFrame, WorkerIdentity, json_depth,
};
pub use session::{
    CellCompletion, CellResponse, CellSession, CellSessionEvent, CellSessionEvents, CellStartError,
    MAX_COLLECTED_OUTPUT_BYTES, MAX_COLLECTED_OUTPUT_ITEMS, NestedResponder, NotificationResponder,
    StartedCell, WaitOutcome,
};
