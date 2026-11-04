use super::*;

#[tokio::test]
async fn overdeep_stored_start_is_refused_before_enqueue_and_host_stays_usable() {
    let host = EmbeddedHost::spawn().expect("host");
    let value = (0..41).fold(Value::Null, |value, _| serde_json::json!([value]));
    let error = host
        .start_cell(
            1,
            "throw new Error('refused source ran');",
            HashMap::from([("deep".into(), value)]),
            vec![],
            10_000,
        )
        .expect_err("stored depth refused");
    assert_eq!(
        error,
        "stored value \"deep\" nests 41 containers; limit is 40"
    );
    host.start_cell(2, "store('usable', true);", HashMap::new(), vec![], 10_000)
        .expect("valid start");
    // A liveness bound only: generous, so load never trips it.
    let event = tokio::time::timeout(Duration::from_secs(30), host.read_event())
        .await
        .expect("terminal bound")
        .expect("host remains usable");
    assert!(
        matches!(event, RuntimeEvent::Done { cell_id: 2, ref stored } if stored.get("usable") == Some(&Value::Bool(true))),
        "{event:?}"
    );
    host.terminate().await.expect("joined");
}

#[tokio::test]
async fn terminate_remains_sticky_across_dequeued_start() {
    let host = EmbeddedHost::spawn().expect("host");
    let gate = Arc::new(std::sync::Barrier::new(2));
    host.command_tx
        .send(HostCommand::Start(StartExecution {
            execution_id: 1,
            source: "while (true) {}".into(),
            tools: vec![],
            stored: HashMap::new(),
            yield_time_ms: 10_000,
            before_start: Some(Arc::clone(&gate)),
        }))
        .expect("queued start");
    gate.wait();
    let stopped = Arc::clone(&host.stopped);
    let release = thread::spawn(move || {
        while !stopped.load(Ordering::Acquire) {
            thread::yield_now();
        }
        gate.wait();
    });
    let result = host.terminate().await;
    release.join().expect("start gate released");
    if result.is_err() {
        // Even a failed regression must not leave a busy worker behind.
        host.interrupt();
        host.terminate().await.expect("failure cleanup joined");
    }
    assert!(
        result.is_ok(),
        "sticky stop was erased by Start: {result:?}"
    );
    assert!(host.worker.lock().await.is_none());
}

#[tokio::test]
async fn native_callback_checks_exact_bytes_before_emitting() {
    let one = serde_json::json!({"type": "input_text", "text": "é"})
        .to_string()
        .len();
    let host = EmbeddedHost::spawn_with_limits(EvalLimits {
        bytes: one,
        items: 4,
        pending: 2,
    })
    .expect("host");
    host.start_cell(1, "text('é'); text('é');", HashMap::new(), vec![], 10_000)
        .expect("start");
    assert!(matches!(
        host.read_event().await,
        Ok(RuntimeEvent::Content { .. })
    ));
    assert!(
        host.read_event()
            .await
            .expect_err("second output exceeds exact limit")
            .contains("resource limit")
    );
    host.terminate().await.expect("join");
    assert!(host.worker.lock().await.is_none());
}

#[tokio::test]
async fn blocked_consumer_keeps_independent_loss_and_cancel_control() {
    let host = EmbeddedHost::spawn().expect("host");
    host.start_cell(
        1,
        "for (let i=0; i<300; i++) notify('x');",
        HashMap::new(),
        vec![],
        10_000,
    )
    .expect("start");
    // A liveness bound only: generous, so load never trips it.
    let loss = tokio::time::timeout(Duration::from_secs(30), host.wait_loss())
        .await
        .expect("control available");
    assert!(loss.contains("resource limit"));
    assert!(host.events.lock().await.len() <= 256);
    tokio::time::timeout(Duration::from_secs(30), host.terminate())
        .await
        .expect("cancel available")
        .expect("joined");
    assert!(host.worker.lock().await.is_none());
}
