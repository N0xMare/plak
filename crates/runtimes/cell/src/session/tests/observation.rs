use super::*;

async fn release_on_stop(stop: bool) {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Read(vec![HostFrame::Shutdown]),
        ],
    );
    let (_, session, _events) = session(&program).await;
    let mut initial = execute(&session).await.initial;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(initial.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let mut observation = tokio::spawn(initial);
    if stop {
        session.stop().await.expect("voluntary stop");
    } else {
        session.shutdown().await.expect("voluntary shutdown");
    }
    let answer = tokio::time::timeout(Duration::from_secs(5), &mut observation).await;
    if answer.is_err() {
        observation.abort();
        let _ = observation.await;
    }
    assert!(
        matches!(answer, Ok(Ok(Err(ref loss))) if loss == "cell worker stopped"),
        "{answer:?}"
    );
}

#[tokio::test]
async fn shutdown_releases_an_in_flight_parked_observation() {
    release_on_stop(false).await;
}

#[tokio::test]
async fn stop_releases_an_in_flight_parked_observation() {
    release_on_stop(true).await;
}

#[tokio::test]
async fn an_empty_buffered_yield_resumes_for_a_real_window() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![
                WorkerFrame::Yielded { cell: 1 },
                WorkerFrame::Output {
                    cell: 1,
                    item: OutputItem::Notify {
                        text: "parked".into(),
                    },
                },
            ]),
            Step::Read(vec![HostFrame::Resume {
                cell: 1,
                window: WINDOW,
            }]),
            Step::Write(vec![done(1)]),
        ],
    );
    let (_, session, mut events) = session(&program).await;
    drop(execute(&session).await.initial);
    let Some(CellSessionEvent::Notification { responder, .. }) = events.receive().await else {
        panic!("yield buffered")
    };
    assert!(responder.acknowledge());
    let response = tokio::time::timeout(Duration::from_secs(5), session.wait("1", WINDOW)).await;
    let _ = session.shutdown().await;
    assert!(
        matches!(
            response,
            Ok(WaitOutcome::Live(CellResponse::Result { error: None, .. }))
        ),
        "{response:?}"
    );
}

#[tokio::test]
async fn buffered_terminal_peek_does_not_resume() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![done(1)]),
        ],
    );
    let (_, session, _events) = session(&program).await;
    drop(execute(&session).await.initial);
    abandonment::finish_pump(&session).await;
    assert_eq!(
        session.try_take("1").await,
        Some(WaitOutcome::Live(CellResponse::Result {
            cell: 1,
            items: Vec::new(),
            error: None,
        }))
    );
    assert_eq!(session.try_take("1").await, None);
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn buffered_peek_never_resumes_a_parked_cell() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker_as(
        directory.path(),
        QUICKJS_WORKER,
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![WorkerFrame::Yielded { cell: 1 }]),
            Step::ReadExact(vec![HostFrame::Shutdown]),
        ],
        "printf ok > \"$0.ok\"\n",
    );
    let (_, session, _events) = session(&program).await;
    assert!(matches!(
        execute(&session).await.initial.await,
        Ok(CellResponse::Yielded { .. })
    ));
    let buffered = session.try_take("1").await;
    let _ = session.shutdown().await;
    assert_eq!(buffered, None);
    assert!(
        directory.path().join("worker.ok").is_file(),
        "no Resume preceded Shutdown"
    );
}

async fn overflowing_yield(byte_limit: bool) {
    let directory = tempfile::tempdir().expect("directory");
    let frames = if byte_limit {
        vec![
            text(&"x".repeat(MAX_COLLECTED_OUTPUT_BYTES / 2)),
            text(&"y".repeat(MAX_COLLECTED_OUTPUT_BYTES / 2)),
            text("overflow"),
        ]
    } else {
        (0..=MAX_COLLECTED_OUTPUT_ITEMS)
            .map(|_| text("item"))
            .collect()
    };
    let mut steps = vec![Step::Read(vec![configure(), start(1)])];
    let batches = frames.chunks(16).len();
    for (index, batch) in frames.chunks(16).enumerate() {
        let call = u64::try_from(index + 1).expect("call");
        let mut output = batch.to_vec();
        output.push(nested_call(1, call));
        steps.push(Step::Write(output));
        steps.push(Step::Read(vec![nested_result(call)]));
        steps.push(Step::Write(vec![WorkerFrame::Delivery {
            cell: 1,
            call,
            verdict: DeliveryVerdict::Accepted,
        }]));
    }
    steps.push(Step::Write(vec![
        WorkerFrame::Yielded { cell: 1 },
        text("later output"),
        done(1),
    ]));
    let program = scripted_worker(directory.path(), &steps);
    let (client, session, mut events) = session(&program).await;
    drop(execute(&session).await.initial);
    for _ in 0..batches {
        let event = tokio::select! {
            biased;
            event = events.receive() => event,
            _ = client.wait_loss() => None,
        };
        let Some(CellSessionEvent::NestedCall { responder, .. }) = event else {
            break;
        };
        assert_eq!(
            responder.resolve(serde_json::json!(7)).await,
            NestedDeliveryOutcome::Accepted
        );
    }
    abandonment::finish_pump(&session).await;
    let response = session.wait("1", WINDOW).await;
    let _ = session.shutdown().await;
    let WaitOutcome::Live(CellResponse::Result {
        items, error: None, ..
    }) = response
    else {
        panic!("overflow must retain the terminal: {response:?}")
    };
    assert!(items.len() <= MAX_COLLECTED_OUTPUT_ITEMS);
    assert!(items.iter().map(output_bytes).sum::<usize>() <= MAX_COLLECTED_OUTPUT_BYTES);
    assert!(
        matches!(items.last(), Some(OutputItem::Text { text }) if text.contains("output omitted"))
    );
}

#[tokio::test]
async fn a_single_yield_byte_overflow_keeps_the_terminal() {
    overflowing_yield(true).await;
}

#[tokio::test]
async fn a_single_yield_item_overflow_keeps_the_terminal() {
    overflowing_yield(false).await;
}

#[tokio::test]
async fn expedite_rearms_the_same_resume_with_zero_window() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![WorkerFrame::Yielded { cell: 1 }]),
            Step::ReadExact(vec![HostFrame::Resume {
                cell: 1,
                window: 600_000,
            }]),
            Step::ReadExact(vec![HostFrame::Resume { cell: 1, window: 0 }]),
            Step::Write(vec![text("expedited"), WorkerFrame::Yielded { cell: 1 }]),
            Step::Read(vec![HostFrame::Shutdown]),
        ],
    );
    let (_, session, _events) = session(&program).await;
    assert!(matches!(
        execute(&session).await.initial.await,
        Ok(CellResponse::Yielded { .. })
    ));
    let mut wait = Box::pin(session.wait("1", 600_000));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    session.expedite("1").await.expect("expedite");
    let response = tokio::time::timeout(Duration::from_secs(5), wait).await;
    session.shutdown().await.expect("voluntary shutdown");
    assert_eq!(
        response,
        Ok(WaitOutcome::Live(CellResponse::Yielded {
            cell: 1,
            items: vec![OutputItem::Text {
                text: "expedited".into()
            }],
        }))
    );
}

#[tokio::test]
async fn expedite_racing_an_unpublished_yield_coalesces_the_extra_step() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![WorkerFrame::Yielded { cell: 1 }]),
            Step::ReadExact(vec![HostFrame::Resume {
                cell: 1,
                window: WINDOW,
            }]),
            Step::Write(vec![nested_call(1, 7)]),
            Step::ReadExact(vec![nested_result(7)]),
            Step::Write(vec![
                WorkerFrame::Delivery {
                    cell: 1,
                    call: 7,
                    verdict: DeliveryVerdict::Accepted,
                },
                text("already parked"),
                WorkerFrame::Yielded { cell: 1 },
            ]),
            Step::ReadExact(vec![HostFrame::Resume { cell: 1, window: 0 }]),
            Step::Write(vec![
                text("zero-window step"),
                WorkerFrame::Yielded { cell: 1 },
                done(1),
            ]),
        ],
    );
    let (_, session, mut events) = session(&program).await;
    execute(&session)
        .await
        .initial
        .await
        .expect("initial yield");
    let mut wait = Box::pin(session.wait("1", WINDOW));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(wait);
    let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
        panic!("nested gate");
    };
    // Hold publication while the worker has already parked and is awaiting
    // its next frame. Resume(0) may then advance one unobserved step.
    let publication = session.completions.lock().await;
    assert_eq!(
        responder.resolve(serde_json::json!(7)).await,
        NestedDeliveryOutcome::Accepted
    );
    session
        .expedite("1")
        .await
        .expect("expedite before publication");
    drop(publication);
    abandonment::finish_pump(&session).await;
    let response = session.wait("1", WINDOW).await;
    let _ = session.shutdown().await;
    assert_eq!(
        response,
        WaitOutcome::Live(CellResponse::Result {
            cell: 1,
            items: vec![
                OutputItem::Text {
                    text: "already parked".into()
                },
                OutputItem::Text {
                    text: "zero-window step".into()
                }
            ],
            error: None,
        })
    );
}
