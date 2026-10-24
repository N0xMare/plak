//! Real-worker coverage of the host session's nested-result depth guard.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "real-worker session assertions"
)]

use std::{path::Path, sync::Arc, time::Duration};

use plakous_cell::{
    BindingInput, CellClient, CellPrelude, CellResponse, CellSession, CellSessionEvent,
    EngineConfig, MONTY_WORKER, MontyBinding, MontyBootstrap, NestedDeliveryOutcome,
    ObservationMode, OutputItem,
};

#[tokio::test]
async fn nested_responder_depth_guard_rejects_only_the_call_and_preserves_the_session() {
    let client = Arc::new(
        CellClient::spawn(
            Path::new(env!("CARGO_BIN_EXE_plakous-py-host")),
            MONTY_WORKER,
            EngineConfig::MONTY,
        )
        .await
        .expect("worker"),
    );
    let (session, mut events) = CellSession::new(
        client,
        ObservationMode::HostWindow {
            request_bound: Duration::from_secs(30),
        },
    );
    let source = "try:\n    value = await fetch({})\n    depth = 0\n    while value is not None:\n        depth += 1\n        value = value['next']\n    print(f'depth:{depth}')\nexcept RuntimeError as error:\n    print(f'rejected:{error}')";
    for depth in [40, 41, 50] {
        let bootstrap = (depth == 40).then(|| MontyBootstrap {
            source: String::new(),
            bindings: vec![MontyBinding {
                function: "fetch".into(),
                tool: "fetch".into(),
                input: BindingInput::Dictionary,
            }],
        });
        let started = session
            .execute(source, CellPrelude::Monty { bootstrap }, 30_000, async {
                Ok::<(), ()>(())
            })
            .await
            .expect("start");
        let Some(CellSessionEvent::NestedCall { responder, .. }) =
            tokio::time::timeout(Duration::from_secs(10), events.receive())
                .await
                .expect("call bound")
        else {
            panic!("nested call");
        };
        let value = (0..depth).fold(
            serde_json::Value::Null,
            |value, _| serde_json::json!({"next": value}),
        );
        assert_eq!(
            responder.resolve(value).await,
            NestedDeliveryOutcome::Accepted,
            "the value or the guarded rejection reaches the interpreter"
        );
        let response = started.initial.await.expect("observation");
        let expected = if depth == 40 {
            "depth:40"
        } else {
            "rejected:nested result nests deeper than 40 levels"
        };
        assert!(
            matches!(response, CellResponse::Result { error: None, ref items, .. } if items.iter().any(|item| matches!(item, OutputItem::Text { text } if text.contains(expected)))),
            "{response:?}"
        );
    }
    let next = session
        .execute(
            "41 + 1",
            CellPrelude::Monty { bootstrap: None },
            30_000,
            async { Ok::<(), ()>(()) },
        )
        .await
        .expect("another cell")
        .initial
        .await
        .expect("result");
    assert!(
        matches!(next, CellResponse::Result { error: None, ref items, .. } if items.iter().any(|item| matches!(item, OutputItem::Text { text } if text == "42")))
    );
    assert!(!session.generation_lost());
    session.shutdown().await.expect("joined");
}
