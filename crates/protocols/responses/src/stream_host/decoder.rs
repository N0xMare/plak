use std::fmt;

use crate::{ResponsesStreamKernel, ResponsesStreamLimits};

use super::{
    ResponsesFamilyHooks, ResponsesFamilyStreamError, ResponsesFamilyStreamEvent,
    ResponsesStreamDialectData,
};

/// Concrete non-cloning Responses SSE decoder used by Codex-family migration.
pub struct ResponsesFamilySseDecoder {
    kernel: Option<ResponsesStreamKernel>,
    hooks: ResponsesFamilyHooks,
    failed: bool,
}

impl ResponsesFamilySseDecoder {
    /// Creates a decoder from immutable admitted family/dialect data.
    #[must_use]
    pub fn new(data: ResponsesStreamDialectData, limits: ResponsesStreamLimits) -> Self {
        let mut kernel = ResponsesStreamKernel::new(data.profile, limits);
        if data.typed_terminal_cutoff {
            kernel = kernel.with_typed_terminal_cutoff();
        }
        Self {
            kernel: Some(kernel),
            hooks: ResponsesFamilyHooks::new(data),
            failed: false,
        }
    }

    fn with_kernel_mut<T>(
        &mut self,
        operation: impl FnOnce(&mut ResponsesStreamKernel, &mut ResponsesFamilyHooks) -> T,
    ) -> T {
        let Some(mut kernel) = self.kernel.take() else {
            unreachable!("Responses kernel cannot be re-entered");
        };
        let output = operation(&mut kernel, &mut self.hooks);
        self.kernel = Some(kernel);
        output
    }

    /// Feeds arbitrary byte fragments and returns every completed event.
    ///
    /// # Errors
    ///
    /// Returns a terminal family error for malformed, over-limit, or invalid
    /// lifecycle input.
    pub fn push(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        if self.failed {
            return Err(ResponsesFamilyStreamError::DecoderFailed);
        }
        match self.with_kernel_mut(|kernel, hooks| kernel.push(bytes, hooks)) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Validates EOF and returns auto-dispatched residual events.
    ///
    /// # Errors
    ///
    /// Returns a terminal family error for truncation or missing terminal state.
    pub fn finish(
        &mut self,
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        if self.failed {
            return Err(ResponsesFamilyStreamError::DecoderFailed);
        }
        match self.with_kernel_mut(ResponsesStreamKernel::finish) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

impl fmt::Debug for ResponsesFamilySseDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesFamilySseDecoder")
            .field("kernel", &self.kernel)
            .field("expected_model", &"<redacted>")
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}
