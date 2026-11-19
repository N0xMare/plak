//! Offline cell-protocol adaptations of micro-monty's MIT code-mode tasks:
//! `par-weather-conditions`, `retry-permanent-errors`, and `page-stop-early`.

use std::{path::Path, sync::Arc, time::Duration};

use plakous_cell::{
    BindingInput, CellClient, CellPrelude, CellResponse, CellSession, CellSessionEvent,
    EngineConfig, MONTY_WORKER, MontyBinding, MontyBootstrap, NestedDeliveryOutcome,
    ObservationMode, OutputItem,
};
use serde_json::json;

fn bootstrap() -> MontyBootstrap {
    MontyBootstrap {
        source: "\
async def weather(*args, **kwargs): return await _weather(*args, **kwargs)\n\
async def page(*args, **kwargs): return await _page(*args, **kwargs)\n\
async def read(*args, **kwargs): return await _read(*args, **kwargs)\n"
            .into(),
        bindings: ["weather", "page", "read"]
            .into_iter()
            .map(|name| MontyBinding {
                function: format!("_{name}"),
                tool: name.into(),
                input: BindingInput::Dictionary,
            })
            .collect(),
    }
}

#[tokio::test]
async fn codemode_gather_retry_pagination_and_text() -> Result<(), Box<dyn std::error::Error>> {
    let client = Arc::new(
        CellClient::spawn(
            Path::new(env!("CARGO_BIN_EXE_plakous-py-host")),
            MONTY_WORKER,
            EngineConfig::MONTY,
        )
        .await?,
    );
    let (session, mut events) = CellSession::new(
        client,
        ObservationMode::HostWindow {
            request_bound: Duration::from_secs(10),
        },
    );
    let source = r#"async def lookup(city):
    for attempt in range(2):
        try:
            return (await weather(city=city))['conditions']
        except ToolError as error:
            if str(error) != 'retry':
                return str(error)
    raise AssertionError('retry exhausted')

results = await asyncio.gather(lookup('A'), lookup('B'), lookup('C'))
assert results == ['sun', 'rain', 'not found'], results

cursor = None
selected = []
while len(selected) < 2:
    result = await page({'cursor': cursor})
    selected.extend(item['name'] for item in result['items'] if item['active'])
    cursor = result['next']

document = await read(path='notes.txt')
assert isinstance(document, str) and document == '{"not": "decoded"}'
selected
"#;
    let started = session
        .execute(
            source,
            CellPrelude::Monty {
                bootstrap: Some(bootstrap()),
            },
            30_000,
            async { Ok::<(), ()>(()) },
        )
        .await
        .map_err(|error| format!("{error:?}"))?;

    tokio::time::timeout(Duration::from_secs(10), async {
        // No answer is released until all three requests are admitted.
        let mut weather = Vec::new();
        for _ in 0..3 {
            let Some(CellSessionEvent::NestedCall { name, input, responder, .. }) = events.receive().await else {
                return Err("missing parallel call");
            };
            assert_eq!(name, "weather");
            let city = input["city"].as_str().ok_or("missing city")?.to_owned();
            weather.push((city, responder));
        }
        weather.sort_unstable_by(|left, right| right.0.cmp(&left.0));
        assert_eq!(weather.iter().map(|(city, _)| city.as_str()).collect::<Vec<_>>(), ["C", "B", "A"]);
        let mut batch = tokio::task::JoinSet::new();
        for (city, responder) in weather {
            batch.spawn(async move {
                match city.as_str() {
                    "A" => responder.resolve(json!({"conditions": "sun"})).await,
                    "B" => responder.reject("retry".into()).await,
                    _ => responder.reject("not found".into()).await,
                }
            });
        }
        assert!(batch.join_all().await.into_iter().all(|verdict| verdict == NestedDeliveryOutcome::Accepted));

        for (tool, args, value) in [
            ("weather", json!({"city":"B"}), json!({"conditions":"rain"})),
            ("page", json!({"cursor":null}), json!({"items":[{"name":"one", "active":true}], "next":"second"})),
            ("page", json!({"cursor":"second"}), json!({"items":[{"name":"ignore", "active":false},{"name":"two", "active":true}], "next":"must not fetch"})),
            ("read", json!({"path":"notes.txt"}), json!("{\"not\": \"decoded\"}")),
        ] {
            let Some(CellSessionEvent::NestedCall { name, input, responder, .. }) = events.receive().await else {
                return Err("missing sequential call");
            };
            assert_eq!((name.as_str(), input), (tool, args));
            assert_eq!(responder.resolve(value).await, NestedDeliveryOutcome::Accepted);
        }
        Ok::<(), &str>(())
    }).await??;
    let response = started
        .initial
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(
        matches!(response, CellResponse::Result { error: None, items, .. } if items.contains(&OutputItem::Text { text: "['one', 'two']".into() }))
    );
    session.shutdown().await?;
    Ok(())
}
