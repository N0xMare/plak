//! Runner protocol, version 2: what a Plakous host and a runner say to each
//! other over one byte stream (spec 01 §8.4).
//!
//! A connection carries length-prefixed JSON frames ([`frame`]). The runner
//! speaks first with its [`Hello`]; the host answers with its [`Bind`], and
//! only then sends requests. Every request names its [`RequestClass`]: an
//! effect call is keyed by its line, call and attempt and runs at most once
//! per key, under the line's current [`LineOwner`]; an owner change is
//! announced with [`TransferOwner`] before the new owner's first request.
//!
//! Operation payloads and backend errors are JSON values whose schema the
//! execution environment owns: this crate frames, keys and orders them, and
//! does not interpret them. Streams ([`StreamFrame`]) are framed here so that
//! long-lived processes and their output fit the same connection.

pub mod frame;
mod message;
mod owner;

pub use message::{
    Bind, Hello, HostFrame, LineStatus, Operation, Refusal, Request, RequestClass, RunnerFrame,
    StoppedForeground, StreamEvent, StreamFrame, TransferOwner, WireError, capabilities, verbs,
};
pub use owner::LineOwner;

/// The protocol version this crate speaks. Both ends must match it exactly.
/// A home-local runner binds any build of it, so every change to the
/// runner's semantics bumps it (spec 01 §8.4); the frames of each version
/// are pinned by a test.
pub const PROTOCOL_VERSION: u32 = 2;

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
