#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostConfig = crate::runtime::HostConfig<TestExtension>;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

#[test]
fn seeded_save_failure_retains_the_committed_successor_and_its_locator() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    let id = config.session.id.clone();
    let database = config.product_home.join("runtime.sqlite3");
    let mut competing = config.session.clone();
    let store = config.store.clone();
    config.seed_history = Some(plakous::sessions::PortableSessionHistory::new(
        Vec::new(),
        "no-replay".into(),
        plakous::model::ModelId::new("offline-loopback-v1").unwrap(),
    ));
    let created = create_seeded_session_with_preflight(config, move |_| {
        // Advance only the product revision, so canonical creation succeeds
        // and its final product save deterministically refuses stale CAS.
        store.save(&mut competing).map_err(CliFailure::new)
    })
    .unwrap();
    assert_eq!(created.session.id, id);
    assert!(created.completion_error.is_some());
    let locator = created.session.plakous_locator.unwrap();
    plakous::sessions::AgentSessions::export_history_sqlite(&database, locator).unwrap();
    let (record, notice) = created.into_session();
    assert_eq!(record.id, id);
    assert_eq!(record.plakous_locator, Some(locator));
    assert!(notice.contains(&id), "{notice}");
    assert!(
        notice.contains("committed successor, not its source"),
        "{notice}"
    );
    assert!(
        record.transcript.pending_notices().iter().any(|row| {
            matches!(row, TranscriptRow::Notice(notice) if notice.text.contains(&id))
        })
    );
}

#[test]
fn replay_fallback_authors_one_persistable_notice_only_when_needed() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    record_replay_fallback(&config.home, &mut config.session, None);
    assert_eq!(config.session.transcript.pending_notices().len(), 0);
    let error = AgentError::new(
        AgentErrorKind::InvalidInput,
        "opaque replay target binding mismatch: selected configuration",
    );
    record_replay_fallback(&config.home, &mut config.session, Some(&error));
    let logged = session_log::read_tail_for_kind(
        &config.home,
        &config.session.id,
        4,
        Some("session.replay_stripped"),
    )
    .unwrap();
    assert_eq!(logged.len(), 1);
    assert!(logged[0].contains("selected configuration"));
    let notices = config.session.transcript.pending_notices();
    assert_eq!(notices.len(), 1);
    let TranscriptRow::Notice(notice) = &notices[0] else {
        panic!("not a notice");
    };
    assert!(notice.text.contains("conversation was carried"));
    assert!(!notice.text.contains("selected configuration"));
    assert!(!notice.text.contains('\n'));
    config.store.save(&mut config.session).unwrap();
    let loaded = config.store.load(&config.session.id).unwrap();
    assert_eq!(loaded.transcript.iter().filter(|row|matches!(row,TranscriptRow::Notice(notice) if notice.text.contains("Provider replay was incompatible"))).count(),1);
}

use super::*;
use crate::runtime::*;

use crate::runtime::clear::*;

use crate::runtime::shell::*;

use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, TargetSelection, WireBinding};
use crate::session_log::{self};
use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
use plakous::agent::ReasoningEffort;
use plakous::application::ApplicationExecutor;
use plakous::subscriptions::NativeProvider;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

fn root_template(tools: &[&str]) -> crate::binding::RootTemplateFact {
    crate::binding::RootTemplateFact {
        name: "root-agent".to_owned(),
        account: "codex".to_owned(),
        model: "offline-loopback-v1".to_owned(),
        tools: crate::tool_plan::TemplateToolSelection::parse(tools).unwrap(),
        tool_sections: std::collections::BTreeMap::from([(
            "tools.fetch_url".to_owned(),
            format!("[tools.fetch_url]\nmax_bytes = {}\n", tools.len()),
        )]),
    }
}

/// A wildcard's unlowerable tool is omitted at creation with one
/// Settings notice in the transcript, and the listed tools are frozen
/// into the record for every later bind.
#[test]
fn creation_freezes_listed_mcp_tools_and_records_the_omission_notice() {
    let executable = std::env::current_exe().unwrap();
    let server = executable
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap()
        .join("mcp-fake-server");
    assert!(
        server.is_file(),
        "run the workspace tests so the fixture is built"
    );
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    let mut template = root_template(&["mcp:fake"]);
    template.tool_sections = std::collections::BTreeMap::from([(
        "mcp_servers.fake".to_owned(),
        format!(
            "[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {:?}\n\
                 args = [\"--tools\", \"unlowerable\"]\nenv = {{ }}\nsecret_env = {{ }}\n\
                 startup_seconds = 20\ncall_seconds = 120\n",
            server.to_str().unwrap()
        ),
    )]);
    config.root_template = Some(template);
    freeze_root_tools(&mut config).unwrap();

    let notices = config.session.transcript.pending_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let TranscriptRow::Notice(notice) = &notices[0] else {
        panic!("not a notice: {notices:?}");
    };
    assert_eq!(notice.scope, NoticeScope::Settings);
    assert!(notice.text.contains("fake/tangled"), "{}", notice.text);
    let frozen = root_tool_selection(&config.session).mcp_tools.unwrap();
    let names = frozen
        .iter()
        .map(|record| record.mcp_name().to_owned())
        .collect::<Vec<_>>();
    assert!(names.contains(&"echo".to_owned()), "{names:?}");
    assert!(!names.contains(&"tangled".to_owned()), "{names:?}");
}

/// The root's tools are frozen at creation: editing the `[root] agent`
/// template afterwards changes neither the reopened record nor its
/// rebinds, and a new session takes the edit.
#[test]
fn root_tools_are_frozen_at_creation_and_an_edit_changes_new_sessions_only() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    config.root_template = Some(root_template(&["web_search"]));
    let opened = executor
        .block_on(build_session_from_config(&mut config, &mut Vec::new()))
        .unwrap()
        .unwrap();
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    executor.block_on(opened.release()).unwrap().unwrap();

    config.root_template = Some(root_template(&["fetch_url", "image_gen"]));
    let reopened = config.store.load(&config.session.id).unwrap();
    let frozen = root_tool_selection(&reopened);
    assert_eq!(
        frozen.selection,
        crate::tool_plan::TemplateToolSelection::parse(["web_search"]).unwrap()
    );
    assert_eq!(
        frozen.tool_sections["tools.fetch_url"],
        "[tools.fetch_url]\nmax_bytes = 1\n"
    );
    assert_eq!(reopened.frozen_tools.as_ref().unwrap().selection(), frozen);

    let mut fresh = SessionRecord::new(config.session.workspace_root.clone());
    fresh.account = config.session.account.clone();
    fresh.model = config.session.model.clone();
    assert!(root_tool_selection(&fresh).selection.is_empty());
    config.session = fresh;
    freeze_root_tools(&mut config).unwrap();
    assert_eq!(
        root_tool_selection(&config.session).selection,
        crate::tool_plan::TemplateToolSelection::parse(["fetch_url", "image_gen"]).unwrap()
    );

    let mut off_tuple = SessionRecord::new(config.session.workspace_root.clone());
    off_tuple.account = config.session.account.clone();
    off_tuple.model = Some("other".into());
    config.session = off_tuple;
    freeze_root_tools(&mut config).unwrap();
    assert_eq!(
        config.session.frozen_tools,
        Some(crate::frozen_tools::FrozenRootTools::new(None)),
        "a session created off the template's tuple has no tools for life"
    );
}

#[test]
fn exact_native_refresh_preserves_semantics_and_account_with_monotonic_generation() {
    use plakous::model::DigestDomain;
    fn digest(domain: DigestDomain, value: &'static [u8]) -> Digest {
        Digest::sha256(domain, 5, &[value]).unwrap()
    }
    fn selected(value: &'static [u8]) -> SelectedProviderConfigurationId {
        serde_json::from_value(
            serde_json::to_value(digest(DigestDomain::SelectedProviderConfiguration, value))
                .unwrap(),
        )
        .unwrap()
    }
    fn effective(value: &'static [u8]) -> EffectiveProviderConfigurationId {
        serde_json::from_value(
            serde_json::to_value(digest(DigestDomain::EffectiveProviderConfiguration, value))
                .unwrap(),
        )
        .unwrap()
    }

    let old = FreshNativeBinding {
        selected: selected(b"selected"),
        effective: effective(b"effective"),
        account: digest(DigestDomain::ModelBinding, b"account"),
        credential_authority: digest(DigestDomain::ModelBinding, b"authority"),
        selection: digest(DigestDomain::ModelBinding, b"selection epoch"),
        generation: 7,
    };
    let mut fresh = FreshNativeBinding {
        generation: 8,
        ..old
    };
    assert!(old.admits(&fresh));
    fresh.generation = 6;
    assert!(!old.admits(&fresh), "generation rollback");
    fresh.generation = 8;
    fresh.account = digest(DigestDomain::ModelBinding, b"foreign account");
    assert!(!old.admits(&fresh));
    fresh.account = old.account;
    fresh.credential_authority = digest(DigestDomain::ModelBinding, b"foreign slot");
    assert!(!old.admits(&fresh));
    fresh.credential_authority = old.credential_authority;
    fresh.selection = digest(DigestDomain::ModelBinding, b"relogin epoch");
    assert!(!old.admits(&fresh));
    assert!(
        old.refusal(&fresh, "Codex", "codex")
            .message()
            .contains("login selection changed")
    );
    assert!(
        !old.refusal(&fresh, "Codex", "codex")
            .message()
            .contains("plak login")
    );
    fresh.selection = old.selection;
    fresh.effective = effective(b"changed model semantics");
    assert!(!old.admits(&fresh));
    assert!(
        old.refusal(&fresh, "Claude", "claude")
            .message()
            .contains("original account and configuration")
    );
    fresh.effective = old.effective;
    fresh.selected = selected(b"changed selection");
    assert!(!old.admits(&fresh));
}

#[test]
fn unavailable_native_authority_names_its_login_remedy() {
    let failure = native_authority_unavailable("Codex", "codex");
    assert!(failure.message().contains("run `plak login codex`"));
    assert!(failure.message().contains("earlier build"));
}

#[test]
fn changed_native_account_names_both_exact_restore_and_history_carry() {
    let failure = native_account_changed("Claude", "claude");
    assert!(
        failure
            .message()
            .contains("original account with `plak login claude`")
    );
    assert!(failure.message().contains("/rebind"));
    let guidance = attach_repair_guidance(
        "provider account changed; start a new session for the newly logged-in account",
    );
    assert!(guidance.contains("original account"));
    assert!(guidance.contains("/rebind"));
    let guidance = attach_repair_guidance("login selection changed");
    assert!(guidance.contains("login-selection fence"));
    assert!(guidance.contains("cannot restore the old selection"));
}

#[test]
fn claude_refresh_rejection_and_attach_guidance_name_distinct_repairs() {
    let failure = native_credential_message("Claude", "claude", true);
    assert_eq!(
        failure.message(),
        "Claude credential was rejected at refresh; run `plak login claude`"
    );
    assert!(attach_repair_guidance(failure.message()).contains("same-account Claude"));
    assert!(
        attach_repair_guidance("resolved harness plan is bound to a different model capability binding (adapter revision, declared features or context contract)")
            .contains("/rebind")
    );
    assert!(attach_repair_guidance("credential is unavailable").contains("no usable login"));
}

#[test]
fn exact_openrouter_resume_binds_selected_and_resolved_wire() {
    let mut current = SessionRecord::new("/work/openrouter".into());
    current.account = Some("openrouter".into());
    current.model = Some("vendor/model".into());
    current.composition = crate::binding::CompositionSelection::Direct;
    current.target_selection = TargetSelection::OpenRouter {
        wire: WireBinding::resolve(ProtocolSelection::Auto),
        routing: OpenRouterRoutingSelection::GatewayManaged,
    };
    let mut candidate = current.clone();
    assert!(exact_resume_target_matches(
        &current,
        &candidate,
        "vendor/model"
    ));

    candidate.target_selection = TargetSelection::OpenRouter {
        wire: WireBinding::resolve(ProtocolSelection::Responses),
        routing: OpenRouterRoutingSelection::GatewayManaged,
    };
    assert!(!exact_resume_target_matches(
        &current,
        &candidate,
        "vendor/model"
    ));
}

#[tokio::test(start_paused = true)]
async fn delayed_transition_stage_times_out_without_detaching_source() {
    let started = tokio::time::Instant::now();
    let outcome = bounded_transition_stage(Duration::from_millis(20), async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        "late"
    })
    .await;
    assert_eq!(outcome, Err(()));
    assert_eq!(started.elapsed(), Duration::from_millis(20));
}

#[test]
fn code_mode_attach_failure_names_js_sidecar() {
    let guidance =
        attach_repair_guidance("Codex Code Mode sidecar is unavailable: plakous-js-host not found");
    assert!(guidance.contains("PLAK_JS_HOST"));
    assert!(!guidance.contains("PLAK_CODE_MODE_HOST"));
    assert!(!guidance.contains("0.149.0"));
}

#[test]
fn product_javascript_attach_failure_names_the_sidecar_repair() {
    let guidance =
        attach_repair_guidance("product JavaScript host is unavailable: plakous-js-host not found");

    assert!(guidance.contains("PLAK_JS_HOST"));
    assert!(guidance.contains("plakous-js-host"));
    assert!(!guidance.contains("PLAK_CODE_MODE_HOST"));
    assert!(!guidance.contains("/login"));
}

#[test]
fn failed_startup_is_joined_before_error_returns() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (action_tx, _action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    drop(event_tx);
    let (startup_tx, startup_rx) = std::sync::mpsc::channel();
    let joined = Arc::new(AtomicBool::new(false));
    let thread_joined = Arc::clone(&joined);
    let handle = HostWorkerHandle::spawn(move || {
        startup_tx.send(Err("startup refused".to_owned())).unwrap();
        thread::sleep(Duration::from_millis(20));
        thread_joined.store(true, Ordering::Release);
        Err(CliFailure::new("startup refused"))
    });

    let Err(error) = await_host_startup(startup_rx, action_tx, event_rx, handle) else {
        panic!("failed startup unexpectedly attached");
    };
    assert_eq!(error.message(), "startup refused");
    assert!(joined.load(Ordering::Acquire));
}

#[test]
fn failed_startup_preserves_recovery_required_class_after_join() {
    let (action_tx, _action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    drop(event_tx);
    let (startup_tx, startup_rx) = std::sync::mpsc::channel();
    let handle = HostWorkerHandle::spawn(move || {
        startup_tx.send(Err("unfinished head".to_owned())).unwrap();
        Err(CliFailure::recovery_required("unfinished head"))
    });
    let Err(error) = await_host_startup(startup_rx, action_tx, event_rx, handle) else {
        panic!("failed startup unexpectedly attached");
    };
    assert_eq!(error, CliFailure::recovery_required("unfinished head"));
    assert_eq!(error.exit_code(), 5);
    assert_eq!(error.message(), "unfinished head");
}

#[test]
fn deferred_startup_never_claims_attachment_before_failed_open_falls_back() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("p");
    let session_home = SessionHome::new(home.join("sessions"));
    let store = Arc::new(crate::session_store::MemoryStore::new());
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.account = Some("codex".into());
    session.model = Some("gpt-existing".into());
    store.save(&mut session).unwrap();
    let config_id = session.id.clone();
    let revision = session.store_revision;
    let publication = Arc::new(AtomicBool::new(false));
    let config = HostConfig {
        surface: test_kit::recording_surface(),
        auto_continue: true,
        topology_admission: None,
        state_roots: crate::agent::ProductStateRoots::with_doubles(
            home.clone(),
            home.join("state"),
        ),
        product_home: home.clone(),
        home: session_home.clone(),
        store: Arc::clone(&store) as Arc<dyn SessionRecordStore>,
        session,
        target: test_kit::registered_authority(NativeProvider::Codex),
        model_id: "gpt-existing".into(),
        effort: Some(ReasoningEffort::High),
        effort_explicit: true,
        compaction_enabled: true,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: true,
        root_template: None,
        attached_one_shot: false,
        interactive: true,
        cwd: root.path().to_path_buf(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        resume_publication: Some(Arc::clone(&publication)),
        runtime_factory: Arc::new(ProductionRuntimeFactory),
        ready_path: ReadyPathTiming::default(),
        seed_history: None,
        source_releases: SourceReleases::default(),
        continuation: None,
    };
    let shell_host = test_kit::doubles_shell_host();
    let (actions, events, worker) = spawn_deferred_tui_host(move || TuiHostPreparation {
        providers: Vec::new(),
        mode: TuiHostMode::Attached(config),
        catalog_refresh: None,
        shell_host,
    });

    let mut claimed_ready = false;
    loop {
        let event = events.recv_timeout(Duration::from_secs(2)).unwrap();
        match event {
            HostEvent::AgentAttached(true) => claimed_ready = true,
            HostEvent::AgentAttached(false) => break,
            _ => {}
        }
    }
    assert!(!claimed_ready);
    assert!(!publication.load(Ordering::Acquire));
    let unchanged = store.load(&config_id).unwrap();
    assert_eq!(unchanged.store_revision, revision);
    assert!(
        session_log::read_tail(&session_home, &config_id, 8)
            .unwrap()
            .iter()
            .all(|line| !line.contains("session.resumed"))
    );
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
}
