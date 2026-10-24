use futures_util::StreamExt as _;

use super::*;

/// A wire whose errors carry their own text, so a test can tell which side
/// of the join produced the terminal.
enum FakeWire {}

impl AttemptWire for FakeWire {
    type Response = ();
    type AttemptError = &'static str;
    type ProtocolError = &'static str;

    fn model_error(_descriptor: &ResolvedModelDescriptor, error: &&'static str) -> ModelError {
        ModelError::new(ModelErrorKind::Protocol, format!("operation: {error}"))
    }

    fn observation_error(
        _descriptor: &ResolvedModelDescriptor,
        error: &&'static str,
    ) -> ModelError {
        ModelError::new(ModelErrorKind::Transport, format!("observation: {error}"))
    }
}

fn diagnostic(text: &'static str) -> ModelEvent {
    ModelEvent::Diagnostic(ModelDiagnostic::new(text))
}

fn diagnostic_text(event: Option<Result<ModelEvent, ModelError>>) -> Option<String> {
    match event {
        Some(Ok(ModelEvent::Diagnostic(diagnostic))) => {
            Some(diagnostic.message().as_str().to_owned())
        }
        _ => None,
    }
}

fn joined(
    pre_dispatch: &[&'static str],
    result: Result<(), &'static str>,
    events: Vec<Result<ModelEvent, &'static str>>,
) -> JoinedStream<
    FakeWire,
    futures_util::stream::Iter<std::vec::IntoIter<Result<ModelEvent, &'static str>>>,
> {
    JoinedStream::new(
        crate::chat::test_descriptor(),
        pre_dispatch
            .iter()
            .map(|text| ModelDiagnostic::new(*text))
            .collect(),
        Box::pin(std::future::ready(result)),
        futures_util::stream::iter(events),
    )
}

#[tokio::test]
async fn pre_dispatch_diagnostics_lead_and_the_operation_error_wins_the_terminal() {
    let mut stream = joined(
        &["replay stripped"],
        Err("refused"),
        vec![Ok(diagnostic("streamed")), Err("broken")],
    );
    assert_eq!(
        diagnostic_text(stream.next().await).as_deref(),
        Some("replay stripped")
    );
    assert_eq!(
        diagnostic_text(stream.next().await).as_deref(),
        Some("streamed")
    );
    let Some(Err(terminal)) = stream.next().await else {
        panic!("missing terminal error");
    };
    assert_eq!(terminal.kind(), ModelErrorKind::Protocol);
    assert!(
        terminal.to_string().contains("operation: refused"),
        "{terminal}"
    );
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn a_successful_operation_publishes_the_observation_error() {
    let mut stream = joined(&[], Ok(()), vec![Err("broken")]);
    let Some(Err(terminal)) = stream.next().await else {
        panic!("missing observation error");
    };
    assert_eq!(terminal.kind(), ModelErrorKind::Transport);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn a_clean_attempt_ends_without_a_terminal_error() {
    let mut stream = joined(&[], Ok(()), vec![Ok(diagnostic("only"))]);
    assert!(matches!(
        stream.next().await,
        Some(Ok(ModelEvent::Diagnostic(_)))
    ));
    assert!(stream.next().await.is_none());
    assert!(stream.next().await.is_none());
}

#[test]
fn sse_content_type_ignores_parameters_case_and_space() {
    for accepted in [
        "text/event-stream",
        "text/event-stream; charset=utf-8",
        " Text/Event-Stream ;charset=utf-8",
    ] {
        assert!(is_sse_content_type(accepted), "{accepted}");
    }
    for refused in [
        "",
        "application/json",
        "text/event-streams",
        "text/plain; text/event-stream",
    ] {
        assert!(!is_sse_content_type(refused), "{refused}");
    }
}
