use super::{Arc, ProductStateRoots};
use crate::platform::{ApiModelLists, ApiWindow, PlatformCredentials};

#[derive(Clone)]
pub(super) struct ApiState {
    credentials: Arc<dyn PlatformCredentials>,
    pub(super) lists: ApiModelLists,
    pub(super) window: ApiWindow,
}

impl ApiState {
    pub(super) fn new(credentials: Arc<dyn PlatformCredentials>) -> Self {
        Self {
            credentials,
            lists: ApiModelLists::Offline,
            window: ApiWindow::Working,
        }
    }
}

impl ProductStateRoots {
    pub(crate) fn platform_credentials(&self) -> &dyn PlatformCredentials {
        self.api.credentials.as_ref()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_test_platform_credentials(
        mut self,
        credentials: Arc<dyn PlatformCredentials>,
    ) -> Self {
        self.api.credentials = credentials;
        self
    }
}
