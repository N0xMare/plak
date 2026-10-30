use super::*;

#[tokio::test]
async fn notice_does_not_poll_a_terminal_past_queued_nested_calls() {
    let mut in_flight = false;
    let mut observation = LocalCompositionObservation::Execute {
        cell_id: "cell".into(),
        response: Box::pin(async {
            panic!("queued nested calls prohibit a terminal-preferring poll")
        }),
    };
    assert!(matches!(
        observation
            .receive(&mut in_flight, true, None, true, false)
            .await,
        ObservedResponse::Yielded(_)
    ));
}

#[tokio::test]
async fn notice_before_resume_preempts_without_polling_the_wait() {
    let mut observation = LocalCompositionObservation::Wait {
        cell_id: "cell".into(),
        response: Box::pin(async { panic!("notice must preempt before Resume") }),
        observation_timeout: Duration::from_secs(1),
    };
    let mut in_flight = false;
    assert!(matches!(
        observation
            .receive(&mut in_flight, true, None, true, true)
            .await,
        ObservedResponse::Yielded(LocalCompositionResponse::Yielded { .. })
    ));
    assert!(!in_flight);
}

#[tokio::test]
async fn notice_after_resume_retains_the_in_flight_wait() {
    let (done, completion) = tokio::sync::oneshot::channel();
    let mut observation = LocalCompositionObservation::Wait {
        cell_id: "cell".into(),
        response: Box::pin(async move {
            let _ = completion.await;
            Ok(LocalCompositionWaitOutcome::Live(
                LocalCompositionResponse::Result {
                    cell_id: "cell".into(),
                    items: Vec::new(),
                    error: None,
                },
            ))
        }),
        observation_timeout: Duration::from_secs(1),
    };
    let mut in_flight = false;
    {
        let mut receive = Box::pin(observation.receive(&mut in_flight, false, None, true, true));
        assert!(futures_util::poll!(receive.as_mut()).is_pending());
    }
    // A new notice must not replace the outstanding observation with a
    // synthetic running answer, even while its response remains pending.
    {
        let mut receive = Box::pin(observation.receive(&mut in_flight, true, None, true, true));
        assert!(futures_util::poll!(receive.as_mut()).is_pending());
    }
    assert!(done.send(()).is_ok());
    assert!(matches!(
        observation
            .receive(&mut in_flight, true, None, true, true)
            .await,
        ObservedResponse::Wait(Ok(LocalCompositionWaitOutcome::Live(
            LocalCompositionResponse::Result { .. }
        )))
    ));
}

#[tokio::test]
async fn host_window_notice_after_poll_yields_without_a_resume() {
    let mut observation = LocalCompositionObservation::Wait {
        cell_id: "cell".into(),
        response: Box::pin(std::future::pending()),
        observation_timeout: Duration::from_secs(1),
    };
    let mut in_flight = false;
    {
        let mut receive = Box::pin(observation.receive(&mut in_flight, false, None, false, true));
        assert!(futures_util::poll!(receive.as_mut()).is_pending());
    }
    let mut receive = Box::pin(observation.receive(&mut in_flight, true, None, false, true));
    assert!(matches!(
        futures_util::poll!(receive.as_mut()),
        std::task::Poll::Ready(ObservedResponse::Yielded(
            LocalCompositionResponse::Yielded { .. }
        ))
    ));
}

#[tokio::test]
async fn host_window_terminal_beats_a_notice_without_resuming() {
    let mut in_flight = false;
    let mut observation = LocalCompositionObservation::Wait {
        cell_id: "cell".into(),
        response: Box::pin(async {
            Ok(LocalCompositionWaitOutcome::Live(
                LocalCompositionResponse::Result {
                    cell_id: "cell".into(),
                    items: Vec::new(),
                    error: None,
                },
            ))
        }),
        observation_timeout: Duration::from_secs(1),
    };
    assert!(matches!(
        observation
            .receive(&mut in_flight, true, None, false, true)
            .await,
        ObservedResponse::Wait(Ok(LocalCompositionWaitOutcome::Live(
            LocalCompositionResponse::Result { .. }
        )))
    ));
}
