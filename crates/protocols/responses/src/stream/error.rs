//! Kernel-local error classification helpers for stream lifecycle failures.
//!
//! Dialects never see this module's types on the public wire; they map through
//! [`super::hooks::DialectStreamHooks`] into provider error enums.

/// Resource category strings used when the kernel enforces byte/event caps.
pub mod resource {
    /// Total accepted stream bytes for one operation.
    pub const TOTAL_STREAM_BYTES: &str = "total stream bytes";
    /// Count of decoded JSON SSE events (not framing messages).
    pub const SSE_EVENTS: &str = "SSE events";
}
