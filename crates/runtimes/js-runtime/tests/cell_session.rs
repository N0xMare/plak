//! Real-worker coverage of guarded nested results and retained-value framing.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "real-worker session assertions"
)]

use std::{collections::HashMap, path::Path, sync::Arc};

use plakous_cell::{
    CellClient, CellPrelude, CellResponse, CellSession, CellSessionEvent, EngineConfig,
    NestedDeliveryOutcome, ObservationMode, OutputItem, QUICKJS_WORKER, StartedCell,
};

async fn session() -> (CellSession, plakous_cell::CellSessionEvents) {
    let client = Arc::new(
        CellClient::spawn(
            Path::new(env!("CARGO_BIN_EXE_plakous-js-host")),
            QUICKJS_WORKER,
            EngineConfig::QUICKJS,
        )
        .await
        .expect("worker"),
    );
    CellSession::new(client, ObservationMode::EngineParked)
}

async fn start(session: &CellSession, source: &str) -> StartedCell {
    let prelude = CellPrelude::Quickjs {
        tools: vec![
            serde_json::json!({"name": "fetch", "tool_name": "fetch", "kind": "function", "description": "fetch"}),
        ],
        stored: HashMap::new(),
    };
    session
        .execute(source, prelude, 10_000, async { Ok::<(), ()>(()) })
        .await
        .expect("start")
}

fn has_text(response: &CellResponse, expected: &str) -> bool {
    matches!(response, CellResponse::Result { error: None, items, .. } if items.iter().any(|item| matches!(item, OutputItem::Text { text } if text.contains(expected))))
}

#[tokio::test]
async fn nested_responder_depth_guard_rejects_only_the_call_and_preserves_the_session() {
    let (session, mut events) = session().await;
    let source = "try { let value = await tools.fetch({}); let depth = 0; while (value !== null) { depth++; value = value.next; } text('depth:' + depth); } catch (error) { text('rejected:' + error); }";
    for depth in [40, 41, 50] {
        let started = start(&session, source).await;
        let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
            panic!("nested call");
        };
        let value = (0..depth).fold(
            serde_json::Value::Null,
            |value, _| serde_json::json!({"next": value}),
        );
        assert_eq!(
            responder.resolve(value).await,
            NestedDeliveryOutcome::Accepted
        );
        let response = started.initial.await.expect("observation");
        let expected = if depth == 40 {
            "depth:40"
        } else {
            "nested result nests deeper than 40 levels"
        };
        assert!(has_text(&response, expected), "{response:?}");
    }
    let next = start(&session, "text('still alive')")
        .await
        .initial
        .await
        .expect("result");
    assert!(has_text(&next, "still alive"), "{next:?}");
    session.shutdown().await.expect("joined");
}

#[tokio::test]
async fn overdeep_stored_values_fail_in_cell_and_preserve_earlier_valid_writes() {
    let (session, _events) = session().await;
    for depth in [40, 41, 50, 128] {
        let source = format!(
            "let value = null; for (let n = 0; n < {depth}; n++) value = {{next: value}}; try {{ store('deep', value); text('stored'); }} catch (error) {{ text(String(error)); }}"
        );
        let response = start(&session, &source)
            .await
            .initial
            .await
            .expect("result");
        let expected = if depth == 40 {
            "stored"
        } else {
            "stored value nests deeper than 40 levels"
        };
        assert!(has_text(&response, expected), "{depth}: {response:?}");
    }
    let quoted = start(&session, r#"store('quoted', '[]{}"\\'.repeat(80)); text(load('quoted').length > 40 ? 'quoted kept' : 'bad');"#).await.initial.await.expect("quoted value");
    assert!(has_text(&quoted, "quoted kept"), "{quoted:?}");
    let source = "let value = load('deep'); let depth = 0; while (value !== null) { depth++; value = value.next; } text('retained:' + depth); store('early', 'kept'); let bad = null; for (let n = 0; n < 50; n++) bad = [bad]; store('bad', bad);";
    let failed = start(&session, source)
        .await
        .initial
        .await
        .expect("definite cell failure");
    assert!(
        matches!(failed, CellResponse::Result { error: Some(ref error), ref items, .. } if error.contains("stored value nests deeper than 40 levels") && items.iter().any(|item| matches!(item, OutputItem::Text { text } if text == "retained:40"))),
        "{failed:?}"
    );
    let next = start(
        &session,
        "text(load('early')); text(load('bad') === undefined ? 'no bad write' : 'bad write');",
    )
    .await
    .initial
    .await
    .expect("next result");
    assert!(
        has_text(&next, "kept") && has_text(&next, "no bad write"),
        "{next:?}"
    );
    session.shutdown().await.expect("joined");
}
