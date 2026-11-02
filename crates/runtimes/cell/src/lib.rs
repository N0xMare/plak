//! The cell protocol between Plak and an isolated interpreter worker, and the
//! parent-side client and session that speak it.
//!
//! This crate links no interpreter and no runtime authority: workers live in
//! their own crates (`plakous-js-runtime`, `plakous-py-worker`), and the driver-facing adapter in
//! the `plakous` facade.

mod client;
mod frame;
mod process_resources;
mod protocol;
mod session;
mod startup;
mod stored;
#[cfg(feature = "test-hooks")]
mod test_hooks;
#[cfg(not(feature = "test-hooks"))]
mod test_hooks {
    pub(crate) fn cell_hello_millis() -> Option<String> {
        None
    }
}

pub use stored::{StoredValueDepthError, validate_stored_values};

pub use startup::CellStartupError;

pub use client::{
    COMMAND_QUEUE_ITEMS, CellClient, CellEvent, ChildProcessUsage, MAX_QUEUED_BYTES,
    NestedDeliveryOutcome, WORKER_HELLO_TIMEOUT, WORKER_SHUTDOWN_SETTLE_TIMEOUT, WorkerStop,
};
pub use frame::{
    MAX_FRAME_BYTES, encode_frame, read_frame, read_frame_blocking, write_frame_blocking,
};
pub use process_resources::{
    ProcessResourceUsage, children_rusage_delta, sample_children_rusage, sample_self_rusage,
};
pub use protocol::{
    BindingInput, CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellStopReason, CellValue,
    DeliveryVerdict, EngineConfig, HostFrame, LossClass, MAX_CELL_VALUE_DEPTH, MONTY_WORKER,
    MontyBinding, MontyBootstrap, OutputItem, QUICKJS_WORKER, ResourceUsage, WorkerFrame,
    WorkerIdentity, json_depth,
};
pub use session::{
    CellCompletion, CellResponse, CellSession, CellSessionEvent, CellSessionEvents, CellStartError,
    MAX_COLLECTED_OUTPUT_BYTES, MAX_COLLECTED_OUTPUT_ITEMS, NestedResponder, NotificationResponder,
    ObservationMode, StartedCell, WaitOutcome,
};
