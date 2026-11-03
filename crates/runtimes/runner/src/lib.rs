//! The two ends of runner protocol v1 over a Plakous execution environment
//! (spec 01 §8.4), and an in-process loopback that joins them.
//!
//! [`RunnerService`] is the runner end: it answers requests from a local
//! [`plakous_tools::ExecutionEnvironment`] and each line's process sessions,
//! fences them by line owner, runs each effect-call and owner-control key at
//! most once, and carries services' stdio as credit-paced streams.
//! [`RunnerClient`] is the host end: a runner link that frames, keys and
//! fences each call, forwards yields and cancellations, pings the runner, and
//! replays by key after a reconnection. [`Loopback`] wires both over
//! in-memory streams for conformance; nothing here selects a runner for a
//! session.

mod client;
mod connection;
mod loopback;
mod service;
mod state;
mod streams;

pub use client::{ClientWindows, ConnectError, RunnerClient};
pub use loopback::{Loopback, LoopbackError};
pub use service::{RunnerIdentity, RunnerLimits, RunnerService};

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
