//! Protocol types for the product agent tree runtime:
//! templates, spawn tiers, sealed candidates, and adoption.
//!
//! The wire types themselves live in `plakous-host-protocol` so an external
//! `plak agent` client can depend on them without the host. This module is
//! their host-side name: every internal path stays valid through the
//! re-exports below. Combination rules and UTF-8 byte budgets are product-side
//! after a closed V2-shaped object parse (`deny_unknown_fields`).

pub use plakous_host_protocol::topology::*;
