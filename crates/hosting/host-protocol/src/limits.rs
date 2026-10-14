//! Bounds every driver and host of the protocol agree on.

use std::time::Duration;

/// How long a host waits for a cancelled turn to settle before it reports the
/// cancel as stuck and moves on. Shared by the session host and
/// the topology runtime so both stop the same way.
pub const HOST_CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long releasing a quiescent session may take once its turn settled: the
/// runtime's shutdown joins any store call already in flight (such as idle
/// history retention after the last turn) and then commits its shutdown
/// receipt, each bounded by the agent's `DEFAULT_STORE_OPERATION_TIMEOUT`
/// (30 s). This crate stays free of the runtime, so `plakous-hosted` asserts at
/// compile time that this value covers two such deadlines.
pub const HOST_SESSION_RELEASE_TIMEOUT: Duration = Duration::from_secs(60);

/// Ordinary-lane capacity of the intent mailbox.
pub const INTENT_QUEUE_CAPACITY: usize = 64;

/// Capacity of the observation mailbox.
pub const OBSERVATION_QUEUE_CAPACITY: usize = 512;

/// Largest assistant delta the observation mailbox coalesces into one item.
pub const COALESCED_DELTA_BYTES: usize = 256 * 1024;
