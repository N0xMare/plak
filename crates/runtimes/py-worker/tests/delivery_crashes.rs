//! Real-worker cuts around result consumption and its acceptance receipt.

#![cfg(all(unix, feature = "test-hooks"))]
#![allow(clippy::expect_used, clippy::panic, reason = "crash-cut assertions")]

use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use plakous_cell::{
    BindingInput, CellClient, CellPrelude, CellSession, CellSessionEvent, EngineConfig,
    MONTY_WORKER, MontyBinding, MontyBootstrap, NestedDeliveryOutcome, ObservationMode,
};

#[tokio::test]
async fn cell_worker_delivery_crash_cuts_preserve_only_proven_acceptance() {
    for (cut, accepted) in [("before_delivery", false), ("after_accepted", true)] {
        let directory = tempfile::tempdir().expect("directory");
        let ready = directory.path().join("ready");
        let wrapper = directory.path().join("worker");
        let worker = env!("CARGO_BIN_EXE_plakous-py-host");
        assert!(!worker.contains('\''));
        assert!(!ready.to_string_lossy().contains('\''));
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexport PLAK_TEST_CELL_CUT='{cut}'\nexport PLAK_TEST_CELL_CUT_READY='{}'\nexec '{worker}'\n",
                ready.display()
            ),
        )
        .expect("wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        let client = Arc::new(
            CellClient::spawn(&wrapper, MONTY_WORKER, EngineConfig::MONTY)
                .await
                .expect("real worker"),
        );
        let (session, mut events) = CellSession::new(
            Arc::clone(&client),
            ObservationMode::HostWindow {
                request_bound: Duration::from_secs(30),
            },
        );
        let bootstrap = MontyBootstrap {
            source: String::new(),
            bindings: vec![MontyBinding {
                function: "fetch".into(),
                tool: "fetch".into(),
                input: BindingInput::Dictionary,
            }],
        };
        let started = session
            .execute(
                "await fetch({})",
                CellPrelude::Monty {
                    bootstrap: Some(bootstrap),
                },
                30_000,
                async { Ok::<(), ()>(()) },
            )
            .await
            .expect("start");
        let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
            panic!("one nested call");
        };
        let mut delivery = tokio::spawn(responder.resolve(serde_json::json!(7)));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready.is_file() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker reached the exact boundary");
        assert_eq!(std::fs::read_to_string(&ready).expect("cut"), cut);
        if accepted {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), &mut delivery)
                    .await
                    .expect("receipt arrives before kill")
                    .expect("delivery task"),
                NestedDeliveryOutcome::Accepted
            );
        } else {
            assert!(!delivery.is_finished(), "no receipt before handoff");
        }
        client.kill("crash cut").await;
        if !accepted {
            let outcome = delivery.await.expect("delivery task");
            assert!(matches!(outcome, NestedDeliveryOutcome::Lost(_)));
        }
        assert!(started.initial.await.is_err(), "no terminal was published");
        assert!(
            session.shutdown().await.is_err(),
            "loss cannot become joined"
        );
        // Reusing this session cannot replay either the source or nested call.
        assert!(
            session
                .execute(
                    "await fetch({})",
                    CellPrelude::Monty { bootstrap: None },
                    30_000,
                    async { Ok::<(), ()>(()) }
                )
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.receive())
                .await
                .is_err()
        );
    }
}
