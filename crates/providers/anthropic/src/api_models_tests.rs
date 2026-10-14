use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FIXTURE: &str = include_str!("../tests/fixtures/v1_models.json");

fn live() -> TestResult<Vec<AnthropicApiModel>> {
    let page = parse_anthropic_models_page(FIXTURE.as_bytes())?;
    assert!(!page.has_more);
    Ok(anthropic_api_models(&page.models, 1_800_000_000)?)
}

fn find<'a>(models: &'a [AnthropicApiModel], id: &str) -> &'a AnthropicApiModel {
    models
        .iter()
        .find(|model| model.id() == id)
        .unwrap_or_else(|| panic!("{id} is listed"))
}

#[test]
fn live_fixture_lists_every_id_and_classifies_each_by_family_rule() -> TestResult {
    let models = live()?;
    assert_eq!(models.len(), 14);
    let usable: Vec<_> = models
        .iter()
        .filter(|model| model.status() == &AnthropicApiModelStatus::Usable)
        .map(AnthropicApiModel::id)
        .collect();
    assert_eq!(
        usable,
        [
            "claude-haiku-5-5",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-opus-4-6",
        ]
    );
    for id in [
        "claude-opus-4-5-20251101",
        "claude-haiku-4-5-20251001",
        "claude-sonnet-4-5-20250929",
    ] {
        let model = find(&models, id);
        let AnthropicApiModelStatus::NeedsReview(reason) = model.status() else {
            panic!("{id} must need review");
        };
        assert!(reason.contains("manual thinking"), "{reason}");
        assert_eq!(model.family(), None);
    }
    Ok(())
}

#[test]
fn per_model_output_limits_windows_and_thresholds_come_from_their_sources() -> TestResult {
    let models = live()?;
    let haiku = find(&models, "claude-haiku-5-5");
    assert_eq!(haiku.max_tokens(), 128_000);
    assert_eq!(haiku.max_input_tokens(), 1_000_000);
    assert_eq!(haiku.price_threshold_input_tokens(), Some(100_000));
    assert_eq!(haiku.family(), Some("claude-5-haiku"));
    let sonnet = find(&models, "claude-sonnet-5-5");
    assert_eq!(sonnet.max_input_tokens(), 1_000_000);
    assert_eq!(sonnet.price_threshold_input_tokens(), None);
    let opus45 = find(&models, "claude-opus-4-5-20251101");
    assert_eq!(opus45.max_tokens(), 64_000);
    assert_eq!(opus45.max_input_tokens(), 200_000);
    Ok(())
}

#[test]
fn effort_ladders_are_the_published_levels() -> TestResult {
    let models = live()?;
    assert_eq!(
        find(&models, "claude-opus-5-5").efforts(),
        [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]
    );
    assert_eq!(
        find(&models, "claude-sonnet-4-6").efforts(),
        [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ]
    );
    Ok(())
}

#[test]
fn hosted_feature_flags_follow_the_reviewed_docs() -> TestResult {
    let models = live()?;
    let sonnet5 = find(&models, "claude-sonnet-5").hosted_features();
    assert!(sonnet5.compaction && sonnet5.hosted_ptc && !sonnet5.tool_search);
    let haiku45 = find(&models, "claude-haiku-4-5-20251001").hosted_features();
    assert!(!haiku45.compaction && !haiku45.hosted_ptc && haiku45.code_execution);
    assert!(!find(&models, "claude-opus-5-5").hosted_features().web_fetch);
    Ok(())
}

#[test]
fn bundled_table_agrees_with_the_live_fixture() -> TestResult {
    let bundled = anthropic_api_models(&anthropic_api_bundled_listing()?, 0)?;
    let live = live()?;
    assert_eq!(bundled, live);
    Ok(())
}

#[test]
fn unlisted_family_member_needs_review_and_list_never_mints_a_threshold() -> TestResult {
    let mut listed = parse_anthropic_models_page(FIXTURE.as_bytes())?.models;
    let mut novel = listed[0].clone();
    novel.id = "claude-opus-5-6".into();
    listed.push(novel);
    let mut other = listed[0].clone();
    other.id = "claude-mythos-5-1".into();
    listed.push(other);
    let models = anthropic_api_models(&listed, 0)?;
    let AnthropicApiModelStatus::NeedsReview(reason) = find(&models, "claude-opus-5-6").status()
    else {
        panic!("an id outside the reviewed table must need review");
    };
    assert!(
        reason.contains("reviewed Anthropic model table"),
        "{reason}"
    );
    assert_eq!(
        find(&models, "claude-opus-5-6").price_threshold_input_tokens(),
        None
    );
    assert!(matches!(
        find(&models, "claude-mythos-5-1").status(),
        AnthropicApiModelStatus::NeedsReview(_)
    ));
    Ok(())
}

#[test]
fn lifecycle_fields_retire_or_warn() -> TestResult {
    let mut listed = parse_anthropic_models_page(FIXTURE.as_bytes())?.models;
    listed[0].retires_at = Some("2026-01-01T00:00:00Z".into());
    listed[1].lifecycle = Some("deprecated".into());
    listed[1].retires_at = Some("2099-01-01T00:00:00Z".into());
    let models = anthropic_api_models(&listed, 1_800_000_000)?;
    assert!(matches!(
        models[0].status(),
        AnthropicApiModelStatus::Retired(_)
    ));
    assert_eq!(models[1].status(), &AnthropicApiModelStatus::Usable);
    assert!(
        models[1]
            .deprecation()
            .is_some_and(|note| note.contains("2099"))
    );
    Ok(())
}

#[test]
fn facts_digest_binds_what_a_session_sends_and_not_records() -> TestResult {
    let mut listed = parse_anthropic_models_page(FIXTURE.as_bytes())?.models;
    let before = anthropic_api_models(&listed, 0)?[0].facts_digest();
    assert_eq!(before, anthropic_api_models(&listed, 0)?[0].facts_digest());
    // A smaller published window changes nothing a working-window session sends.
    listed[0].max_input_tokens = Some(500_000);
    assert_eq!(before, anthropic_api_models(&listed, 0)?[0].facts_digest());
    listed[0].max_tokens = Some(64_000);
    assert_ne!(before, anthropic_api_models(&listed, 0)?[0].facts_digest());
    Ok(())
}

type Mutation = fn(&mut AnthropicListedModel);

#[test]
fn live_limits_or_efforts_above_the_reviewed_row_need_review() -> TestResult {
    let base = parse_anthropic_models_page(FIXTURE.as_bytes())?.models;
    let raise: [(&str, Mutation); 3] = [
        ("claude-sonnet-5-5", |row| {
            row.max_input_tokens = Some(50_000_000);
        }),
        ("claude-sonnet-5-5", |row| {
            row.max_tokens = Some(256_000);
        }),
        // The reviewed Sonnet 4.6 ladder has no `xhigh`.
        ("claude-sonnet-4-6", |row| {
            row.efforts = Some(vec!["low".into(), "xhigh".into()]);
        }),
    ];
    for (id, raise) in raise {
        let mut listed = base.clone();
        raise(listed.iter_mut().find(|row| row.id == id).expect("listed"));
        let models = anthropic_api_models(&listed, 1_800_000_000)?;
        let AnthropicApiModelStatus::NeedsReview(reason) = find(&models, id).status() else {
            panic!("a live raise minted a usable {id}");
        };
        assert!(reason.contains("higher"), "{reason}");
    }
    // Lowering stays usable and narrows the facts.
    let mut listed = base;
    let row = listed
        .iter_mut()
        .find(|row| row.id == "claude-haiku-5-5")
        .expect("listed");
    row.max_input_tokens = Some(400_000);
    row.max_tokens = Some(64_000);
    row.efforts = Some(vec!["low".into(), "high".into()]);
    let models = anthropic_api_models(&listed, 1_800_000_000)?;
    let haiku = find(&models, "claude-haiku-5-5");
    assert_eq!(haiku.status(), &AnthropicApiModelStatus::Usable);
    assert_eq!(haiku.max_input_tokens(), 400_000);
    assert_eq!(haiku.max_tokens(), 64_000);
    assert_eq!(
        haiku.efforts(),
        [ReasoningEffort::Low, ReasoningEffort::High]
    );
    Ok(())
}

#[test]
fn malformed_lifecycle_times_fail_the_listing() {
    for field in ["retires_at", "deprecated_at"] {
        let body = format!(r#"{{"data":[{{"id":"claude-sonnet-5-5","{field}":"next tuesday"}}]}}"#);
        assert!(
            parse_anthropic_models_page(body.as_bytes()).is_err(),
            "an unparseable {field} was accepted"
        );
    }
    let mut listed = anthropic_api_bundled_listing().expect("bundled");
    listed[0].retires_at = Some("soon".into());
    assert!(anthropic_api_models(&listed, 0).is_err());
    assert!(accept_anthropic_listed_models(&listed).is_err());
}

#[test]
fn unknown_lifecycle_values_fail_closed() -> TestResult {
    let mut listed = parse_anthropic_models_page(FIXTURE.as_bytes())?.models;
    for row in &mut listed {
        row.lifecycle = Some("sunset-pending".into());
    }
    let models = anthropic_api_models(&listed, 1_800_000_000)?;
    assert!(
        models
            .iter()
            .all(|model| model.status() != &AnthropicApiModelStatus::Usable),
        "an unknown lifecycle value was admitted as usable"
    );
    Ok(())
}

#[test]
fn listed_rows_are_validated_like_a_fetched_page() {
    let bundled = anthropic_api_bundled_listing().expect("bundled");
    let mutations: [fn(&mut Vec<AnthropicListedModel>); 4] = [
        |rows| {
            rows[0].id = "claude-haiku-5-5\u{1b}[2J\tusable".into();
        },
        |rows| rows.push(rows[0].clone()),
        |rows| {
            rows[0].efforts = Some(vec!["ultra".into()]);
        },
        |rows| {
            rows[0].max_tokens = Some(0);
        },
    ];
    for mutate in mutations {
        let mut rows = bundled.clone();
        mutate(&mut rows);
        assert!(accept_anthropic_listed_models(&rows).is_err());
        assert!(anthropic_api_models(&rows, 0).is_err());
    }
    let over = (0..=MAX_MODELS)
        .map(|index| AnthropicListedModel {
            id: format!("claude-x-{index}"),
            ..bundled[0].clone()
        })
        .collect::<Vec<_>>();
    let error = accept_anthropic_listed_models(&over).expect_err("bound");
    assert!(error.to_string().contains("row bound"), "{error}");
}

#[test]
fn config_admission_refuses_an_unreviewed_pattern_match_and_a_raised_output_bound() -> TestResult {
    let config = |id: &str, output: u32| {
        crate::AnthropicPlatformModelConfig::new(
            plakous_model::ModelId::new(id).expect("id"),
            plakous_model::ModelFamilyId::new("claude-5-sonnet").expect("family"),
            output,
            plakous_model::ModelAdapterProfileId::new(
                crate::config::ANTHROPIC_API_ADAPTIVE_PROFILE_ID,
            )
            .expect("profile"),
            crate::AnthropicPlatformCapabilityProfile::AdaptiveClientTools,
        )
    };
    let now = plakous_model::Clock::now(&plakous_model::SystemClock);
    assert!(
        crate::config::preflight_platform_admission(&config("claude-sonnet-77", 128_000)?, now)
            .is_err(),
        "claude-sonnet-77 (no table row) passed the reviewed admission on its pattern"
    );
    crate::config::preflight_platform_admission(&config("claude-sonnet-5-5", 128_000)?, now)?;
    assert!(
        crate::config::preflight_platform_admission(&config("claude-sonnet-5-5", 128_001)?, now)
            .is_err()
    );
    Ok(())
}

#[test]
fn malformed_or_duplicate_pages_fail_whole() {
    for body in [
        r#"{"data":[{"id":"claude-opus-5"},{"id":"claude-opus-5"}]}"#,
        r#"{"data":[{"id":"bad id"}]}"#,
        r#"{"data":[{"id":"claude-opus-5","max_tokens":"many"}]}"#,
        r#"{"models":[]}"#,
        "not json",
    ] {
        assert!(
            parse_anthropic_models_page(body.as_bytes()).is_err(),
            "{body}"
        );
    }
    let page = parse_anthropic_models_page(
        br#"{"data":[{"id":"claude-opus-5","novel":{"x":1}}],"has_more":true,"last_id":"claude-opus-5"}"#,
    )
    .expect("unknown fields are ignored");
    assert!(page.has_more);
    assert_eq!(page.last_id.as_deref(), Some("claude-opus-5"));
}
