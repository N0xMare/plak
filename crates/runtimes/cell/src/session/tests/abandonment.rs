use super::*;

pub(super) async fn finish_pump(session: &CellSession) {
    let mut pump = session.pump.lock().await.take().expect("pump");
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(Duration::from_secs(30), pump.0.take().expect("task"))
        .await
        .expect("worker completed")
        .expect("pump completed");
}

async fn orphaned_yields(count: usize) {
    let directory = tempfile::tempdir().expect("directory");
    let mut frames = Vec::new();
    for index in 0..count {
        frames.push(text(&index.to_string()));
        frames.push(WorkerFrame::Yielded { cell: 1 });
    }
    frames.push(text("terminal"));
    frames.push(done(1));
    let program = scripted_worker(
        directory.path(),
        &[Step::Read(vec![configure(), start(1)]), Step::Write(frames)],
    );
    let (_, session, _events) = session(&program).await;
    drop(execute(&session).await.initial);
    finish_pump(&session).await;
    let expected = (0..count)
        .map(|index| OutputItem::Text {
            text: index.to_string(),
        })
        .chain([OutputItem::Text {
            text: "terminal".into(),
        }])
        .collect::<Vec<_>>();
    let mut stale_yields = 0;
    let mut observed = Vec::new();
    loop {
        match session.wait("1", WINDOW).await {
            WaitOutcome::Live(CellResponse::Yielded { items, .. }) => {
                stale_yields += 1;
                observed.extend(items);
            }
            WaitOutcome::Live(CellResponse::Result { items, error, .. }) => {
                assert_eq!(error, None);
                observed.extend(items);
                break;
            }
            lost => panic!("the cell terminal was dropped: {lost:?}"),
        }
    }
    assert_eq!(observed, expected);
    assert_eq!(stale_yields, 0, "a buffered terminal precedes stale yields");
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn orphaned_yields_do_not_drop_the_cell_terminal() {
    orphaned_yields(2).await;
}

#[tokio::test]
async fn orphaned_yield_overflow_does_not_drop_the_cell_terminal() {
    orphaned_yields(4).await;
}

#[tokio::test]
async fn orphaned_yields_do_not_hide_a_true_worker_loss() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![WorkerFrame::Yielded { cell: 1 }; 4]),
        ],
    );
    let (_, session, _events) = session(&program).await;
    drop(execute(&session).await.initial);
    finish_pump(&session).await;
    let WaitOutcome::Lost(loss) = session.wait("1", WINDOW).await else {
        panic!("a buffered worker loss precedes orphaned yields")
    };
    assert!(
        !loss.contains("completion closed"),
        "the original loss is retained: {loss}"
    );
    assert!(
        !loss.contains("completion queue"),
        "queue saturation is not a loss: {loss}"
    );
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn abandoned_resume_retains_the_cell_terminal() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker(
        directory.path(),
        &[
            Step::Read(vec![configure(), start(1)]),
            Step::Write(vec![WorkerFrame::Yielded { cell: 1 }]),
            Step::Read(vec![HostFrame::Resume {
                cell: 1,
                window: WINDOW,
            }]),
            Step::Write(vec![
                text("resumed"),
                WorkerFrame::Yielded { cell: 1 },
                done(1),
            ]),
        ],
    );
    let (_, session, _events) = session(&program).await;
    assert!(matches!(
        execute(&session).await.initial.await,
        Ok(CellResponse::Yielded { .. })
    ));
    let mut wait = Box::pin(session.wait("1", WINDOW));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(wait);
    finish_pump(&session).await;
    assert_eq!(
        session.wait("1", WINDOW).await,
        WaitOutcome::Live(CellResponse::Result {
            cell: 1,
            items: vec![OutputItem::Text {
                text: "resumed".into()
            }],
            error: None,
        })
    );
    let _ = session.shutdown().await;
}

#[tokio::test]
async fn abandoned_host_window_observations_retain_the_terminal() {
    let directory = tempfile::tempdir().expect("directory");
    let program = scripted_worker_as(
        directory.path(),
        crate::protocol::MONTY_WORKER,
        &[
            Step::Read(vec![monty_configure(), monty_start(1)]),
            Step::Write(vec![
                text("kept"),
                nested_call(1, 7),
                WorkerFrame::Waiting {
                    cell: 1,
                    calls: vec![7],
                },
            ]),
            Step::Read(vec![nested_result(7)]),
            Step::Write(vec![
                WorkerFrame::Delivery {
                    cell: 1,
                    call: 7,
                    verdict: DeliveryVerdict::Accepted,
                },
                done(1),
            ]),
        ],
        "",
    );
    let (session, mut events) = monty_session(&program, Duration::from_secs(30)).await;
    let mut initial = monty_execute(&session, WINDOW).await.initial;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(initial.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(initial);
    let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
        panic!("the worker is held at its nested call")
    };
    // Abandon a second subscribed observer while the worker is still held.
    let mut wait = Box::pin(session.wait("1", WINDOW));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(wait.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(wait);
    assert_eq!(
        responder.resolve(serde_json::json!(7)).await,
        NestedDeliveryOutcome::Accepted
    );
    finish_pump(&session).await;
    assert_eq!(
        session.wait("1", WINDOW).await,
        WaitOutcome::Live(CellResponse::Result {
            cell: 1,
            items: vec![OutputItem::Text {
                text: "kept".into()
            }],
            error: None,
        })
    );
    let _ = session.shutdown().await;
}
