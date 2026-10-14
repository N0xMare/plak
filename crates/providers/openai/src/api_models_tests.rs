use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FIXTURE: &str = include_str!("../tests/fixtures/v1_models.json");

fn live() -> TestResult<Vec<OpenAiApiModel>> {
    let listed = parse_openai_models_page(FIXTURE.as_bytes())?;
    Ok(openai_api_models(&listed)?)
}

fn find<'a>(models: &'a [OpenAiApiModel], id: &str) -> &'a OpenAiApiModel {
    models
        .iter()
        .find(|model| model.id() == id)
        .unwrap_or_else(|| panic!("{id} is listed"))
}

#[test]
fn live_fixture_classifies_profiles_text_and_non_coding_models() -> TestResult {
    let models = live()?;
    let usable: Vec<_> = models
        .iter()
        .filter(|model| model.status() == &OpenAiApiModelStatus::Usable)
        .map(OpenAiApiModel::id)
        .collect();
    assert_eq!(
        usable,
        [
            "gpt-6-luna",
            "gpt-6-sol",
            "gpt-5.6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
        ]
    );
    let astra = find(&models, "gpt-6-astra");
    let OpenAiApiModelStatus::NeedsReview(reason) = astra.status() else {
        panic!("gpt-6-astra must need review");
    };
    assert!(
        reason.contains("no reviewed OpenAI platform profile"),
        "{reason}"
    );
    for id in ["gpt-5", "gpt-4.1", "o3", "gpt-5-nano-2099-01-01"] {
        assert!(
            matches!(
                find(&models, id).status(),
                OpenAiApiModelStatus::NeedsReview(_)
            ),
            "{id}"
        );
    }
    for id in [
        "tts-1",
        "whisper-1",
        "text-embedding-3-small",
        "sora-2",
        "gpt-image-1",
        "omni-moderation-latest",
        "gpt-realtime",
    ] {
        let OpenAiApiModelStatus::NotCodingModel(reason) = find(&models, id).status() else {
            panic!("{id} must not be a coding model");
        };
        assert!(
            reason.contains("not") || reason.contains("audio"),
            "{reason}"
        );
    }
    Ok(())
}

#[test]
fn luna_window_is_the_price_threshold_and_output_is_128k() -> TestResult {
    let models = live()?;
    let luna = find(&models, "gpt-6-luna");
    assert_eq!(luna.max_input_tokens(), 922_000);
    assert_eq!(luna.price_threshold_input_tokens(), Some(272_000));
    assert_eq!(luna.max_tokens(), 128_000);
    assert_eq!(luna.profile(), Some("gpt-6-luna"));
    assert_eq!(
        luna.efforts(),
        &[
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]
    );
    assert!(
        !luna.efforts().contains(&ReasoningEffort::Ultra),
        "ultra is Codex-client-only"
    );
    let astra = find(&models, "gpt-6-astra");
    assert_eq!(astra.efforts().first(), Some(&ReasoningEffort::Low));
    assert!(!astra.efforts().contains(&ReasoningEffort::None));
    let sol = find(&models, "gpt-6.1-sol");
    assert_eq!(sol.price_threshold_input_tokens(), Some(272_000));
    assert!(!sol.efforts().contains(&ReasoningEffort::None));
    let red = find(&models, "gpt-daybreak-red-latest");
    assert_eq!(red.max_input_tokens(), 272_000);
    assert_eq!(
        red.price_threshold_input_tokens(),
        None,
        "Codex 372k is above the documented max input, so it is not the working window"
    );
    let blue = find(&models, "gpt-daybreak-blue-latest");
    assert_eq!(blue.max_input_tokens(), 922_000);
    assert_eq!(blue.price_threshold_input_tokens(), Some(272_000));
    Ok(())
}

#[test]
fn facts_digest_moves_when_a_reviewed_limit_moves() -> TestResult {
    let models = live()?;
    let luna = find(&models, "gpt-6-luna");
    let mut listed = parse_openai_models_page(FIXTURE.as_bytes())?;
    assert!(listed.iter().any(|row| row.id == "gpt-6-luna"));
    let again = openai_api_models(&listed)?;
    assert_eq!(
        find(&again, "gpt-6-luna").facts_digest(),
        luna.facts_digest()
    );
    listed.retain(|row| row.id != "gpt-6-luna");
    let without = openai_api_models(&listed)?;
    assert!(without.iter().all(|model| model.id() != "gpt-6-luna"));
    Ok(())
}

#[test]
fn malformed_list_and_duplicate_ids_fail() {
    assert!(parse_openai_models_page(b"{}").is_err());
    assert!(
        parse_openai_models_page(br#"{"data":[{"id":"gpt-6-luna"},{"id":"gpt-6-luna"}]}"#).is_err()
    );
    let skipped = parse_openai_models_page(br#"{"data":[{"id":"ok id"},{"id":"gpt-6-luna"}]}"#)
        .expect("unsafe ids are skipped");
    assert_eq!(skipped.len(), 1);
}

#[test]
fn live_page_cannot_raise_a_reviewed_window() -> TestResult {
    let body = br#"{"object":"list","data":[{"id":"gpt-6-luna","context_window":9000000,"lifecycle":"nope","max_output":1}]}"#;
    let listed = parse_openai_models_page(body)?;
    let models = openai_api_models(&listed)?;
    let luna = find(&models, "gpt-6-luna");
    assert_eq!(luna.max_input_tokens(), 922_000);
    assert_eq!(luna.price_threshold_input_tokens(), Some(272_000));
    assert_eq!(luna.max_tokens(), 128_000);
    assert_eq!(luna.status(), &OpenAiApiModelStatus::Usable);
    Ok(())
}

#[test]
fn row_bound_and_cached_id_checks_match_the_page_parser() {
    let rows = (0..=2048)
        .map(|index| format!(r#"{{"id":"m{index}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let ids = format!(r#"{{"data":[{rows}]}}"#);
    assert!(parse_openai_models_page(ids.as_bytes()).is_err());
    let duplicated = vec![
        OpenAiListedModel {
            id: "gpt-6-luna".to_owned(),
        },
        OpenAiListedModel {
            id: "gpt-6-luna".to_owned(),
        },
    ];
    assert!(accept_openai_listed_models(&duplicated).is_err());
    let skipped = accept_openai_listed_models(&[
        OpenAiListedModel {
            id: "not an id".to_owned(),
        },
        OpenAiListedModel {
            id: "gpt-6-luna".to_owned(),
        },
    ])
    .expect("unsafe ids are skipped");
    assert_eq!(skipped.len(), 1);
}

#[test]
fn bundled_listing_admits_luna_low_and_medium() -> TestResult {
    let models = openai_api_models(&openai_api_bundled_listing()?)?;
    let luna = find(&models, "gpt-6-luna");
    assert_eq!(luna.status(), &OpenAiApiModelStatus::Usable);
    assert!(luna.efforts().contains(&ReasoningEffort::Low));
    assert!(luna.efforts().contains(&ReasoningEffort::Medium));
    assert!(!bundled_table()?.revision.is_empty());
    Ok(())
}

#[test]
fn an_unknown_effort_or_hosted_feature_in_a_table_row_is_a_table_error() {
    assert!(parse_table(BUNDLED_TABLE).is_some());
    for (known, unknown) in [
        ("\"max\"\n", "\"max\",\n        \"ultra\"\n"),
        (
            "\"tool_search\"\n",
            "\"tool_search\",\n        \"file_search\"\n",
        ),
    ] {
        let table = BUNDLED_TABLE.replacen(known, unknown, 1);
        assert_ne!(table, BUNDLED_TABLE, "{unknown}");
        assert!(parse_table(&table).is_none(), "{unknown}");
    }
}
