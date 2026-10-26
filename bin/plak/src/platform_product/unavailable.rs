use crate::home::paths::PlakPaths;
use std::sync::Arc;

pub(crate) struct ProductPlatformCredentials;
impl ProductPlatformCredentials {
    pub(crate) fn shared(_: &PlakPaths) -> Arc<Self> {
        Arc::new(Self)
    }
}
