use super::*;
use crate::stream::hooks::DialectStreamHooks;
use plakous_http::SseError;

#[derive(Debug, PartialEq)]
enum TestError {
    Sse,
    Sequence,
    Done,
    AfterTerminal,
    Truncated,
    MissingTerminal,
    MissingDone,
    Limit(&'static str),
    Decode(&'static str),
}

#[derive(Debug, PartialEq)]
struct TestEvent {
    terminal: bool,
    data: String,
}

struct TestHooks {
    fail_decode: bool,
}

impl DialectStreamHooks for TestHooks {
    type Event = TestEvent;
    type Error = TestError;

    fn map_sse_error(_err: SseError, _limits: &ResponsesStreamLimits) -> Self::Error {
        TestError::Sse
    }

    fn map_sequence_error(_err: crate::SequenceValidationError) -> Self::Error {
        TestError::Sequence
    }

    fn map_done_forbidden() -> Self::Error {
        TestError::Done
    }

    fn map_event_after_terminal() -> Self::Error {
        TestError::AfterTerminal
    }

    fn map_truncated() -> Self::Error {
        TestError::Truncated
    }

    fn map_missing_terminal() -> Self::Error {
        TestError::MissingTerminal
    }

    fn map_missing_done() -> Self::Error {
        TestError::MissingDone
    }

    fn map_resource_limit(resource: &'static str, _maximum: usize) -> Self::Error {
        TestError::Limit(resource)
    }

    fn map_field_error(_err: ResponsesFieldError) -> Self::Error {
        TestError::Decode("field")
    }

    fn event_is_terminal(event: &Self::Event) -> bool {
        event.terminal
    }

    fn decode_json_event(
        &mut self,
        data: &str,
        _event_name: Option<&str>,
        _sequence: Option<u64>,
    ) -> Result<Option<Self::Event>, Self::Error> {
        if self.fail_decode {
            return Err(TestError::Decode("hook"));
        }
        let value: Value = serde_json::from_str(data).map_err(|_| TestError::Decode("json"))?;
        let terminal = value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "done");
        Ok(Some(TestEvent {
            terminal,
            data: data.to_owned(),
        }))
    }
}

fn frame(json: &str) -> Vec<u8> {
    format!("data: {json}\n\n").into_bytes()
}

#[test]
fn xai_profile_forbids_done_and_requires_typed_terminal() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::xai(),
        ResponsesStreamLimits::default(),
    );
    let mut hooks = TestHooks { fail_decode: false };
    let events = kernel
        .push(&frame(r#"{"type":"done","sequence_number":0}"#), &mut hooks)
        .unwrap_or_else(|error| panic!("push: {error:?}"));
    assert_eq!(events.len(), 1);
    assert!(kernel.typed_terminal_seen());
    assert!(matches!(
        kernel.push(b"data: [DONE]\n\n", &mut hooks),
        Err(TestError::AfterTerminal | TestError::Done)
    ));
}

#[test]
fn openrouter_profile_requires_done_after_terminal() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::openrouter(),
        ResponsesStreamLimits::default(),
    );
    let mut hooks = TestHooks { fail_decode: false };
    assert!(
        kernel
            .push(&frame(r#"{"type":"done","sequence_number":0}"#), &mut hooks)
            .is_ok()
    );
    let mut missing_done = kernel.clone();
    assert!(matches!(
        missing_done.finish(&mut hooks),
        Err(TestError::MissingDone)
    ));
    assert!(kernel.push(b"data: [DONE]\n\n", &mut hooks).is_ok());
    assert!(kernel.finish(&mut hooks).is_ok());
}

#[test]
fn openai_terminal_closes_the_event_boundary_and_ignores_buffered_trailers() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::openai_platform(),
        ResponsesStreamLimits::default(),
    )
    .with_typed_terminal_cutoff();
    let mut hooks = TestHooks { fail_decode: false };
    let bytes = concat!(
        "data: {\"type\":\"done\",\"sequence_number\":0}\n\n",
        "data: {\"type\":\"opaque.provider.trailer\",\"sequence_number\":1}\n\n",
        "data: [DONE]\n\n"
    );
    let events = kernel
        .push(bytes.as_bytes(), &mut hooks)
        .unwrap_or_else(|error| panic!("push: {error:?}"));
    assert_eq!(events.len(), 1);
    assert!(events[0].terminal);
    assert!(kernel.typed_terminal_seen());
    assert!(!kernel.done_marker_seen());
    assert!(kernel.finish(&mut hooks).is_ok());
}

#[test]
fn openai_terminal_cutoff_ignores_malformed_and_over_limit_same_chunk_suffix() {
    let terminal = b"data: {\"type\":\"done\",\"sequence_number\":0}\n\n";
    let limits =
        ResponsesStreamLimits::new(64, 256, 16).unwrap_or_else(|error| panic!("limits: {error}"));
    for suffix in [
        &[0xff, b'\n', b'\n'][..],
        &vec![b'x'; limits.max_total_bytes() + 1],
    ] {
        let mut kernel =
            ResponsesStreamKernel::new(ResponsesDialectProfile::openai_platform(), limits)
                .with_typed_terminal_cutoff();
        let mut hooks = TestHooks { fail_decode: false };
        let mut body = terminal.to_vec();
        body.extend_from_slice(suffix);
        let events = kernel
            .push(&body, &mut hooks)
            .unwrap_or_else(|error| panic!("push: {error:?}"));
        assert_eq!(events.len(), 1);
        assert!(events[0].terminal);
        assert_eq!(kernel.total_bytes, terminal.len());
        assert!(kernel.finish(&mut hooks).is_ok());
    }
}

#[test]
fn total_byte_limit_is_enforced() {
    let limits =
        ResponsesStreamLimits::new(32, 40, 16).unwrap_or_else(|error| panic!("limits: {error}"));
    let mut kernel = ResponsesStreamKernel::new(ResponsesDialectProfile::xai(), limits);
    let mut hooks = TestHooks { fail_decode: false };
    let err = kernel
        .push(&[b'x'; 41], &mut hooks)
        .err()
        .unwrap_or_else(|| panic!("expected over-limit error"));
    assert_eq!(err, TestError::Limit(resource::TOTAL_STREAM_BYTES));
}

#[test]
fn openai_auto_dispatch_residual_finish_decodes_unterminated_frame() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::openai_platform(),
        ResponsesStreamLimits::default(),
    );
    let mut hooks = TestHooks { fail_decode: false };
    // No trailing blank line: residual must be auto-dispatched at finish.
    let residual = b"data: {\"type\":\"done\",\"sequence_number\":0}\n";
    assert_eq!(
        kernel
            .push(residual, &mut hooks)
            .unwrap_or_else(|error| panic!("{error:?}")),
        [] as [TestEvent; 0]
    );
    let events = kernel
        .finish(&mut hooks)
        .unwrap_or_else(|error| panic!("finish residual: {error:?}"));
    assert_eq!(events.len(), 1);
    assert!(kernel.typed_terminal_seen());
}

#[test]
fn xai_blank_line_finish_rejects_residual_frame() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::xai(),
        ResponsesStreamLimits::default(),
    );
    let mut hooks = TestHooks { fail_decode: false };
    let residual = b"data: {\"type\":\"done\",\"sequence_number\":0}\n";
    assert_eq!(
        kernel
            .push(residual, &mut hooks)
            .unwrap_or_else(|error| panic!("{error:?}")),
        [] as [TestEvent; 0]
    );
    assert!(matches!(
        kernel.finish(&mut hooks),
        Err(TestError::Truncated)
    ));
}
