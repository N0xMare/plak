//! Deterministic provider-neutral tool test double.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use plakous_model::{AttemptId, CallId, Digest, RunId, TurnId};

use plakous_tools::{
    RegisteredToolIdentity, ToolContext, ToolExecutor, ToolExposureBinding, ToolFuture,
    ToolInvocation, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// Redacted invocation evidence captured by [`ScriptedTool`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedToolInvocation {
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    call_id: CallId,
    identity: RegisteredToolIdentity,
    exposure: Option<ToolExposureBinding>,
    argument_digest: Digest,
    argument_bytes: usize,
}

impl CapturedToolInvocation {
    /// Returns the run identity.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Returns the turn identity.
    #[must_use]
    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Returns the attempt identity.
    #[must_use]
    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    /// Returns the call identity.
    #[must_use]
    pub const fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Returns the frozen registered identity.
    #[must_use]
    pub const fn identity(&self) -> &RegisteredToolIdentity {
        &self.identity
    }

    /// Returns the provider-facing exposure binding retained at execution.
    #[must_use]
    pub const fn exposure(&self) -> Option<&ToolExposureBinding> {
        self.exposure.as_ref()
    }

    /// Returns the canonical argument digest without argument content.
    #[must_use]
    pub const fn argument_digest(&self) -> Digest {
        self.argument_digest
    }

    /// Returns the exact argument byte count.
    #[must_use]
    pub const fn argument_bytes(&self) -> usize {
        self.argument_bytes
    }
}

/// One deterministic scripted executor action.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ScriptedToolStep {
    /// Complete immediately with the supplied outcome or runtime error.
    Complete(Result<ToolOutcome, ToolRuntimeError>),
    /// Remain pending until the invocation cancellation token fires.
    AwaitCancellation,
    /// Emit ordered progress updates, then complete.
    ProgressThenComplete {
        /// Ordered progress messages.
        messages: Vec<String>,
        /// Terminal result.
        result: Result<ToolOutcome, ToolRuntimeError>,
    },
}

/// FIFO deterministic serial tool implementation with redacted captured calls.
///
/// Overlapping calls fail without consuming an additional scripted step.
pub struct ScriptedTool {
    steps: Mutex<VecDeque<ScriptedToolStep>>,
    captured: Mutex<Vec<CapturedToolInvocation>>,
    active: AtomicBool,
}

impl fmt::Debug for ScriptedTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptedTool")
            .field("remaining_steps", &self.remaining_steps())
            .field("captured_count", &self.captured().len())
            .finish_non_exhaustive()
    }
}

impl ScriptedTool {
    /// Creates a tool from immediate FIFO terminal results.
    #[must_use]
    pub fn new(results: impl IntoIterator<Item = Result<ToolOutcome, ToolRuntimeError>>) -> Self {
        Self::from_steps(results.into_iter().map(ScriptedToolStep::Complete))
    }

    /// Creates a tool from complete FIFO scripted steps.
    #[must_use]
    pub fn from_steps(steps: impl IntoIterator<Item = ScriptedToolStep>) -> Self {
        Self {
            steps: Mutex::new(steps.into_iter().collect()),
            captured: Mutex::new(Vec::new()),
            active: AtomicBool::new(false),
        }
    }

    /// Returns cloned redacted captured calls.
    #[must_use]
    pub fn captured(&self) -> Vec<CapturedToolInvocation> {
        match self.captured.lock() {
            Ok(captured) => captured.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Returns unconsumed scripted step count.
    #[must_use]
    pub fn remaining_steps(&self) -> usize {
        match self.steps.lock() {
            Ok(steps) => steps.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    fn record(&self, invocation: &ToolInvocation) {
        let captured = CapturedToolInvocation {
            run_id: invocation.run_id(),
            turn_id: invocation.turn_id(),
            attempt_id: invocation.attempt_id(),
            call_id: invocation.call_id(),
            identity: invocation.identity().clone(),
            exposure: invocation.exposure().cloned(),
            argument_digest: invocation.arguments().digest(),
            argument_bytes: invocation.arguments().raw_bytes(),
        };
        match self.captured.lock() {
            Ok(mut invocations) => invocations.push(captured),
            Err(poisoned) => poisoned.into_inner().push(captured),
        }
    }

    fn pop_step(&self) -> Option<ScriptedToolStep> {
        match self.steps.lock() {
            Ok(mut steps) => steps.pop_front(),
            Err(poisoned) => poisoned.into_inner().pop_front(),
        }
    }
}

struct ActiveScriptedCall {
    tool: Arc<ScriptedTool>,
}

impl Drop for ActiveScriptedCall {
    fn drop(&mut self) {
        self.tool.active.store(false, Ordering::Release);
    }
}

impl ToolExecutor for ScriptedTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        if self.active.swap(true, Ordering::AcqRel) {
            return Box::pin(async {
                Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "overlapping scripted tool execution is unsupported",
                ))
            });
        }
        let active = ActiveScriptedCall {
            tool: Arc::clone(&self),
        };
        self.record(&invocation);
        let step = self.pop_step();
        Box::pin(async move {
            let _active = active;
            match step {
                Some(ScriptedToolStep::Complete(result)) => result,
                Some(ScriptedToolStep::AwaitCancellation) => {
                    context.cancellation().cancelled().await;
                    Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Cancelled,
                        "scripted tool execution was cancelled",
                    ))
                }
                Some(ScriptedToolStep::ProgressThenComplete { messages, result }) => {
                    for message in messages {
                        context.progress().report(message, None, None)?;
                    }
                    result
                }
                None => Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::ScriptExhausted,
                    "scripted tool has no remaining execution step",
                )),
            }
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use plakous_tools::assert_tool_executor_object_safe;

    #[test]
    fn trait_is_object_safe() {
        let tool = ScriptedTool::new(std::iter::empty());
        assert_tool_executor_object_safe(&tool);
    }
}
