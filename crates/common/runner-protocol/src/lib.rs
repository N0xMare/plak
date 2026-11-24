//! Native runner protocol (spec 01 §8.4). Pairing matches the exact
//! [`VersionTriple`]; home-local binds match the protocol version regardless
//! of revision or flavor. Older protocols get only minimal read-only `Lines`
//! inspection, not pairing. Job retention uses a separate versioned endpoint
//! ([`retention`]).
//!
//! A connection carries length-prefixed JSON frames ([`frame`]). The runner
//! speaks first with its [`Hello`]; the host answers with its [`Bind`], and
//! only then sends requests. Every request names its [`RequestClass`]: an
//! effect verb in a model call is keyed by its line, call and attempt and
//! runs at most once per key; reads retain only the owner fence. An owner change is
//! announced with [`TransferOwner`] before the new owner's first request.
//!
//! Operation payloads and backend errors are JSON values whose schema the
//! execution environment owns: this crate frames, keys and orders them, and
//! does not interpret them. Streams ([`StreamFrame`]) are framed here so that
//! long-lived processes and their output fit the same connection.

pub mod frame;
mod message;
pub mod operator;
mod owner;
pub mod retention;

pub use message::{
    Bind, Hello, HostFrame, LineStatus, OWNER_FLOOR_QUERY_LIMIT, Operation, OwnerFloor, Refusal,
    Request, RequestClass, RunnerFrame, StoppedForeground, StreamEvent, StreamFrame, TransferOwner,
    UnpromotedProcess, WireError, capabilities, verbs,
};
pub use owner::LineOwner;

/// The exact protocol version for native binds (spec 01 §8.4).
pub const PROTOCOL_VERSION: u32 = 9;

/// Stable stopped-foreground receipt identity, independent of the connection
/// and owner generation. Another call reusing the PID cannot match it.
#[must_use]
pub fn foreground_report_id(line: &str, report: &StoppedForeground) -> String {
    format!(
        "foreground-report:{}",
        serde_json::json!([line, report.call_key, report.pid])
    )
}

/// The exact identity two ends compare before any request: the protocol
/// version, the source revision and the build flavor. A binary id is
/// evidence only (spec 01 §8.4).
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct VersionTriple {
    /// [`PROTOCOL_VERSION`] of the speaker.
    pub protocol: u32,
    /// The source revision the speaker was built from.
    pub source_revision: String,
    /// The build flavor, such as `release` or `debug`.
    pub flavor: String,
}

impl VersionTriple {
    /// This crate's protocol version with the given build facts.
    #[must_use]
    pub fn current(source_revision: impl Into<String>, flavor: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            source_revision: source_revision.into(),
            flavor: flavor.into(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod pinned_tests;
