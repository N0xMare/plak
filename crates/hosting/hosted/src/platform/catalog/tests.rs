#![allow(clippy::unwrap_used, reason = "offline seam contracts")]

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::platform::ApiModelLists;
use crate::platform::listing::{
    CachedRows, ListChoice, after_fetch, cache_path, cached_for_test, choose, read_cache,
    write_cache,
};

/// Six hours, the cached list's lifetime.
const API_MODEL_LIST_TTL_SECONDS: u64 = 6 * 60 * 60;

/// The bundled listing classifies exactly like the recorded live fixture
/// (the provider crate pins that equality).
fn live_listing() -> Vec<plakous::anthropic::AnthropicListedModel> {
    plakous::anthropic::anthropic_api_bundled_listing().unwrap()
}

fn anthropic() -> ApiModelCatalog {
    anthropic_catalog(&live_listing(), ApiCatalogSource::Live, 1_800_000_000).unwrap()
}

#[test]
fn unknown_id_is_refused_with_up_to_three_closest_listed_ids() {
    let error = anthropic()
        .admit("claude-haiku-5.5", Some(ReasoningEffort::Medium))
        .unwrap_err();
    let ApiAdmissionError::Unknown { suggestions, .. } = &error else {
        panic!("{error:?}");
    };
    assert!(
        suggestions.len() <= 3 && !suggestions.is_empty(),
        "{suggestions:?}"
    );
    assert_eq!(suggestions[0], "claude-haiku-5-5");
    let text = error.to_string();
    assert!(
        text.contains("does not list model `claude-haiku-5.5`"),
        "{text}"
    );
    assert!(text.contains("plak models anthropic"), "{text}");
    let none = anthropic()
        .admit("gpt-6-luna", Some(ReasoningEffort::Low))
        .unwrap_err();
    assert!(!none.to_string().contains("closest"), "{none}");
}

#[test]
fn listed_id_without_a_reviewed_family_is_refused_with_its_reason() {
    let error = anthropic()
        .admit("claude-opus-4-5-20251101", Some(ReasoningEffort::Low))
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("needs review"), "{text}");
    assert!(text.contains("manual thinking"), "{text}");
}

#[test]
fn output_bound_is_each_models_own_limit() {
    let catalog = anthropic();
    for (model, bound) in [
        ("claude-haiku-5-5", 128_000),
        ("claude-sonnet-5-5", 128_000),
        ("claude-opus-4-6", 128_000),
        ("claude-sonnet-4-5-20250929", 64_000),
    ] {
        assert_eq!(
            catalog.find(model).unwrap().output_bound(),
            bound,
            "{model}"
        );
    }
    for provider in [PlatformProvider::OpenAi, PlatformProvider::Xai] {
        for row in bundled_catalog(provider, "test").models {
            assert_eq!(row.output_bound(), row.max_output_tokens, "{}", row.model);
            if row.usable() {
                assert!(row.output_bound() > 32_768, "{}", row.model);
            }
        }
    }
}

#[test]
fn haiku_works_under_its_price_threshold_and_sonnet_under_its_full_window() {
    let catalog = anthropic();
    let haiku = catalog.find("claude-haiku-5-5").unwrap();
    assert_eq!(haiku.window(ApiWindow::Working), 100_000);
    assert_eq!(haiku.window(ApiWindow::Full), 1_000_000);
    let sonnet = catalog.find("claude-sonnet-5-5").unwrap();
    assert_eq!(sonnet.window(ApiWindow::Working), 1_000_000);
    assert_eq!(sonnet.window(ApiWindow::Full), 1_000_000);
}

#[test]
fn effort_ladder_is_the_published_one_and_still_explicit() {
    let catalog = anthropic();
    for effort in [
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ] {
        assert!(
            catalog.admit("claude-opus-5-5", Some(effort)).is_ok(),
            "{effort:?}"
        );
    }
    let missing = catalog.admit("claude-opus-5-5", None).unwrap_err();
    assert!(
        missing.to_string().contains("low|medium|high|xhigh|max"),
        "{missing}"
    );
    let xhigh = catalog
        .admit("claude-sonnet-4-6", Some(ReasoningEffort::XHigh))
        .unwrap_err();
    assert!(xhigh.to_string().contains("low|medium|high|max"), "{xhigh}");
    let openai = bundled_catalog(PlatformProvider::OpenAi, "test");
    for effort in [
        ReasoningEffort::None,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
    ] {
        assert!(
            openai.admit("gpt-6-luna", Some(effort)).is_ok(),
            "{effort:?}"
        );
    }
    let omitted = openai.admit("gpt-6-luna", None).unwrap_err();
    assert!(
        omitted
            .to_string()
            .contains("none|low|medium|high|xhigh|max"),
        "{omitted}"
    );
    assert!(
        openai
            .admit("gpt-6-luna", Some(ReasoningEffort::Ultra))
            .is_err()
    );
}

#[test]
fn reopen_keeps_the_stored_window_and_refuses_moved_facts_with_the_rebind_remedy() {
    let provider = PlatformProvider::Anthropic;
    let catalog = anthropic();
    let haiku = catalog.find("claude-haiku-5-5").unwrap();
    let mut stored = b"anthropic.platform.messages.v4;prefix".to_vec();
    stored.extend(haiku.binding_evidence(provider, ApiWindow::Full));
    stored.extend([0_u8, 1, 2, 255]);
    assert_eq!(haiku.reopen_window(provider, &stored), Ok(1_000_000));
    let working = haiku.binding_evidence(provider, ApiWindow::Working);
    assert_eq!(haiku.reopen_window(provider, &working), Ok(100_000));

    let mut moved = live_listing();
    moved[0].max_tokens = Some(64_000);
    let moved = anthropic_catalog(&moved, ApiCatalogSource::Live, 0).unwrap();
    let moved_haiku = moved.find("claude-haiku-5-5").unwrap();
    assert_ne!(
        moved_haiku.binding_digest(provider),
        haiku.binding_digest(provider)
    );
    let refusal = moved_haiku.reopen_window(provider, &stored).unwrap_err();
    assert!(
        refusal.contains("changed since this session was created"),
        "{refusal}"
    );
    assert!(
        refusal.ends_with("rebind with Fork now in the Agents tab, or start a new session"),
        "{refusal}"
    );
    assert!(!refusal.contains("/model"), "{refusal}");
    assert!(haiku.reopen_window(provider, b"no binding at all").is_err());
}

#[test]
fn reopen_ignores_threshold_and_feature_edits_and_refuses_a_window_the_model_lost() {
    let provider = PlatformProvider::Anthropic;
    let haiku = anthropic().find("claude-haiku-5-5").unwrap().clone();
    let stored = haiku.binding_evidence(provider, ApiWindow::Working);
    // A reviewed-table edit to the price threshold changes nothing the
    // session sends: the session keeps the window number it was created with.
    let mut edited = haiku.clone();
    edited.price_threshold = Some(150_000);
    assert_eq!(
        edited.binding_digest(provider),
        haiku.binding_digest(provider)
    );
    assert_eq!(edited.reopen_window(provider, &stored), Ok(100_000));
    // A provider that no longer serves the stored window refuses the reopen.
    let full = haiku.binding_evidence(provider, ApiWindow::Full);
    let mut shrunk = haiku.clone();
    shrunk.full_window = 500_000;
    assert_eq!(shrunk.reopen_window(provider, &stored), Ok(100_000));
    assert!(shrunk.reopen_window(provider, &full).is_err());
    // Efforts and the output bound are bound.
    let mut fewer = haiku.clone();
    fewer.efforts.pop();
    assert!(fewer.reopen_window(provider, &stored).is_err());
    let mut lower = haiku;
    lower.max_output_tokens = 64_000;
    assert!(lower.reopen_window(provider, &stored).is_err());
}

fn anthropic_rows(rows: &CachedRows) -> Option<Vec<plakous::anthropic::AnthropicListedModel>> {
    match rows {
        CachedRows::Anthropic(models) => Some(models.clone()),
        CachedRows::OpenAi(_) | CachedRows::Xai(_) => None,
    }
}

#[test]
fn cache_ttl_fallback_and_keying() {
    let provider = PlatformProvider::Anthropic;
    let now = 1_800_000_000;
    let fresh = cached_for_test(provider, "fp1", now - 60, live_listing());
    assert!(matches!(
        choose(Some(&fresh), anthropic_rows, ApiModelLists::Live, now),
        ListChoice::Use(_, ApiCatalogSource::Cached { age_seconds: 60 })
    ));
    let stale = cached_for_test(
        provider,
        "fp1",
        now - API_MODEL_LIST_TTL_SECONDS,
        live_listing(),
    );
    assert_eq!(
        choose(Some(&stale), anthropic_rows, ApiModelLists::Live, now),
        ListChoice::Fetch
    );
    assert!(matches!(
        choose(Some(&stale), anthropic_rows, ApiModelLists::Offline, now),
        ListChoice::Use(_, ApiCatalogSource::Cached { .. })
    ));
    assert_eq!(
        choose(None, anthropic_rows, ApiModelLists::Live, now),
        ListChoice::Fetch
    );
    // A cache dated in the future is never fresh: refetched when lists are
    // live, and not trusted over the bundled table offline.
    let future = cached_for_test(provider, "fp1", now + 60, live_listing());
    assert_eq!(
        choose(Some(&future), anthropic_rows, ApiModelLists::Live, now),
        ListChoice::Fetch
    );
    assert!(matches!(
        choose(Some(&future), anthropic_rows, ApiModelLists::Offline, now),
        ListChoice::Bundled(_)
    ));
    assert!(matches!(
        choose(None, anthropic_rows, ApiModelLists::Offline, now),
        ListChoice::Bundled(_)
    ));
    assert!(matches!(
        after_fetch(
            Ok(live_listing()),
            Some(live_listing()),
            API_MODEL_LIST_TTL_SECONDS
        ),
        ListChoice::Use(_, ApiCatalogSource::Live)
    ));
    assert!(matches!(
        after_fetch(
            Err("outage".into()),
            Some(live_listing()),
            API_MODEL_LIST_TTL_SECONDS
        ),
        ListChoice::Use(
            _,
            ApiCatalogSource::Cached {
                age_seconds: API_MODEL_LIST_TTL_SECONDS
            }
        )
    ));
    let ListChoice::Bundled(reason) =
        after_fetch::<plakous::anthropic::AnthropicListedModel>(Err("outage".into()), None, 0)
    else {
        panic!("no cache must fall back to the bundled table");
    };
    assert!(reason.contains("outage"), "{reason}");

    let home = tempfile::tempdir().unwrap();
    let path = cache_path(home.path(), provider, "fp1");
    assert!(path.ends_with("cache/api-models/anthropic-fp1.json"));
    write_cache(&path, &fresh).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "cache file is owner-only");
    assert!(
        !std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "tmp")),
        "the unique stage file is renamed, not left behind"
    );
    assert_eq!(read_cache(&path, provider, "fp1"), Some(fresh));
    assert_eq!(read_cache(&path, provider, "fp2"), None);
    assert_eq!(read_cache(&path, PlatformProvider::OpenAi, "fp1"), None);
}

#[test]
fn bundled_rows_keep_openai_and_xai_on_their_current_facts() {
    let openai = bundled_catalog(PlatformProvider::OpenAi, "test");
    let luna = openai.find("gpt-6-luna").unwrap();
    assert_eq!(luna.window(ApiWindow::Working), 272_000);
    assert_eq!(luna.window(ApiWindow::Full), 922_000);
    assert_eq!(luna.output_bound(), 128_000);
    assert_eq!(luna.tool_mode, ApiToolMode::CodeModeOnly);
    let xai = bundled_catalog(PlatformProvider::Xai, "test");
    let grok = xai.find("grok-4.6").unwrap();
    // Input and output share the 500,000 context: 128,000 is reserved for
    // output, so the full window is the rest and either window fits.
    assert_eq!(grok.window(ApiWindow::Working), 200_000);
    assert_eq!(grok.window(ApiWindow::Full), 372_000);
    assert_eq!(grok.output_bound(), 128_000);
    let current = xai.find("grok-4.7").unwrap();
    assert!(current.usable());
    assert_eq!(current.window(ApiWindow::Working), 200_000);
    assert_eq!(current.output_bound(), 128_000);
    let older = xai.find("grok-4.3").unwrap();
    assert_eq!(older.window(ApiWindow::Full), 872_000);
    assert_eq!(older.output_bound(), 128_000);
    assert!(xai.admit("grok-4.3", Some(ReasoningEffort::None)).is_ok());
    assert!(xai.admit("grok-4.7", Some(ReasoningEffort::XHigh)).is_ok());
    assert!(xai.admit("grok-4.7", Some(ReasoningEffort::Low)).is_ok());
    assert!(xai.admit("grok-4.7", Some(ReasoningEffort::Ultra)).is_err());
}

const OPENAI_FIXTURE: &str = include_str!("../../../tests/fixtures/openai_v1_models.json");

fn openai_listed() -> Vec<plakous::openai::OpenAiListedModel> {
    let page: serde_json::Value = serde_json::from_str(OPENAI_FIXTURE).unwrap();
    let listed = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| plakous::openai::OpenAiListedModel {
            id: row["id"].as_str().unwrap().to_owned(),
        })
        .collect::<Vec<_>>();
    plakous::openai::accept_openai_listed_models(&listed).unwrap()
}

fn openai_live() -> ApiModelCatalog {
    openai_catalog(&openai_listed(), ApiCatalogSource::Live).unwrap()
}

#[test]
fn openai_live_list_admits_only_the_reviewed_profiles() {
    let catalog = openai_live();
    let usable: Vec<_> = catalog
        .models
        .iter()
        .filter(|row| row.usable())
        .map(|row| row.model.as_str())
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
    assert!(
        catalog
            .admit("gpt-6-luna", Some(ReasoningEffort::Low))
            .is_ok()
    );
    assert!(
        catalog
            .admit("gpt-6-luna", Some(ReasoningEffort::Medium))
            .is_ok()
    );
    let review = catalog
        .admit("gpt-6-astra", Some(ReasoningEffort::Low))
        .unwrap_err();
    assert!(review.to_string().contains("needs review"), "{review}");
    let audio = catalog
        .admit("tts-1", Some(ReasoningEffort::Low))
        .unwrap_err();
    assert!(audio.to_string().contains("not a coding model"), "{audio}");
    let unknown = catalog
        .admit("gpt-6-lune", Some(ReasoningEffort::Low))
        .unwrap_err();
    let ApiAdmissionError::Unknown { suggestions, .. } = &unknown else {
        panic!("{unknown:?}");
    };
    assert_eq!(suggestions.first().map(String::as_str), Some("gpt-6-luna"));
}

#[test]
fn openai_reopen_refuses_when_the_stored_facts_no_longer_match() {
    let provider = PlatformProvider::OpenAi;
    let luna = openai_live().find("gpt-6-luna").unwrap().clone();
    let stored = luna.binding_evidence(provider, ApiWindow::Working);
    assert_eq!(luna.reopen_window(provider, &stored), Ok(272_000));
    let mut moved = stored.clone();
    for byte in &mut moved[BINDING_PREFIX..BINDING_PREFIX + 64] {
        *byte = b'a';
    }
    let refusal = luna.reopen_window(provider, &moved).unwrap_err();
    assert!(
        refusal.contains("changed since this session was created"),
        "{refusal}"
    );
    assert!(refusal.contains("rebind"), "{refusal}");
}

/// Byte offset of the digest inside [`PlatformModel::binding_evidence`].
const BINDING_PREFIX: usize = ";plak.api-model-binding.v2=".len();
