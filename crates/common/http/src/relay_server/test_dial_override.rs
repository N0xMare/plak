//! The repository-only dial override (`fault-injection`): absent from
//! product builds.

use super::{GatewayService, Origin, RelayServer};

impl<S: GatewayService> RelayServer<S> {
    /// Repository tests only: dials `logical` (`https://host[:port]`) at the
    /// loopback `dial` origin instead, while every pin check still runs on
    /// the logical URL.
    #[must_use]
    pub fn with_dial_override(self, logical: &str, dial: Origin) -> Self {
        let logical = logical.to_owned();
        self.rebuild(move |inner| inner.dial_overrides.push((logical, dial)))
    }
}
