//! Real JavaScript worker loss before handoff and after correlated acceptance.

#![cfg(all(unix, feature = "test-hooks", debug_assertions))]
#![allow(clippy::expect_used, clippy::panic, reason = "crash-cut assertions")]

use std::{collections::HashMap, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use plakous_cell::{
    CellClient, CellPrelude, CellSession, CellSessionEvent, EngineConfig, NestedDeliveryOutcome,
    ObservationMode, QUICKJS_WORKER,
};

#[tokio::test]
async fn cell_worker_delivery_crash_cuts_preserve_only_proven_acceptance() {
    for (cut, accepted) in [("before_delivery", false), ("after_accepted", true)] {
        let directory = tempfile::tempdir().expect("directory");
        let ready = directory.path().join("ready");
        let wrapper = directory.path().join("worker");
        let worker = env!("CARGO_BIN_EXE_plakous-js-host");
        assert!(!worker.contains('\''));
        assert!(!ready.to_string_lossy().contains('\''));
        std::fs::write(&wrapper, format!("#!/bin/sh\nexport PLAK_TEST_CELL_CUT='{cut}'\nexport PLAK_TEST_CELL_CUT_READY='{}'\nexec '{worker}'\n", ready.display())).expect("wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        let client = Arc::new(
            CellClient::spawn(&wrapper, QUICKJS_WORKER, EngineConfig::QUICKJS)
                .await
                .expect("real worker"),
        );
        let (session, mut events) =
            CellSession::new(Arc::clone(&client), ObservationMode::EngineParked);
        let started = session
            .execute("text(await tools.fetch({}));", prelude(), 30_000, async {
                Ok::<(), ()>(())
            })
            .await
            .expect("start");
        let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
            panic!("one nested call");
        };
        let mut delivery = tokio::spawn(responder.resolve(serde_json::json!(7)));
        // A liveness bound only: generous, so load never trips it.
        tokio::time::timeout(Duration::from_secs(30), async {
            while !std::fs::read_to_string(&ready).is_ok_and(|marker| marker == cut) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("exact crash boundary");
        if accepted {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(30), &mut delivery)
                    .await
                    .expect("receipt before kill")
                    .expect("delivery"),
                NestedDeliveryOutcome::Accepted
            );
        } else {
            assert!(!delivery.is_finished(), "handoff has no receipt");
        }
        client.kill("crash cut").await;
        if !accepted {
            assert!(
                matches!(
                    delivery.await.expect("delivery"),
                    NestedDeliveryOutcome::Lost(_)
                ),
                "delivery remains unknown"
            );
        }
        assert!(
            started.initial.await.is_err(),
            "no successful cell terminal"
        );
        assert!(
            session.shutdown().await.is_err(),
            "loss never becomes joined"
        );
        assert!(
            session
                .execute("text(await tools.fetch({}));", prelude(), 30_000, async {
                    Ok::<(), ()>(())
                })
                .await
                .is_err(),
            "no source replay"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.receive())
                .await
                .is_err(),
            "no nested replay"
        );
    }
}

fn prelude() -> CellPrelude {
    CellPrelude::Quickjs {
        tools: vec![
            serde_json::json!({"name": "fetch", "tool_name": "fetch", "kind": "function", "description": "fetch"}),
        ],
        stored: HashMap::new(),
    }
}
