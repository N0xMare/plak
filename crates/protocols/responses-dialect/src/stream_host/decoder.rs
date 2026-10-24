use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use plakous_attempt::{is_decoding, mark_decode_failed, mark_decoded};
use plakous_responses::{
    DialectStreamHooks, ResponsesDecodeLimits, ResponsesFieldError, ResponsesStreamKernel,
    ResponsesStreamLimits, required_u32,
};
use serde_json::{Map, Value};

use super::{
    ResponsesFamilyHooks, ResponsesFamilyStreamError, ResponsesFamilyStreamEvent,
    ResponsesStreamDialectData, ResponsesStreamHostError,
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

/// Validates `output_index` against decode limits.
///
/// # Errors
///
/// Returns `E` when the field is missing/invalid or exceeds the item budget.
pub fn required_output_index<E>(
    object: &Map<String, Value>,
    limits: ResponsesDecodeLimits,
) -> Result<u32, E>
where
    E: From<ResponsesFieldError> + ResponsesStreamHostError,
{
    let index = required_u32(object, "output_index")?;
    let index_usize = usize::try_from(index).map_err(|_| {
        E::from_field_error(ResponsesFieldError::Invalid {
            field: "output_index",
        })
    })?;
    if index_usize >= limits.max_output_items() {
        return Err(E::resource_limit("output items", limits.max_output_items()));
    }
    Ok(index)
}

/// Admitted SSE decoder front-half around one kernel + hook state.
///
/// A decode failure poisons the operation, so pushes mutate the owned kernel and
/// hooks directly. This avoids cloning all framing, lifecycle, and projection
/// state for every transport fragment.
pub struct ResponsesSseDecoderFront<H> {
    attempt_lifecycle: Arc<AtomicU8>,
    kernel: ResponsesStreamKernel,
    hooks: H,
    failed: bool,
}

impl<H> ResponsesSseDecoderFront<H> {
    /// Builds a front-half decoder.
    #[must_use]
    pub fn new(attempt_lifecycle: Arc<AtomicU8>, kernel: ResponsesStreamKernel, hooks: H) -> Self {
        Self {
            attempt_lifecycle,
            kernel,
            hooks,
            failed: false,
        }
    }

    /// Returns a shared handle to dialect hooks.
    #[must_use]
    pub const fn hooks(&self) -> &H {
        &self.hooks
    }

    /// Returns the stream kernel.
    #[must_use]
    pub const fn kernel(&self) -> &ResponsesStreamKernel {
        &self.kernel
    }

    /// Whether the decoder is terminally failed.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }

    /// Feeds bytes through the kernel under attempt-admission checks.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn push<E>(&mut self, chunk: &[u8]) -> Result<Vec<H::Event>, E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        if self.failed {
            return Err(E::decoder_failed());
        }
        if !is_decoding(&self.attempt_lifecycle) {
            return Err(E::attempt_not_admitted());
        }
        match self.kernel.push(chunk, &mut self.hooks) {
            Ok(events) => Ok(events),
            Err(error) => {
                mark_decode_failed(&self.attempt_lifecycle);
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Finishes the stream, returns residual decoded events, and marks the
    /// attempt decoded on success.
    ///
    /// Auto-dispatch dialects may complete a final SSE frame only at EOF. Those
    /// events are semantic stream output and must be delivered to the caller.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn finish_events<E>(mut self) -> Result<Vec<H::Event>, E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        if self.failed {
            return Err(E::decoder_failed());
        }
        if !is_decoding(&self.attempt_lifecycle) {
            return Err(E::attempt_not_admitted());
        }
        match self.kernel.finish(&mut self.hooks) {
            Ok(residual) => {
                mark_decoded(&self.attempt_lifecycle).map_err(|_| E::attempt_not_admitted())?;
                Ok(residual)
            }
            Err(error) => {
                mark_decode_failed(&self.attempt_lifecycle);
                Err(error)
            }
        }
    }

    /// Finishes a stream whose dialect requires blank-line-closed frames.
    ///
    /// Existing xAI/OpenRouter callers use this compatibility convenience; its
    /// profiles cannot produce residual events. Auto-dispatch dialects must use
    /// [`Self::finish_events`] so EOF-decoded events are not discarded.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn finish<E>(self) -> Result<(), E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        let residual = self.finish_events()?;
        debug_assert!(residual.is_empty());
        Ok(())
    }
}

impl<H> fmt::Debug for ResponsesSseDecoderFront<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesSseDecoderFront")
            .field("attempt_decoding", &is_decoding(&self.attempt_lifecycle))
            .field("kernel", &self.kernel)
            .field("failed", &self.failed)
            .field("hooks", &"<redacted dialect hooks>")
            .finish_non_exhaustive()
    }
}
