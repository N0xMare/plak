use super::*;
use plakous::runner::{
    PlacementFailure, ProbePhase, RunnerServices, RunnerStatus, WorkspaceDiscovery,
};
use std::time::Duration;

#[tokio::test]
async fn ready_fence_rejects_loss_after_the_last_cleanup_answer() {
    publication_cut(false).await;
}

#[tokio::test]
async fn ready_fence_rejects_a_rebind_after_reconciliation() {
    publication_cut(true).await;
}

async fn publication_cut(rebind_before_publication: bool) {
    let mut fixture = stopped_runtime().await;
    fixture.session.take().unwrap().release().await.unwrap();
    let workspace = fixture.services.paths().unwrap().workspace().unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let (client, descriptor) = line_runner(workspace, fixture.line, stops.clone()).await;
    let admitted = Arc::new(AtomicUsize::new(0));
    let runner = admitted_echo_services(descriptor, admitted.clone());
    let registry = fixture.services.runners();
    registry.configure(ConfiguredRunner {
        runner_id: "review".into(),
        names: vec!["review".into()],
    });
    registry.require_connection("review", true);
    registry.run_probes_on(tokio::runtime::Handle::current());
    let retries: Retries = Arc::default();
    let passes = Arc::new(AtomicUsize::new(0));
    registry.probe_with(cut_probe(
        &fixture,
        client.clone(),
        runner.clone(),
        retries.clone(),
        passes.clone(),
        stops,
        rebind_before_publication,
    ));
    let rows = registry.refresh(None).await.unwrap();
    assert_eq!(
        rows[0].status,
        RunnerStatus::Blocked(PlacementFailure::RunnerUnreachable {
            phase: ProbePhase::LinkLost,
            timed_out: false,
        })
    );
    assert!(!runner.is_available());
    assert!(registry.services("review").is_err());
    assert_effect_blocked(&runner, &admitted).await;
    assert_eq!(client.is_connected(), rebind_before_publication);
    assert_eq!(passes.load(Ordering::SeqCst), 1);
    for _ in 0..3 {
        retry_remote(registry.clone(), "review".into(), retries.clone());
    }
    assert!(retries.lock().unwrap()["review"].scheduled);
    assert_eq!(retries.lock().unwrap()["review"].delay, 1);
    let mut changes = registry.changes();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.rows()[0].status != RunnerStatus::Ready {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(passes.load(Ordering::SeqCst), 2, "one coalesced redial");
    assert!(client.is_connected());
    assert!(runner.is_available());
    assert!(!retries.lock().unwrap()["review"].scheduled);
    runner.backend().link().call(effect()).await.unwrap();
    assert_eq!(admitted.load(Ordering::SeqCst), 1);
    client.disconnect();
}

fn cut_probe(
    fixture: &StoppedLine,
    client: RunnerClient,
    runner: RunnerServices,
    retries: Retries,
    passes: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    rebind_before_publication: bool,
) -> Arc<plakous::runner::RunnerProbe> {
    let context = fixture.services.runner_probe_context();
    let catalog = fixture.catalog.clone();
    let line = fixture.line;
    Arc::new(move |registry, id| {
        let services = context.with_runner_registry(registry.clone());
        let (client, runner, retries, passes, stops, catalog) = (
            client.clone(),
            runner.clone(),
            retries.clone(),
            passes.clone(),
            stops.clone(),
            catalog.clone(),
        );
        Box::pin(async move {
            let pass = passes.fetch_add(1, Ordering::SeqCst);
            if pass > 0 {
                tokio::time::resume();
                rebind(&services, &client, line, stops.clone()).await;
            }
            let connection = client.connection_revision();
            let pending = reconcile_remote(&services, &id, &client).await?;
            assert_eq!(pending, [] as [LineId; 0]);
            assert!(
                stops.load(Ordering::SeqCst) > 0,
                "cleanup must answer before the cut"
            );
            if pass == 0 {
                // Reconciliation uses real SQLite I/O; virtualize only the
                // retry delay, not the barrier's I/O and lease timers.
                tokio::time::pause();
                // First-bind loss must be observed before the watcher exists,
                // even if another link has already connected by publication.
                client.disconnect();
                if rebind_before_publication {
                    rebind(&services, &client, line, stops).await;
                }
            }
            let result = registry.ready(
                &id,
                connection,
                runner,
                catalog.options(),
                WorkspaceDiscovery::default(),
                1,
            );
            match result {
                Ok(()) => complete_reconciliation(&services, &id, &client, pending, &retries),
                Err(_) => retry_remote(registry, id, retries),
            }
            result
        })
    })
}

async fn rebind(services: &Services, client: &RunnerClient, line: LineId, stops: Arc<AtomicUsize>) {
    let (stream, _) = line_stream(services.paths().unwrap().workspace().unwrap(), line, stops);
    client.reconnect(stream).await.unwrap();
    client.lines().await.unwrap();
    assert!(client.is_connected());
}

fn effect() -> plakous::tools::runner::RunnerCall {
    plakous::tools::runner::RunnerCall::unkeyed(
        plakous::tools::runner::RunnerVerb::VcsEffect,
        serde_json::Value::Null,
    )
}

async fn assert_effect_blocked(runner: &RunnerServices, admitted: &AtomicUsize) {
    assert!(matches!(
        runner.backend().link().call(effect()).await,
        Err(plakous::tools::runner::RunnerFailure::Unavailable(_))
    ));
    assert_eq!(admitted.load(Ordering::SeqCst), 0);
}
