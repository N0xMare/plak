//! The two ends of runner protocol v1 over a Plakous execution environment
//! (spec 01 §8.4), and an in-process loopback that joins them.
//!
//! [`RunnerService`] is the runner end: it answers requests from a local
//! [`plakous_tools::ExecutionEnvironment`], fences them by line owner and
//! runs each effect-call key at most once. [`RunnerClient`] is the host end:
//! a runner link that frames, keys and fences each call and replays by key
//! after a reconnection. [`Loopback`] wires both over in-memory streams for
//! conformance; nothing here selects a runner for a session.

mod client;
mod connection;
mod loopback;
mod service;
mod state;

pub use client::{ClientWindows, ConnectError, RunnerClient};
pub use loopback::{Loopback, LoopbackError};
pub use service::{RunnerIdentity, RunnerService};

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
