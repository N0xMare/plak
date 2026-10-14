use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/v1_models.json");

fn listed() -> Vec<XaiListedModel> {
    parse_xai_models_page(FIXTURE.as_bytes()).unwrap()
}

fn models() -> Vec<XaiApiModel> {
    xai_api_models(&listed()).unwrap()
}

fn find<'a>(models: &'a [XaiApiModel], id: &str) -> &'a XaiApiModel {
    models.iter().find(|model| model.id() == id).unwrap()
}

#[test]
fn the_fixture_classifies_the_reviewed_family() {
    let models = models();
    let usable: Vec<_> = models
        .iter()
        .filter(|model| matches!(model.status(), XaiApiModelStatus::Usable))
        .map(XaiApiModel::id)
        .collect();
    assert_eq!(usable, ["grok-4.3", "grok-4.5", "grok-4.6", "grok-4.7"]);
    let grok = find(&models, "grok-4.7");
    assert_eq!(
        grok.efforts(),
        &[
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ]
    );
    assert!(xai_platform_effort_admitted(
        "grok-4.7",
        ReasoningEffort::Low
    ));
    assert!(!xai_platform_effort_admitted(
        "grok-4.7",
        ReasoningEffort::None
    ));
    assert_eq!(grok.context_length(), 500_000);
    assert_eq!(grok.price_threshold_input_tokens(), Some(200_000));
    assert_eq!(grok.working_window(), 200_000);
    // No published output limit: 128,000 is reserved out of the shared
    // context, so either window plus the output bound fits in it.
    assert_eq!(grok.max_tokens(), 128_000);
    assert_eq!(grok.max_input_tokens(), 372_000);
    let older = find(&models, "grok-4.3");
    assert_eq!(older.context_length(), 1_000_000);
    assert_eq!(older.working_window(), 200_000);
    assert_eq!(older.max_tokens(), 128_000);
    assert_eq!(older.max_input_tokens(), 872_000);
    assert!(older.efforts().contains(&ReasoningEffort::None));
    assert!(matches!(
        find(&models, "grok-4.20-0309-non-reasoning").status(),
        XaiApiModelStatus::NeedsReview(_)
    ));
    assert!(matches!(
        find(&models, "grok-4.20-multi-agent-0309").status(),
        XaiApiModelStatus::NeedsReview(_)
    ));
    assert!(matches!(
        find(&models, "grok-build-0.1").status(),
        XaiApiModelStatus::NeedsReview(reason) if reason.contains("no reasoning effort")
    ));
    assert!(matches!(
        find(&models, "grok-imagine-1").status(),
        XaiApiModelStatus::NotCodingModel(_)
    ));
    assert_eq!(
        bundled_table().unwrap().revision,
        "xai.api-models.2026-10-09.v1"
    );
}

#[test]
fn the_output_reserve_fits_the_shared_context_under_either_window() {
    for model in models() {
        if model.status() != &XaiApiModelStatus::Usable {
            continue;
        }
        let output = u64::from(model.max_tokens());
        assert!(model.working_window() + output <= model.context_length());
        assert_eq!(model.max_input_tokens() + output, model.context_length());
        assert!(output <= model.context_length() / 2);
    }
    // A small context reserves half of itself, never more.
    assert_eq!(output_reserve(100_000), 50_000);
    assert!(xai_platform_capacity_admitted("grok-4.7", 372_000, 128_000));
    assert!(!xai_platform_capacity_admitted(
        "grok-4.7", 372_001, 128_000
    ));
    assert!(!xai_platform_capacity_admitted("grok-4.7", 1, 128_001));
    assert!(!xai_platform_capacity_admitted("grok-build-0.1", 1, 1));
}

#[test]
fn an_unknown_effort_in_a_table_row_is_a_table_error() {
    let table = BUNDLED_TABLE.replacen(r#""xhigh"]"#, r#""xhigh", "ultra"]"#, 1);
    assert_ne!(table, BUNDLED_TABLE);
    assert!(parse_table(&table).is_err());
    assert!(parse_table(BUNDLED_TABLE).is_ok());
}

/// A live row below the reviewed one narrows it: the smaller context and
/// threshold, and only the live efforts the table also reviews.
#[test]
fn live_facts_below_the_reviewed_row_narrow_it() {
    let listed = vec![XaiListedModel {
        id: "grok-4.7".to_owned(),
        created: 1_788_307_200,
        context_length: Some(256_000),
        long_context_threshold: Some(150_000),
        efforts: vec!["low".to_owned(), "medium".to_owned()],
    }];
    let model = xai_api_models(&listed).unwrap().remove(0);
    assert_eq!(model.status(), &XaiApiModelStatus::Usable);
    assert_eq!(model.context_length(), 256_000);
    assert_eq!(model.price_threshold_input_tokens(), Some(150_000));
    assert_eq!(
        model.efforts(),
        &[ReasoningEffort::Low, ReasoningEffort::Medium]
    );
    assert_eq!(model.max_tokens(), 128_000);
    assert_eq!(model.max_input_tokens(), 128_000);
    assert_eq!(model.working_window(), 128_000);
    let unnarrowed = xai_api_models(&xai_api_bundled_listing().unwrap()).unwrap();
    assert_ne!(
        model.facts_digest(),
        find(&unnarrowed, "grok-4.7").facts_digest(),
        "a narrowed ladder binds differently"
    );
    // A live row that publishes no efforts keeps the reviewed ladder.
    let mut silent = listed[0].clone();
    silent.efforts.clear();
    let model = xai_api_models(&[silent]).unwrap().remove(0);
    assert_eq!(model.efforts().len(), 4);
}

#[test]
fn a_live_window_or_effort_above_the_table_needs_review() {
    let mut rows = listed();
    {
        let grok = rows.iter_mut().find(|row| row.id == "grok-4.7").unwrap();
        grok.context_length = Some(2_000_000);
    }
    let classified = xai_api_models(&rows).unwrap();
    assert!(matches!(
        find(&classified, "grok-4.7").status(),
        XaiApiModelStatus::NeedsReview(reason) if reason.contains("higher")
    ));
    assert_eq!(find(&classified, "grok-4.7").context_length(), 500_000);
    assert_ne!(find(&classified, "grok-4.7").context_length(), 2_000_000);

    {
        let grok = rows.iter_mut().find(|row| row.id == "grok-4.7").unwrap();
        grok.context_length = Some(500_000);
        grok.efforts.push("max".to_owned());
    }
    let classified = xai_api_models(&rows).unwrap();
    assert!(matches!(
        find(&classified, "grok-4.7").status(),
        XaiApiModelStatus::NeedsReview(_)
    ));
}

#[test]
fn a_lower_live_window_narrows_the_reviewed_facts() {
    let mut rows = listed();
    let grok = rows.iter_mut().find(|row| row.id == "grok-4.6").unwrap();
    grok.context_length = Some(100_000);
    let classified = xai_api_models(&rows).unwrap();
    let row = find(&classified, "grok-4.6");
    assert!(matches!(row.status(), XaiApiModelStatus::Usable));
    assert_eq!(row.context_length(), 100_000);
    assert_eq!(row.max_tokens(), 50_000);
    assert_eq!(row.working_window(), 50_000);
}

#[test]
fn facts_digest_moves_when_a_bound_fact_moves() {
    let before = find(&models(), "grok-4.7").facts_digest();
    assert_eq!(before, find(&models(), "grok-4.7").facts_digest());
    let mut rows = xai_api_bundled_listing().unwrap();
    // The bundled listing is the reviewed facts. A different id digests apart.
    assert_ne!(before, find(&models(), "grok-4.6").facts_digest());
    rows.retain(|row| row.id == "grok-4.7");
    assert_eq!(before, xai_api_models(&rows).unwrap()[0].facts_digest());
}

#[test]
fn unknown_lifecycle_bad_timestamp_cursor_and_duplicates_fail_closed() {
    let err =
        parse_xai_models_page(br#"{"data":[{"id":"grok-4.7","created":1,"lifecycle":"preview"}]}"#);
    assert!(err.unwrap_err().to_string().contains("lifecycle"));
    let err = parse_xai_models_page(br#"{"data":[{"id":"grok-4.7","created":"yesterday"}]}"#);
    assert!(err.unwrap_err().to_string().contains("timestamp"));
    let err = parse_xai_models_page(br#"{"has_more":true,"data":[{"id":"grok-4.7","created":1}]}"#);
    assert!(err.unwrap_err().to_string().contains("another page"));
    let err = parse_xai_models_page(
        br#"{"data":[{"id":"grok-4.7","created":1},{"id":"grok-4.7","created":1}]}"#,
    );
    assert!(err.unwrap_err().to_string().contains("repeats"));
}

#[test]
fn the_envelope_unit_stays_wire_bytes_while_the_window_is_tokens() {
    // The catalog window is a token count. Construction installs it as the
    // caller ceiling on `responses-wire-bytes-xai-platform-v1` without
    // converting. The follow-up corrects OpenAI, Anthropic and xAI together.
    assert_eq!(
        crate::platform_binding::ACCOUNTING,
        "responses-wire-bytes-xai-platform-v1"
    );
    assert_eq!(find(&models(), "grok-4.7").working_window(), 200_000);
}
