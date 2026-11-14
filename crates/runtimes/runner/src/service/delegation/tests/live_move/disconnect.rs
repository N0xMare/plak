use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_live_move_disconnect_after_read_still_records_occupancy_before_cas() {
    let root = tempfile::tempdir().unwrap();
    let first = Worker::new(root.path(), "first").await;
    let (_old_link, old) = first.port(root.path(), "old-host", 1).await;
    let process = old.start(command(), control(75)).await.unwrap();
    old.promote(process).await.unwrap();
    let next = Worker::new(root.path(), "second").await;
    let (client, serving) = client(&next).await;
    let line = owner(1).line().to_string();
    assert_eq!(
        client
            .owner_floors(std::slice::from_ref(&line))
            .await
            .unwrap()[0]
            .retained
            .len(),
        1
    );
    first.serving.abort();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !first.serving.is_finished() {
        assert!(Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert!(
        client
            .transfer_owner_exact(Some(&owner(1)), &owner(2))
            .await
            .is_err()
    );
    assert!(
        lock(&next.service.inner.tables)
            .owner("home", &line)
            .is_none()
    );
    let context = next.service.retention_context().unwrap();
    let entries = context.registry.entries("home", Some(&line));
    assert_eq!(entries.len(), 1, "the read-to-CAS cut forgot the original");
    assert!(!entries[0].confirmed);
    assert!(!entries[0].inventory.lost);
    assert_eq!(
        context
            .registry
            .occupancy(|_, _| true)
            .values()
            .sum::<usize>(),
        1
    );
    client.disconnect();
    serving.await.unwrap();
}
