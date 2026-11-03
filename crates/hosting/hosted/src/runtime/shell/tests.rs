#![allow(
    clippy::too_many_lines,
    reason = "Shell profile scenarios bind native, API, and OpenRouter roots to their exact selected account and model facts."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostConfig = crate::runtime::HostConfig<TestExtension>;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type ShellHostConfig = crate::runtime::ShellHostConfig<TestExtension>;

use super::*;
use crate::runtime::*;

use crate::binding::CompositionSelection as ProductComposition;
use crate::binding::TargetSelection;
use crate::sessions::SessionRecord;
use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;

#[test]
fn detached_clear_publishes_binding_before_clear_view_and_target_transcript() {
    let root = tempfile::tempdir().unwrap();
    let fixture = test_kit::shell_host_on_doubles::<TestExtension>(
        root.path(),
        SessionRecord::new(root.path().to_path_buf()),
        None,
    );
    let source_id = fixture.config.session.id.clone();
    let (actions, input) = crate::surface_protocol::user_action_channel::<TestExtension>();
    actions.send(UserAction::ClearSession).unwrap();
    drop(actions);
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    run_shell_host_loop(fixture.config, input, Sender::over(tx), None);
    let observed = rx.try_iter().collect::<Vec<_>>();
    let boundary = observed.iter().position(|event| matches!(event, HostEvent::Binding { session_id: Some(id), .. } if id != &source_id)).unwrap();
    let clear = observed
        .iter()
        .position(|event| matches!(event, HostEvent::ClearView))
        .unwrap();
    let transcript = observed
        .iter()
        .rposition(|event| matches!(event, HostEvent::PresentationSnapshot(_)))
        .unwrap();
    assert!(
        boundary < clear && clear < transcript,
        "target binding must survive presentation clear: {observed:#?}"
    );
}

#[test]
fn a_shell_queue_edit_emits_exactly_one_success_receipt() {
    let root = tempfile::tempdir().unwrap();
    let home_dir = root.path().join("home");
    let store = Arc::new(crate::session_store::MemoryStore::new());
    let mut session = SessionRecord::new(root.path().to_path_buf());
    queue::enqueue(&mut session.pending_queue, "before").unwrap();
    store.save(&mut session).unwrap();
    let item = session.pending_queue[0].clone();
    let mut config = ShellHostConfig {
        surface: test_kit::recording_surface(),
        binding: Arc::new(test_kit::MemoryBindingDocument::empty()),
        subscriptions: None,
        openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
        home: SessionHome::new(home_dir.join("sessions")),
        store,
        product_home: home_dir,
        session,
        cwd: root.path().to_path_buf(),
        reason: "test".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
    };
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let sender = Sender::over(wire);
    handle_shell_queue_edit(
        &mut config,
        &sender,
        ShellQueueEdit::Edit {
            id: item.id.clone(),
            revision: item.revision,
            text: "after".into(),
        },
    );
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(event, HostEvent::QueueEditApplied { id } if id == &item.id))
            .count(),
        1
    );
    assert_eq!(config.session.pending_queue[0].text, "after");
}

/// An OpenRouter root binds the wire its template names on the
/// gateway-managed route; a direct API root binds its account's route.
/// Effort and composition follow what a new session from that template
/// binds.
#[test]
fn a_profile_root_names_its_api_or_openrouter_target_exactly() {
    let openrouter =
        profile_target_selection("openrouter", None, Some(ProtocolSelection::Responses)).unwrap();
    assert_eq!(
        openrouter,
        TargetSelection::OpenRouter {
            wire: WireBinding::resolve(ProtocolSelection::Responses),
            routing: OpenRouterRoutingSelection::GatewayManaged,
        }
    );
    assert_eq!(
        profile_target_selection("openrouter", None, None).unwrap(),
        TargetSelection::OpenRouter {
            wire: WireBinding::resolve(ProtocolSelection::Auto),
            routing: OpenRouterRoutingSelection::GatewayManaged,
        }
    );
    assert!(profile_target_selection("openrouter", Some("priority"), None).is_err());
    assert!(profile_target_selection("claude", None, Some(ProtocolSelection::Chat)).is_err());
    let platform = profile_target_selection("openai", None, None).unwrap();
    assert_eq!(
        platform,
        TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::OpenAi
        }
    );

    assert_eq!(
        api_root_binding(
            &openrouter,
            "openai/gpt-5.5",
            Some("high"),
            ProductComposition::PyPtc
        ),
        Ok((None, ProductComposition::PyPtc))
    );
    assert_eq!(
        api_root_binding(
            &openrouter,
            "openai/gpt-5.5",
            None,
            ProductComposition::FollowCatalog
        ),
        Ok((None, ProductComposition::Direct))
    );
    // The template's effort binds as written; nothing defaults it.
    assert_eq!(
        api_root_binding(&platform, "gpt-5.6-luna", None, ProductComposition::JsPtc),
        Err("an API root takes an effort its model publishes")
    );
    assert_eq!(
        api_root_binding(
            &platform,
            "gpt-5.6-luna",
            Some("medium"),
            ProductComposition::JsPtc
        ),
        Ok((Some("medium".to_owned()), ProductComposition::JsPtc))
    );
    assert_eq!(
        api_root_binding(
            &platform,
            "gpt-5.6-luna",
            Some("low"),
            ProductComposition::Direct
        ),
        Ok((Some("low".to_owned()), ProductComposition::Direct))
    );
    assert_eq!(
        api_root_binding(
            &platform,
            "gpt-5.6-luna",
            Some("high"),
            ProductComposition::Direct
        ),
        Ok((Some("high".to_owned()), ProductComposition::Direct))
    );
    // `ultra` is a Codex client level, not an API effort.
    assert!(
        api_root_binding(
            &platform,
            "gpt-5.6-luna",
            Some("ultra"),
            ProductComposition::Direct
        )
        .is_err()
    );
    // The OpenAI API follows its enrolled tool-mode table: a CodeModeOnly
    // row binds native-ptc by default and admits it explicitly.
    assert_eq!(
        api_root_binding(
            &platform,
            "gpt-5.6-luna",
            Some("low"),
            ProductComposition::NativePtc
        ),
        Ok((Some("low".to_owned()), ProductComposition::NativePtc))
    );
    assert_eq!(
        api_root_binding(
            &platform,
            "gpt-6-sol",
            Some("medium"),
            ProductComposition::FollowCatalog
        ),
        Ok((Some("medium".to_owned()), ProductComposition::NativePtc))
    );
    assert!(
        api_root_binding(
            &platform,
            "gpt-3",
            Some("low"),
            ProductComposition::NativePtc
        )
        .is_err()
    );
    for provider in [
        crate::binding::PlatformProvider::Anthropic,
        crate::binding::PlatformProvider::Xai,
    ] {
        let model = crate::platform::platform_models(provider)[0].model.clone();
        let target = TargetSelection::Platform { provider };
        assert!(
            api_root_binding(&target, &model, Some("low"), ProductComposition::NativePtc).is_err(),
            "{provider:?}"
        );
        assert_eq!(
            api_root_binding(
                &target,
                &model,
                Some("low"),
                ProductComposition::FollowCatalog
            ),
            Ok((Some("low".to_owned()), ProductComposition::Direct)),
            "{provider:?}"
        );
    }
    assert!(
        api_root_binding(
            &openrouter,
            "openai/gpt-5.5",
            None,
            ProductComposition::NativePtc
        )
        .is_err()
    );
    assert!(
        api_root_binding(
            &TargetSelection::Native,
            "gpt-5.6-luna",
            None,
            ProductComposition::Direct
        )
        .is_err()
    );
}

#[test]
fn native_model_transition_selects_the_codex_fast_lane_only() {
    assert_eq!(
        profile_target_selection("codex", None, None).unwrap(),
        TargetSelection::Native
    );
    assert_eq!(
        profile_target_selection("codex", Some("priority"), None).unwrap(),
        TargetSelection::NativePriority
    );
    assert_eq!(
        profile_target_selection("grok", Some("priority"), None).unwrap_err(),
        "model: provider-specific option is not admitted for this target"
    );
    assert_eq!(
        profile_target_selection("openai", Some("priority"), None).unwrap_err(),
        "model: provider-specific option is not admitted for this API target"
    );
}

#[test]
fn shell_binding_config_failure_rolls_back_session_and_live_candidate() {
    let root = tempfile::tempdir().unwrap();
    let home_dir = root.path().join("p");
    let record_store = Arc::new(crate::session_store::MemoryStore::new());
    let binding_document = test_kit::MemoryBindingDocument::empty();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.account = Some("codex".into());
    record_store.save(&mut session).unwrap();
    let id = session.id.clone();
    let mut config = ShellHostConfig {
        surface: test_kit::recording_surface(),
        binding: Arc::new(Arc::clone(&binding_document)) as Arc<dyn RootBindingDocument>,
        subscriptions: None,
        openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
        home: SessionHome::new(home_dir.join("sessions")),
        store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
        product_home: home_dir.clone(),
        session: session.clone(),
        cwd: root.path().to_path_buf(),
        reason: "test".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
    };
    let mut candidate = session.clone();
    candidate.account = Some("grok".into());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);

    assert_eq!(
        commit_shell_binding_candidate(&mut config, &events, "account", candidate, || {
            Err(RootBindingSaveError::BeforePublication(
                "injected refusal".into(),
            ))
        }),
        ShellBindingOutcome::Refused
    );
    assert_eq!(config.session.account.as_deref(), Some("codex"));
    let persisted = config.store.load(&id).unwrap();
    assert_eq!(persisted.account.as_deref(), Some("codex"));
    let mut second = config.session.clone();
    second.account = Some("openai".into());
    assert_eq!(
        commit_shell_binding_candidate(&mut config, &events, "account", second.clone(), || {
            panic!("an incomplete API tuple must refuse before configuration publication")
        }),
        ShellBindingOutcome::Refused
    );
    assert_eq!(config.session.account.as_deref(), Some("codex"));
    assert_eq!(
        config.store.load(&id).unwrap().account.as_deref(),
        Some("codex")
    );
    second.model = Some("gpt-5.6-luna".into());
    second.effort = Some("low".into());
    second.composition = ProductComposition::Direct;
    second.target_selection = TargetSelection::Platform {
        provider: crate::binding::PlatformProvider::OpenAi,
    };

    assert_eq!(
        commit_shell_binding_candidate(&mut config, &events, "account", second, || Ok(())),
        ShellBindingOutcome::Committed
    );
    assert_eq!(config.session.account.as_deref(), Some("openai"));
    assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(config.session.effort.as_deref(), Some("low"));
    assert_eq!(
        config.session.target_selection,
        TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::OpenAi,
        }
    );

    assert_eq!(
        config.store.load(&id).unwrap().account.as_deref(),
        Some("openai")
    );
    assert!(rx.try_iter().any(|event| matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
            if note.contains("injected refusal") && note.contains("rolled back")
    )));
}

#[test]
fn shell_binding_session_failure_never_publishes_config_or_live_candidate() {
    let root = tempfile::tempdir().unwrap();
    let home_dir = root.path().join("p");
    // The session write fails: a binding that cannot be persisted must
    // refuse, and must not publish the configuration half either.
    let record_store = Arc::new(fault_store::FaultStore::refusing(
        "product session store is unreadable",
    ));
    let binding_document = test_kit::MemoryBindingDocument::empty();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.account = Some("codex".into());
    let mut config = ShellHostConfig {
        surface: test_kit::recording_surface(),
        binding: Arc::new(Arc::clone(&binding_document)) as Arc<dyn RootBindingDocument>,
        subscriptions: None,
        openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
        home: SessionHome::new(home_dir.join("sessions")),
        store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
        product_home: home_dir.clone(),
        session: session.clone(),
        cwd: root.path().to_path_buf(),
        reason: "test".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
    };
    let mut candidate = session;
    candidate.account = Some("grok".into());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let config_published = std::cell::Cell::new(false);

    assert_eq!(
        commit_shell_binding_candidate(&mut config, &events, "account", candidate, || {
            config_published.set(true);
            Ok(())
        }),
        ShellBindingOutcome::Refused
    );
    assert!(!config_published.get());
    assert_eq!(config.session.account.as_deref(), Some("codex"));
    assert!(rx.try_iter().any(|event| matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
            if note.contains("session write failed") && note.contains("source was unchanged")
    )));
}

#[test]
fn shell_binding_durability_uncertainty_keeps_published_authorities_aligned() {
    let root = tempfile::tempdir().unwrap();
    let home_dir = root.path().join("p");
    let record_store = Arc::new(crate::session_store::MemoryStore::new());
    let binding_document = test_kit::MemoryBindingDocument::empty();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.account = Some("codex".into());
    record_store.save(&mut session).unwrap();
    let id = session.id.clone();
    let mut config = ShellHostConfig {
        surface: test_kit::recording_surface(),
        binding: Arc::new(Arc::clone(&binding_document)) as Arc<dyn RootBindingDocument>,
        subscriptions: None,
        openrouter_credentials: Arc::new(crate::agent::roots::doubles::NoOpenRouterCredential),
        home: SessionHome::new(home_dir.join("sessions")),
        store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
        product_home: home_dir.clone(),
        session: session.clone(),
        cwd: root.path().to_path_buf(),
        reason: "test".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
    };
    let mut candidate = session;
    candidate.account = Some("grok".into());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);

    assert_eq!(
        commit_shell_binding_candidate(&mut config, &events, "account", candidate, || {
            Err(RootBindingSaveError::PublishedDurabilityUncertain(
                "injected directory sync failure".into(),
            ))
        }),
        ShellBindingOutcome::RecoveryRequired
    );
    assert_eq!(config.session.account.as_deref(), Some("grok"));
    assert_eq!(
        config.store.load(&id).unwrap().account.as_deref(),
        Some("grok")
    );
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
            if note.contains("durability uncertain") && note.contains("recovery required")
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Binding { account: Some(account), .. } if account == "grok"
    )));
    assert!(!observed.contains(&HostEvent::ReattachReady));
}

#[test]
fn attached_oauth_ready_refresh_never_rebinds_canonical_agent() {
    let root = tempfile::tempdir().unwrap();
    let home_dir = root.path().join("p");
    let record_store = Arc::new(crate::session_store::MemoryStore::new());
    let _binding_document = test_kit::MemoryBindingDocument::empty();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.account = Some("codex".into());
    session.model = Some("gpt-existing".into());
    let mut config = HostConfig {
        surface: test_kit::recording_surface(),
        auto_continue: true,
        topology_admission: None,
        state_roots: crate::agent::ProductStateRoots::with_doubles(
            home_dir.clone(),
            home_dir.join("state"),
        ),
        home: SessionHome::new(home_dir.join("sessions")),
        store: Arc::clone(&record_store) as Arc<dyn SessionRecordStore>,
        topology_enabled: true,
        product_home: home_dir.clone(),
        session,
        target: test_kit::registered_authority(NativeProvider::Codex),
        model_id: "gpt-existing".into(),
        effort: Some(ReasoningEffort::High),
        effort_explicit: true,
        compaction_enabled: true,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        root_template: None,
        attached_one_shot: false,
        interactive: true,
        cwd: root.path().to_path_buf(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        resume_publication: None,
        runtime_factory: Arc::new(ProductionRuntimeFactory),
        ready_path: ReadyPathTiming::default(),
        seed_history: None,
        seed_line: None,
        source_releases: SourceReleases::default(),
        continuation: None,
        event_renderer: None,
    };
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);

    // A readiness refresh re-reads external credential authority only: the
    // attached branch keeps its exact provider/model binding.
    let mut readiness = ReadinessRefresh::new();
    readiness.start(&config);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !rx
        .try_iter()
        .any(|event| matches!(event, HostEvent::ProviderSnapshot { .. }))
    {
        assert!(Instant::now() < deadline, "readiness never settled");
        thread::sleep(Duration::from_millis(5));
        readiness.apply_settled(&mut config, &events);
    }
    assert_eq!(config.target.native_provider(), Some(NativeProvider::Codex));
    assert_eq!(config.session.account.as_deref(), Some("codex"));
    assert_eq!(config.session.model.as_deref(), Some("gpt-existing"));
    assert_eq!(config.model_id, "gpt-existing");
}

/// The product's readiness read may wait on another holder's
/// subscription lock: starting it never waits for it, and a slower,
/// older read that settles last cannot replace the newer catalog.
#[test]
fn readiness_reads_run_off_the_loop_and_only_the_newest_applies() {
    struct GatedReadiness(std::sync::Mutex<Vec<(usize, std::sync::mpsc::Receiver<()>)>>);
    impl SurfaceHandler<TestExtension> for GatedReadiness {
        fn dispatch(
            &self,
            _: test_kit::ProbeIntent,
            _: SurfaceFacts<'_>,
            _: &Sender,
        ) -> SurfaceEffect {
            SurfaceEffect::default()
        }
        fn publish_template_readiness(
            &self,
            _: &[plakous_host_protocol::views::ProviderSection],
            _: &Sender,
        ) {
        }
        fn readiness_read(&self) -> ReadinessRead {
            let (read, gate) = self.0.lock().unwrap().remove(0);
            Box::new(move || {
                gate.recv().unwrap();
                Ok((
                    Vec::new(),
                    vec![plakous_host_protocol::views::ProviderSection {
                        account: format!("read-{read}"),
                        label: String::new(),
                        ready: true,
                        entries: Vec::new(),
                    }],
                ))
            })
        }
    }
    struct Host {
        catalog: plakous_host_protocol::views::ReadyCatalog,
        surface: GatedReadiness,
    }
    impl ProductAuthHost<TestExtension> for Host {
        fn replace_ready_catalog(&mut self, catalog: plakous_host_protocol::views::ReadyCatalog) {
            self.catalog = catalog;
        }
        fn ready_catalog(&self) -> &plakous_host_protocol::views::ReadyCatalog {
            &self.catalog
        }
        fn surface(&self) -> Option<&dyn SurfaceHandler<TestExtension>> {
            Some(&self.surface)
        }
    }
    let (older_tx, older) = std::sync::mpsc::channel();
    let (newer_tx, newer) = std::sync::mpsc::channel();
    let mut host = Host {
        catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        surface: GatedReadiness(std::sync::Mutex::new(vec![(0, older), (1, newer)])),
    };
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let mut readiness = ReadinessRefresh::new();
    // Both reads are still blocked; neither start waited for its read.
    readiness.start(&host);
    readiness.start(&host);
    readiness.apply_settled(&mut host, &events);
    assert_eq!(host.catalog.accounts, []);

    let settle = |host: &mut Host, readiness: &ReadinessRefresh| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.catalog.accounts.is_empty() {
            assert!(Instant::now() < deadline, "readiness never settled");
            thread::sleep(Duration::from_millis(5));
            readiness.apply_settled(host, &events);
        }
    };
    newer_tx.send(()).unwrap();
    settle(&mut host, &readiness);
    assert_eq!(host.catalog.accounts[0].account, "read-1");
    older_tx.send(()).unwrap();
    thread::sleep(Duration::from_millis(50));
    readiness.apply_settled(&mut host, &events);
    assert_eq!(host.catalog.accounts[0].account, "read-1");
}
