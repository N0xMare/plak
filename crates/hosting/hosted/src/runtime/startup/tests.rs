#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
use plakous_host_protocol::status::AttachFailure;
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

#[test]
fn an_archived_record_never_opens_or_creates_a_runtime() {
    let root = tempfile::tempdir().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    config.session.archived = true;
    let refused = executor
        .block_on(build_session_from_config(&mut config, &mut Vec::new()))
        .unwrap()
        .err()
        .expect("archived runtime opened");
    assert!(refused.message().starts_with("session_archived:"));
    assert!(config.session.plakous_locator.is_none());
    assert!(!config.product_home.join("runtime.sqlite3").exists());
}

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
    // A server without a placement runs on the host, and each record
    // freezes so.
    let stored = config.session.frozen_tools.as_ref().unwrap();
    assert_eq!(
        stored.mcp_placements(),
        std::collections::BTreeMap::from([("fake", crate::frozen_tools::McpPlacement::Host)])
    );
    let encoded: serde_json::Value = serde_json::from_str(&stored.encode().unwrap()).unwrap();
    let records = encoded["mcp_tools"].as_array().unwrap();
    assert_eq!(records.len(), frozen.len());
    assert!(
        records.iter().all(|tool| tool["placement"] == "host"),
        "{encoded}"
    );
    // A record without a placement, with an unknown one, or a server frozen
    // under two placements does not decode.
    let decode = |value: &serde_json::Value| {
        crate::frozen_tools::FrozenRootTools::decode(&value.to_string()).is_ok()
    };
    assert!(decode(&encoded));
    let mut missing = encoded.clone();
    missing["mcp_tools"][0]
        .as_object_mut()
        .unwrap()
        .remove("placement");
    assert!(!decode(&missing));
    let mut unknown = encoded.clone();
    unknown["mcp_tools"][0]["placement"] = "elsewhere".into();
    assert!(!decode(&unknown));
    assert!(records.len() > 1, "{encoded}");
    let mut split = encoded.clone();
    split["mcp_tools"][0]["placement"] = "runner".into();
    assert!(!decode(&split));
}

/// A record that is an interrupted carry never gets an empty runtime under
/// its carry's create operation: only the carry's settlement creates it.
#[test]
fn an_interrupted_carry_record_is_never_created_empty() {
    use crate::sessions::CarryIntent;
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    config.session.carry_intent = Some(CarryIntent {
        source: "00000000-0000-4000-8000-000000000001/00000000-0000-4000-8000-000000000002/00000000-0000-4000-8000-000000000003"
            .parse()
            .unwrap(),
        continues_line: true,
        cause: "carried".into(),
        orchestration: false,
    });
    let refused = executor
        .block_on(build_session_from_config(&mut config, &mut Vec::new()))
        .unwrap()
        .err()
        .expect("an interrupted carry's record was created empty");
    assert!(
        refused.message().contains("interrupted carry"),
        "{}",
        refused.message()
    );
    let operation =
        plakous::sessions::SessionOperationId::new(config.session.create_operation_id.clone())
            .unwrap();
    let genesis = plakous::sessions::AgentSessions::genesis_locator_for(&operation).unwrap();
    let sessions = config.state_roots.runtime_sessions().unwrap();
    assert!(
        executor
            .block_on(sessions.line_head_of(genesis))
            .unwrap()
            .is_err()
    );
}

/// A created session freezes its genesis profile into the record: what the
/// roster source answers for its root bind, before the runtime create.
#[test]
fn creation_freezes_the_genesis_profile_into_the_record() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, _) = test_support::fresh_injected_attached_host(root.path());
    config.session.root_profile = None;
    let expected = sessions::RootProfile::from_roster(
        &config
            .state_roots
            .roster_source()
            .roster(
                config.session.account.as_deref().unwrap(),
                config.session.model.as_deref().unwrap(),
                config.session.effort.as_deref(),
                config.session.root_profile.as_ref(),
            )
            .unwrap(),
    );
    let opened = executor
        .block_on(build_session_from_config(&mut config, &mut Vec::new()))
        .unwrap()
        .unwrap();
    assert_eq!(config.session.root_profile, Some(expected));
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    executor.block_on(opened.release()).unwrap().unwrap();
    assert_eq!(
        config.store.load(&config.session.id).unwrap().root_profile,
        config.session.root_profile
    );
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
    assert!(failure.message().contains("earlier build"));
    assert_eq!(
        failure.remedy(),
        Some(&plakous_host_protocol::notice_remedy::NoticeRemedy::Login {
            account: plakous_host_protocol::notice_remedy::AccountTarget::Codex
        })
    );
}

#[test]
fn named_runtime_credential_failures_keep_full_reference_and_exact_remedy() {
    use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy};
    for account in ["claude/work", "codex/work", "grok/work"] {
        for failure in [
            native_credential_message("vertical", account, true),
            native_authority_unavailable("vertical", account),
            native_account_changed("vertical", account),
        ] {
            assert!(failure.message().contains(account), "{failure}");
            assert!(matches!(
                failure.remedy(),
                Some(
                    NoticeRemedy::Login {
                        account: AccountTarget::Named(named)
                    } | NoticeRemedy::RestoreAccountOrRebind {
                        account: AccountTarget::Named(named)
                    }) if named.as_str() == account
            ));
        }
    }
}

#[test]
fn native_freshness_keeps_the_recorded_slot_and_refuses_vertical_disagreement() {
    for reference in ["claude/work", "codex/work", "grok/work"] {
        let selected: crate::account::AccountRef = reference.parse().unwrap();
        let provider = selected.vertical().capability().native_provider().unwrap();
        let mut record = SessionRecord::new(std::env::temp_dir());
        record.account = Some(reference.to_owned());
        assert_eq!(native_refresh_account(&record, provider).unwrap(), selected);
        record.account = Some("openai/work".to_owned());
        let failure = native_refresh_account(&record, provider).unwrap_err();
        assert!(matches!(
            std::error::Error::source(&failure)
                .unwrap()
                .downcast_ref::<crate::capabilities::AccountAdmissionError>(),
            Some(crate::capabilities::AccountAdmissionError::AccountTargetMismatch { .. })
        ));
    }
}

#[test]
fn changed_native_account_names_both_exact_restore_and_history_carry() {
    use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy};
    let failure = native_account_changed("Claude", "claude");
    assert!(failure.message().contains("original account"));
    assert_eq!(failure.host_failure().cause, AttachFailure::AccountChanged);
    assert!(
        matches!(attach_repair_notice(&failure.host_failure(), "grok"), TranscriptRow::Notice(notice) if notice.remedy == Some(NoticeRemedy::RestoreAccountOrRebind { account: AccountTarget::Claude }))
    );
    let failure =
        CliFailure::new("unrelated text").with_attach_failure(AttachFailure::LoginSelectionChanged);
    assert!(
        matches!(attach_repair_notice(&failure.host_failure(), "claude"), TranscriptRow::Notice(notice) if notice.remedy == Some(NoticeRemedy::CarryCurrentLogin {}))
    );
}

#[test]
fn claude_refresh_rejection_and_attach_guidance_name_distinct_repairs() {
    use AttachFailure;
    let failure = native_credential_message("Claude", "claude", true);
    assert_eq!(
        failure.host_failure().cause,
        AttachFailure::CredentialRejected
    );
    assert!(failure.remedy().is_some());
    let unavailable = native_authority_unavailable("Claude", "claude");
    assert_eq!(
        unavailable.host_failure().cause,
        AttachFailure::CredentialUnavailable
    );
    let misleading = CliFailure::new("login selection changed; Python host is unavailable");
    assert!(
        matches!(attach_repair_notice(&misleading.host_failure(), "claude"), TranscriptRow::Notice(notice) if notice.remedy == Some(plakous_host_protocol::notice_remedy::NoticeRemedy::RepairThenAttach {}))
    );
}

#[test]
fn a_changed_frozen_binding_leads_the_attach_repair_with_the_carry() {
    use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy};
    let failure = CliFailure::new(
        "resolved harness plan is bound to a different model capability binding (adapter revision, declared features or context contract)",
    )
    .with_attach_failure(AttachFailure::BindingChanged);
    assert!(matches!(
        attach_repair_notice(&failure.host_failure(), "claude"),
        TranscriptRow::Notice(notice)
            if notice.remedy == Some(NoticeRemedy::RestoreBindingOrCarry { account: AccountTarget::Claude })
                && notice.text.starts_with("explicitly carry the full history into a new session")
                && !notice.text.contains('/')
    ));
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
    let failure = CliFailure::new("arbitrary diagnostic")
        .with_attach_failure(AttachFailure::JavaScriptHostUnavailable);
    assert!(
        matches!(attach_repair_notice(&failure.host_failure(), "codex"), TranscriptRow::Notice(notice) if notice.remedy == Some(plakous_host_protocol::notice_remedy::NoticeRemedy::RepairJavaScriptHost {}))
    );
}

#[test]
fn product_javascript_attach_failure_names_the_sidecar_repair() {
    let failure = CliFailure::new("arbitrary diagnostic")
        .with_attach_failure(AttachFailure::PythonHostUnavailable);
    assert!(
        matches!(attach_repair_notice(&failure.host_failure(), "claude"), TranscriptRow::Notice(notice) if notice.remedy == Some(plakous_host_protocol::notice_remedy::NoticeRemedy::RepairPythonHost {}))
    );
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
        startup_tx
            .send(Err(CliFailure::new("startup refused")))
            .unwrap();
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
        startup_tx
            .send(Err(CliFailure::recovery_required("unfinished head")))
            .unwrap();
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
        resume_after_loss: ResumeAfterLoss::Hold,
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
        seed_line: None,
        source_releases: SourceReleases::default(),
        idle_release: None,
        lease_holder: None,
        continuation: None,
        event_renderer: None,
    };
    let shell_host = test_kit::doubles_shell_host();
    let (actions, events, worker) = spawn_deferred_host(move || HostLaunchPreparation {
        providers: Vec::new(),
        mode: HostLaunchMode::Attached(config),
        catalog_refresh: None,
        shell_host,
    });

    let mut claimed_ready = false;
    loop {
        // A liveness bound only: generous, so load never trips it.
        let event = events.recv_timeout(Duration::from_secs(30)).unwrap();
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

struct ReopenFixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
    controller: plakous_offline_crash_harness::BoundaryController,
    executor: plakous::application::ApplicationExecutor,
}

impl ReopenFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("evidence.jsonl"),
            )
            .unwrap();
        Self {
            root,
            workspace,
            controller,
            executor: plakous::application::ApplicationExecutor::new().unwrap(),
        }
    }

    fn sessions(&self, name: &str) -> Arc<plakous::sessions::AgentSessions> {
        Arc::new(
            plakous::sessions::AgentSessions::new(
                plakous::sessions::SessionBackend::sqlite(
                    self.root.path().join(format!("{name}.sqlite3")),
                )
                .unwrap(),
            )
            .unwrap(),
        )
    }

    fn runtime(&self, changed: bool) -> SessionRuntimeBinding {
        if changed {
            // A smaller admitted input changes the model capability binding.
            plakous_offline_crash_harness::overflow_runtime_binding(
                &self.workspace,
                self.controller.clone(),
            )
        } else {
            plakous_offline_crash_harness::runtime_binding(&self.workspace, self.controller.clone())
        }
        .unwrap()
    }

    fn create(
        &self,
        sessions: &plakous::sessions::AgentSessions,
    ) -> plakous::sessions::SessionLocator {
        let created = self
            .executor
            .block_on(
                sessions.create(plakous::sessions::CreateSessionRequest::new(
                    plakous::sessions::SessionOperationId::new("reopen-fixture:create").unwrap(),
                    self.runtime(false),
                )),
            )
            .unwrap()
            .unwrap();
        let plakous::sessions::CreateSessionOutcome::Created { session, .. } = created else {
            panic!("a fresh create creates its session");
        };
        let locator = session.locator();
        self.executor.block_on(session.release()).unwrap().unwrap();
        locator
    }

    /// Both mapping sites: the conversion of a direct open refusal and the
    /// exact reopen path's own failure.
    fn refusals(
        &self,
        sessions: &Arc<plakous::sessions::AgentSessions>,
        locator: plakous::sessions::SessionLocator,
        changed: bool,
    ) -> (
        plakous::sessions::AgentSessionsError,
        crate::agent::CodingSessionOpenFailure,
    ) {
        let opened = self
            .executor
            .block_on(sessions.open(plakous::sessions::OpenSessionRequest::new(
                plakous::sessions::SessionOperationId::new("reopen-fixture:open").unwrap(),
                locator,
                self.runtime(changed),
            )))
            .unwrap();
        let Err(direct) = opened else {
            panic!("the open must be refused");
        };
        let reopened = self
            .executor
            .block_on(crate::agent::build_or_open_bound_session(
                Arc::clone(sessions),
                Some(locator),
                None,
                plakous::sessions::SessionOperationId::new("reopen-fixture:reopen").unwrap(),
                self.runtime(changed),
            ))
            .unwrap();
        let Err(reopened) = reopened else {
            panic!("the exact reopen must be refused");
        };
        (direct, reopened)
    }
}

/// A reopen refused for a reconstructed frozen binding input is typed as a
/// binding change on both mapping sites and selects the carry remedy.
#[test]
fn a_reconstructed_binding_change_types_the_attach_failure_and_selects_the_carry() {
    use plakous_host_protocol::notice_remedy::{AccountTarget, NoticeRemedy};
    let fixture = ReopenFixture::new();
    let sessions = fixture.sessions("runtime");
    let locator = fixture.create(&sessions);
    let (direct, reopened) = fixture.refusals(&sessions, locator, true);
    let change = plakous::agent::BindingChange::ModelCapabilities;
    assert_eq!(direct.binding_change(), Some(&change));
    assert_eq!(reopened.binding_change.as_deref(), Some(&change));
    for failure in [
        CliFailure::from(direct).host_failure(),
        reopened.failure.host_failure(),
    ] {
        assert_eq!(failure.cause, AttachFailure::BindingChanged);
        assert!(matches!(
            attach_repair_notice(&failure, "claude"),
            TranscriptRow::Notice(notice)
                if notice.remedy == Some(NoticeRemedy::RestoreBindingOrCarry { account: AccountTarget::Claude })
        ));
    }
}

/// A refusal that names no binding change is never typed as one.
#[test]
fn a_refusal_without_a_binding_change_is_never_typed_as_one() {
    let fixture = ReopenFixture::new();
    let sessions = fixture.sessions("runtime");
    // A locator this store never wrote.
    let foreign = fixture.create(&fixture.sessions("elsewhere"));
    let (direct, reopened) = fixture.refusals(&sessions, foreign, false);
    assert_eq!(direct.binding_change(), None);
    assert_eq!(reopened.binding_change, None);
    assert_ne!(
        CliFailure::from(direct).host_failure().cause,
        AttachFailure::BindingChanged
    );
    assert_ne!(
        reopened.failure.host_failure().cause,
        AttachFailure::BindingChanged
    );
}

/// `/compact` (role `Compact`) and the API fork path (role `Primary`) build an
/// API-target root's runtime through the production factory from inside the
/// host executor. The build must run there and reach the route's own answer;
/// doubled credentials and catalog refuse it offline, before any request.
#[test]
fn api_target_runtime_builds_run_inside_the_host_executor() {
    let root = tempfile::tempdir().unwrap();
    let mut config = test_support::no_effort_openrouter_injected_host(root.path());
    config.runtime_factory = Arc::new(ProductionRuntimeFactory);
    let subscriptions = NativeSubscriptions::new(
        plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(root.path().join("xdg"))
            .unwrap(),
    );
    let executor = ApplicationExecutor::new().unwrap();
    let mut platform = config.session.clone();
    platform.account = Some(crate::binding::PlatformProvider::Anthropic.account().into());
    platform.target_selection = TargetSelection::Platform {
        provider: crate::binding::PlatformProvider::Anthropic,
    };
    platform.effort = Some("medium".into());
    for (target, session, expected) in [
        (
            HostTargetAuthority::OpenRouter {
                subscriptions: subscriptions.clone(),
                evidence: None,
            },
            config.session.clone(),
            "OpenRouter API key is unavailable",
        ),
        (
            HostTargetAuthority::Platform {
                provider: crate::binding::PlatformProvider::Anthropic,
                subscriptions: subscriptions.clone(),
            },
            platform.clone(),
            "anthropic does not list model `vendor/model-v1`; inspect the account's available models",
        ),
    ] {
        config.target = target;
        config.session = session;
        for role in [RuntimeRole::Compact, RuntimeRole::Primary] {
            let mut stderr = Vec::new();
            let Err(refused) = executor
                .block_on(host_runtime_binding(&config, &mut stderr, role))
                .unwrap()
            else {
                panic!("doubled credentials refuse the API build");
            };
            assert_eq!(
                refused.message(),
                expected,
                "{role:?} on {:?}",
                config.session.target_selection
            );
        }
    }
}

/// Only an open refused for a held lease is renamed for its live holder,
/// typed `tree_hosted_elsewhere` with the lease's refusal kept; any other
/// refusal (a binding change, say) stays as it is while a holder record
/// names a live process, and with no product naming nothing is renamed.
#[test]
fn only_a_held_lease_refusal_names_its_holder() {
    let holder: LeaseHolder = Arc::new(|_| {
        Some("tree_hosted_elsewhere: session s is open in another plak process (pid 7 on m)".into())
    });
    let head: plakous::sessions::SessionLocator = "00000000-0000-4000-8000-0000000000a1/00000000-0000-4000-8000-0000000000b1/00000000-0000-4000-8000-0000000000c1"
        .parse()
        .unwrap();
    let refused = |lease_held| {
        let mut failure =
            crate::agent::CodingSessionOpenFailure::from(CliFailure::new("the binding changed"));
        failure.lease_held = lease_held;
        Err::<AttachedCodingSession, _>(failure)
    };
    let Err(kept) = name_lease_holder(Some(&holder), head, refused(false)) else {
        panic!("opened");
    };
    assert_eq!(kept.failure.code(), None);
    assert_eq!(kept.failure.message(), "the binding changed");
    let Err(named) = name_lease_holder(Some(&holder), head, refused(true)) else {
        panic!("opened");
    };
    assert_eq!(named.failure.code(), Some("tree_hosted_elsewhere"));
    assert!(
        named
            .failure
            .message()
            .ends_with("; the open refused: the binding changed"),
        "{}",
        named.failure.message()
    );
    let Err(unnamed) = name_lease_holder(None, head, refused(true)) else {
        panic!("opened");
    };
    assert_eq!(unnamed.failure.code(), None);
}
