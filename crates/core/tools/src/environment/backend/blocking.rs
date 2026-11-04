//! The bounded blocking lane every synchronous filesystem operation runs on.

use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

use super::{ExecutionBackendError, ExecutionBackendFuture, io_error};

const MAX_BLOCKING_FILESYSTEM_OPERATIONS: usize = 8;

static FILESYSTEM_BLOCKING_LANES: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_BLOCKING_FILESYSTEM_OPERATIONS)));

pub(super) fn run_filesystem_blocking<T, F>(operation: F) -> ExecutionBackendFuture<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ExecutionBackendError> + Send + 'static,
{
    Box::pin(async move {
        let permit = Arc::clone(&FILESYSTEM_BLOCKING_LANES)
            .acquire_owned()
            .await
            .map_err(|_| io_error("filesystem blocking lane closed"))?;
        let result = tokio::task::spawn_blocking(operation)
            .await
            .map_err(|_| io_error("filesystem blocking operation failed to join"))?;
        drop(permit);
        result
    })
}
