use super::*;
use crate::ResponsesStreamDialectData;

fn hooks() -> ResponsesFamilyHooks {
    let mut hooks = ResponsesFamilyHooks::new(ResponsesStreamDialectData::codex("m"));
    hooks
        .decode_json_event(
            r#"{"type":"response.created","response":{"id":"r","model":"m"}}"#,
            None,
            None,
        )
        .expect("creation");
    hooks
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
    let mut hooks = hooks();
    let events = kernel
        .push(&frame(r#"{"type":"response.completed","sequence_number":0,"response":{"object":"response","id":"r","model":"m","status":"completed","output":[]}}"#), &mut hooks)
        .unwrap_or_else(|error| panic!("push: {error:?}"));
    assert_eq!(events.len(), 1);
    assert!(kernel.typed_terminal_seen());
    assert!(matches!(
        kernel.push(b"data: [DONE]\n\n", &mut hooks),
        Err(ResponsesFamilyStreamError::EventAfterTerminal | ResponsesFamilyStreamError::DonePolicy)
    ));
}

#[test]
fn openrouter_profile_requires_done_after_terminal() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::openrouter(),
        ResponsesStreamLimits::default(),
    );
    let mut hooks = hooks();
    assert!(
        kernel
            .push(&frame(r#"{"type":"response.completed","sequence_number":0,"response":{"object":"response","id":"r","model":"m","status":"completed","output":[]}}"#), &mut hooks)
            .is_ok()
    );
    let mut missing_done = kernel.clone();
    assert!(matches!(
        missing_done.finish(&mut hooks),
        Err(ResponsesFamilyStreamError::DonePolicy)
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
    let mut hooks = hooks();
    let bytes = concat!(
        "data: {\"type\":\"response.completed\",\"sequence_number\":0,\"response\":{\"object\":\"response\",\"id\":\"r\",\"model\":\"m\",\"status\":\"completed\",\"output\":[]}}\n\n",
        "data: {\"type\":\"opaque.provider.trailer\",\"sequence_number\":1}\n\n",
        "data: [DONE]\n\n"
    );
    let events = kernel
        .push(bytes.as_bytes(), &mut hooks)
        .unwrap_or_else(|error| panic!("push: {error:?}"));
    assert_eq!(events.len(), 1);
    assert!(events[0].is_terminal());
    assert!(kernel.typed_terminal_seen());
    assert!(!kernel.done_marker_seen());
    assert!(kernel.finish(&mut hooks).is_ok());
}

#[test]
fn openai_terminal_cutoff_ignores_malformed_and_over_limit_same_chunk_suffix() {
    let terminal = b"data: {\"type\":\"response.completed\",\"sequence_number\":0,\"response\":{\"object\":\"response\",\"id\":\"r\",\"model\":\"m\",\"status\":\"completed\",\"output\":[]}}\n\n";
    let limits =
        ResponsesStreamLimits::new(256, 512, 16).unwrap_or_else(|error| panic!("limits: {error}"));
    for suffix in [
        &[0xff, b'\n', b'\n'][..],
        &vec![b'x'; limits.max_total_bytes() + 1],
    ] {
        let mut kernel =
            ResponsesStreamKernel::new(ResponsesDialectProfile::openai_platform(), limits)
                .with_typed_terminal_cutoff();
        let mut hooks = hooks();
        let mut body = terminal.to_vec();
        body.extend_from_slice(suffix);
        let events = kernel
            .push(&body, &mut hooks)
            .unwrap_or_else(|error| panic!("push: {error:?}"));
        assert_eq!(events.len(), 1);
        assert!(events[0].is_terminal());
        assert_eq!(kernel.total_bytes, terminal.len());
        assert!(kernel.finish(&mut hooks).is_ok());
    }
}

#[test]
fn total_byte_limit_is_enforced() {
    let limits =
        ResponsesStreamLimits::new(32, 40, 16).unwrap_or_else(|error| panic!("limits: {error}"));
    let mut kernel = ResponsesStreamKernel::new(ResponsesDialectProfile::xai(), limits);
    let mut hooks = hooks();
    let err = kernel
        .push(&[b'x'; 41], &mut hooks)
        .err()
        .unwrap_or_else(|| panic!("expected over-limit error"));
    assert_eq!(
        err,
        ResponsesFamilyStreamError::ResourceLimit(resource::TOTAL_STREAM_BYTES)
    );
}

#[test]
fn openai_auto_dispatch_residual_finish_decodes_unterminated_frame() {
    let mut kernel = ResponsesStreamKernel::new(
        ResponsesDialectProfile::openai_platform(),
        ResponsesStreamLimits::default(),
    );
    let mut hooks = hooks();
    // No trailing blank line: residual must be auto-dispatched at finish.
    let residual = b"data: {\"type\":\"response.completed\",\"sequence_number\":0,\"response\":{\"object\":\"response\",\"id\":\"r\",\"model\":\"m\",\"status\":\"completed\",\"output\":[]}}\n";
    assert_eq!(
        kernel
            .push(residual, &mut hooks)
            .unwrap_or_else(|error| panic!("{error:?}")),
        [] as [ResponsesFamilyStreamEvent; 0]
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
    let mut hooks = hooks();
    let residual = b"data: {\"type\":\"response.completed\",\"sequence_number\":0,\"response\":{\"object\":\"response\",\"id\":\"r\",\"model\":\"m\",\"status\":\"completed\",\"output\":[]}}\n";
    assert_eq!(
        kernel
            .push(residual, &mut hooks)
            .unwrap_or_else(|error| panic!("{error:?}")),
        [] as [ResponsesFamilyStreamEvent; 0]
    );
    assert!(matches!(
        kernel.finish(&mut hooks),
        Err(ResponsesFamilyStreamError::Truncated)
    ));
}
