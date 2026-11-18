use crate::home::paths::PlakPaths;
use std::sync::Arc;

pub(crate) struct ProductPlatformCredentials;
impl ProductPlatformCredentials {
    pub(crate) fn shared(_: &PlakPaths, _: crate::gateway::HostCustody) -> Arc<Self> {
        Arc::new(Self)
    }
}
