use super::Arc;
use crate::platform::{ApiModelLists, ApiWindow, PlatformCredentials};

#[derive(Clone)]
pub(super) struct ApiState {
    pub(super) lists: ApiModelLists,
    pub(super) window: ApiWindow,
}

impl ApiState {
    pub(super) fn new(_: Arc<dyn PlatformCredentials>) -> Self {
        Self {
            lists: ApiModelLists::Offline,
            window: ApiWindow::Working,
        }
    }
}
