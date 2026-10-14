//! Product-supported direct API tuples, independent of OAuth discovery.
use crate::product_config::ProductCompositionSelection;
use plakous::agent::ReasoningEffort;
use plakous_hosted::binding::PlatformProvider;

/// Every usable model one API route offers offline (the bundled reviewed
/// table under the provider's family rules), in listing order.
#[cfg(test)]
pub(crate) fn models(provider: PlatformProvider) -> Vec<String> {
    plakous_hosted::platform::platform_models(provider)
        .into_iter()
        .map(|row| row.model)
        .collect()
}

/// Whether an exact literal is a usable offline row of one API route.
pub(crate) fn enrolled(provider: PlatformProvider, model: &str) -> bool {
    plakous_hosted::platform::platform_model(provider, model).is_some()
}

/// The effort a template or record spells, when the shared ladder names it.
///
/// Admission against the model's own ladder is separate: `ultra` parses and
/// is then refused by a row that does not publish it.
pub(crate) fn api_effort(name: &str) -> Option<ReasoningEffort> {
    let (effort, explicit) = plakous_hosted::runtime::effort_from_config(Some(name));
    explicit.then_some(effort)
}

/// Admits one API tuple offline, before any session exists: the model must be
/// a usable row of the bundled table, the effort on its published ladder, and
/// the composition one the route admits. Session construction re-admits the
/// model against the provider's live list.
pub(crate) fn validate(
    provider: PlatformProvider,
    model: &str,
    effort: Option<ReasoningEffort>,
    composition: ProductCompositionSelection,
) -> Result<(), String> {
    plakous_hosted::platform::bundled_catalog(provider, "offline")
        .admit(model, effort)
        .map_err(|error| error.to_string())?;
    plakous_hosted::platform::api_composition(provider, model, composition)
        .map(|_| ())
        .map_err(|error| format!("{}: {error}", provider.account()))
}

/// The composition the root template binds: on a direct API account, what
/// `platform::api_composition` decides for its model (`plakous-default` binds
/// `native-ptc` on a CodeModeOnly OpenAI row). An inadmissible template keeps
/// its selection; config load reports it.
pub(crate) fn root_composition(
    config: &crate::product_config::ProductConfig,
) -> ProductCompositionSelection {
    let selected = config.default_composition();
    match (
        config
            .default_account()
            .and_then(PlatformProvider::for_account),
        config.default_model(),
    ) {
        (Some(provider), Some(model)) => {
            plakous_hosted::platform::api_composition(provider, model, selected).unwrap_or(selected)
        }
        _ => selected,
    }
}

pub(crate) fn entries(
    provider: PlatformProvider,
    composition: ProductCompositionSelection,
) -> Vec<plakous_host_protocol::views::ModelEntry> {
    plakous_hosted::platform::platform_models(provider)
        .iter()
        .map(|row| plakous_host_protocol::views::ModelEntry {
            id: row.model.clone(),
            tag: "API · stream · tools · reasoning".into(),
            effort_options: plakous_hosted::platform::effort_names(&row.efforts)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            extras: vec![],
            reasoning: true,
            unavailable_reason: plakous_hosted::platform::api_composition(
                provider,
                &row.model,
                composition,
            )
            .err()
            .map(str::to_owned),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline product contracts")]
mod tests {
    use super::*;
    use crate::Command;

    #[test]
    fn api_run_and_jsonl_commands_choose_exact_billed_accounts() {
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            for model in models(provider) {
                let model = model.as_str();
                for composition in ["direct", "js-ptc", "py-ptc"] {
                    let run = crate::cli::parse(
                        [
                            "run",
                            "--account",
                            provider.account(),
                            "--model",
                            model,
                            "--effort",
                            "low",
                            "--composition",
                            composition,
                            "--",
                            "fixture",
                        ]
                        .map(Into::into),
                    )
                    .unwrap();
                    assert!(
                        matches!(run,Command::RunPlatform {provider:selected,..} if selected==provider)
                    );
                    let agent = crate::cli::parse(
                        [
                            "agent",
                            "--account",
                            provider.account(),
                            "--model",
                            model,
                            "--effort",
                            "medium",
                            "--composition",
                            composition,
                        ]
                        .map(Into::into),
                    )
                    .unwrap();
                    assert!(
                        matches!(agent,Command::Agent {provider:None,platform:Some(selected),wire:None,..} if selected==provider)
                    );
                }
            }
        }
        let historical = crate::cli::parse(
            [
                "run",
                "--account",
                "supergrok",
                "--model",
                "grok-4.6",
                "--effort",
                "medium",
                "--",
                "fixture",
            ]
            .map(Into::into),
        )
        .unwrap();
        assert!(matches!(
            historical,
            Command::Run {
                provider: plakous::subscriptions::NativeProvider::Xai,
                ..
            }
        ));
    }

    #[test]
    fn api_tuple_refusals_happen_during_command_parse() {
        for (model, effort, composition) in [
            ("claude-sonnet-5", "low", "direct"),
            // `high` is on the published GPT-5.6 ladder; `ultra` is Codex-only.
            ("gpt-5.6-sol", "ultra", "direct"),
        ] {
            for command in ["run", "agent"] {
                let mut args = vec![
                    command,
                    "--account",
                    "openai",
                    "--model",
                    model,
                    "--effort",
                    effort,
                    "--composition",
                    composition,
                ];
                if command == "run" {
                    args.extend(["--", "fixture"]);
                }
                assert!(crate::cli::parse(args.into_iter().map(Into::into)).is_err());
            }
        }
        assert!(
            crate::cli::parse(
                [
                    "agent",
                    "--session",
                    "00000000-0000-0000-0000-000000000001",
                    "--account",
                    "openai"
                ]
                .map(Into::into)
            )
            .is_err()
        );
    }

    #[test]
    fn native_ptc_parses_only_for_the_openai_api_code_mode_table() {
        for command in ["run", "agent"] {
            for (account, model, admitted) in [
                ("openai", "gpt-5.6-sol", true),
                ("openai", "gpt-6-luna", true),
                ("anthropic", "claude-sonnet-5", false),
                ("xai-api", "grok-4.6", false),
            ] {
                let mut args = vec![
                    command,
                    "--account",
                    account,
                    "--model",
                    model,
                    "--effort",
                    "low",
                    "--composition",
                    "native-ptc",
                ];
                if command == "run" {
                    args.extend(["--", "fixture"]);
                }
                assert_eq!(
                    crate::cli::parse(args.into_iter().map(Into::into)).is_ok(),
                    admitted,
                    "{command} {account} {model}"
                );
            }
        }
    }

    #[test]
    fn persisted_api_target_never_falls_back_across_account_routes() {
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            let target = crate::product_config::ProductTargetSelection::Platform { provider };
            let restored: crate::product_config::ProductTargetSelection =
                serde_json::from_str(&serde_json::to_string(&target).unwrap()).unwrap();
            assert!(
                matches!(restored.route(provider.account()),Ok(plakous_hosted::binding::AttachRoute::Platform {provider:selected}) if selected==provider)
            );
            for foreign in ["codex", "claude", "grok", "xai", "openrouter"] {
                assert!(restored.route(foreign).is_err());
            }
        }
    }

    #[test]
    fn api_templates_validate_model_effort_and_composition() {
        let mut template = crate::product_config::AgentTemplate {
            account: "anthropic".into(),
            model: "claude-opus-5".into(),
            effort: Some("medium".into()),
            composition: Some(ProductCompositionSelection::PyPtc),
            ..Default::default()
        };
        template.validate("fixture").unwrap();
        template.effort = Some("max".into());
        template.validate("fixture").unwrap();
        template.effort = Some("ultra".into());
        assert!(template.validate("fixture").is_err());
        template.model = "claude-opus-4-5-20251101".into();
        template.effort = Some("low".into());
        assert!(template.validate("fixture").is_err());
        template.model = "claude-opus-5".into();
        template.effort = Some("low".into());
        template.composition = Some(ProductCompositionSelection::NativePtc);
        assert!(template.validate("fixture").is_err());
        template.account = "openai".into();
        template.model = "gpt-5.6-luna".into();
        template.validate("fixture").unwrap();
    }

    #[test]
    fn api_root_defaults_seed_the_same_session_binding_as_explicit_selection() {
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            let mut config = crate::product_config::ProductConfig::default_product();
            config.root_agent = Some("primary".into());
            config.templates.insert(
                "primary".into(),
                crate::product_config::AgentTemplate {
                    account: provider.account().into(),
                    model: models(provider)[0].clone(),
                    effort: Some("medium".into()),
                    composition: Some(ProductCompositionSelection::PyPtc),
                    ..Default::default()
                },
            );
            let mut record =
                crate::store::sessions::SessionRecord::new("/offline/workspace".into());
            crate::commands::session::seed_new_session_binding(&mut record, &config).unwrap();
            assert_eq!(
                record.target_selection,
                crate::product_config::ProductTargetSelection::Platform { provider }
            );
            assert_eq!(record.composition, ProductCompositionSelection::PyPtc);
            assert_eq!(record.effort.as_deref(), Some("medium"));
            assert_eq!(record.model.as_deref(), Some(models(provider)[0].as_str()));
            let command =
                crate::cli::parse(["agent", "--account", provider.account()].map(Into::into))
                    .unwrap();
            assert!(matches!(
                command,
                Command::Agent {
                    composition: None,
                    effort_explicit: false,
                    ..
                }
            ));
        }
    }

    /// A TUI/`plak agent` new session seeded from an OpenAI API root template
    /// on `plakous-default` binds the composition `platform::api_composition`
    /// decides (native-ptc on a CodeModeOnly row), as `plak run` does.
    #[test]
    fn seeded_openai_api_root_binds_the_api_composition_decision() {
        let provider = PlatformProvider::OpenAi;
        let model = "gpt-5.6-sol";
        let mut config = crate::product_config::ProductConfig::default_product();
        config.root_agent = Some("primary".into());
        config.templates.insert(
            "primary".into(),
            crate::product_config::AgentTemplate {
                account: provider.account().into(),
                model: model.into(),
                effort: Some("medium".into()),
                composition: None,
                ..Default::default()
            },
        );
        let mut record = crate::store::sessions::SessionRecord::new("/offline/workspace".into());
        crate::commands::session::seed_new_session_binding(&mut record, &config).unwrap();
        let decided = plakous_hosted::platform::api_composition(
            provider,
            model,
            ProductCompositionSelection::FollowCatalog,
        )
        .unwrap();
        assert_eq!(decided, ProductCompositionSelection::NativePtc);
        assert_eq!(
            record.composition, decided,
            "the seeded API root must bind what plak run binds for the same template"
        );
    }

    #[test]
    fn root_composition_resolves_an_api_template_as_its_session_would() {
        for (provider, model, composition, expected) in [
            (
                PlatformProvider::OpenAi,
                "gpt-5.6-sol",
                None,
                ProductCompositionSelection::NativePtc,
            ),
            (
                PlatformProvider::OpenAi,
                "gpt-5.6-sol",
                Some(ProductCompositionSelection::Direct),
                ProductCompositionSelection::Direct,
            ),
            (
                PlatformProvider::Xai,
                "grok-4.6",
                None,
                ProductCompositionSelection::Direct,
            ),
            (
                PlatformProvider::Anthropic,
                "claude-sonnet-5",
                Some(ProductCompositionSelection::JsPtc),
                ProductCompositionSelection::JsPtc,
            ),
        ] {
            let mut config = crate::product_config::ProductConfig::default_product();
            config.root_agent = Some("primary".into());
            config.templates.insert(
                "primary".into(),
                crate::product_config::AgentTemplate {
                    account: provider.account().into(),
                    model: model.into(),
                    effort: Some("low".into()),
                    composition,
                    ..Default::default()
                },
            );
            assert_eq!(root_composition(&config), expected, "{provider:?}/{model}");
        }
        assert_eq!(
            root_composition(&crate::product_config::ProductConfig::default_product()),
            ProductCompositionSelection::FollowCatalog
        );
    }

    /// `plak run` and `plak agent` save the same API binding for the same
    /// arguments, and both refuse what `platform::api_composition` refuses.
    #[test]
    fn run_and_agent_save_the_same_api_record_for_the_same_arguments() {
        let config = crate::product_config::ProductConfig::default_product();
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            for model in models(provider) {
                let model = model.as_str();
                for composition in [
                    None,
                    Some(ProductCompositionSelection::Direct),
                    Some(ProductCompositionSelection::NativePtc),
                    Some(ProductCompositionSelection::JsPtc),
                    Some(ProductCompositionSelection::PyPtc),
                ] {
                    let run = crate::commands::run::api_run_record(
                        "/offline/workspace".into(),
                        model,
                        Some(ReasoningEffort::Low),
                        composition.unwrap_or(ProductCompositionSelection::FollowCatalog),
                        crate::product_config::ProductTargetSelection::Platform { provider },
                    );
                    let agent = crate::commands::agent::new_agent_record(
                        &config,
                        "/offline/workspace".into(),
                        None,
                        Some(provider),
                        None,
                        Some(model.to_owned()),
                        Some(ReasoningEffort::Low),
                        true,
                        composition,
                    );
                    let context = format!("{provider:?}/{model}/{composition:?}");
                    match (run, agent) {
                        (Ok(run), Ok(agent)) => {
                            assert_ne!(
                                run.composition,
                                ProductCompositionSelection::FollowCatalog,
                                "{context}"
                            );
                            assert_eq!(
                                (
                                    run.account,
                                    run.model,
                                    run.effort,
                                    run.composition,
                                    run.target_selection
                                ),
                                (
                                    agent.account,
                                    agent.model,
                                    agent.effort,
                                    agent.composition,
                                    agent.target_selection
                                ),
                                "{context}"
                            );
                        }
                        (Err(_), Err(_)) => assert!(
                            plakous_hosted::platform::api_composition(
                                provider,
                                model,
                                composition.unwrap_or_default()
                            )
                            .is_err(),
                            "{context}"
                        ),
                        (run, agent) => panic!(
                            "{context}: run {:?} and agent {:?} disagree",
                            run.map(|record| record.composition),
                            agent.map(|record| record.composition)
                        ),
                    }
                }
            }
        }
    }

    fn listing(provider: PlatformProvider) -> String {
        let catalog = plakous_hosted::platform::bundled_catalog(provider, "offline test");
        let mut out = vec![];
        crate::commands::catalog::list_platform_models(&catalog, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// `plak models <API account>` lists the compositions a usable row admits;
    /// native-ptc is admitted (and the default) on every CodeModeOnly OpenAI
    /// API row and nowhere else.
    #[test]
    fn models_listing_names_the_admitted_native_ptc_composition() {
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            for line in listing(provider).lines().skip(1) {
                let model = line.split('\t').next().unwrap();
                let admitted = enrolled(provider, model)
                    && plakous_hosted::platform::api_composition(
                        provider,
                        model,
                        ProductCompositionSelection::NativePtc,
                    )
                    .is_ok();
                assert_eq!(
                    line.contains("native-ptc"),
                    admitted,
                    "{provider:?}: {line}"
                );
            }
        }
    }

    #[test]
    fn api_catalog_command_lists_every_listed_id_with_status_efforts_and_windows() {
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            let command =
                crate::cli::parse(["models", provider.account()].map(Into::into)).unwrap();
            assert!(matches!(command,Command::ModelsPlatform(selected) if selected==provider));
            let out = listing(provider);
            assert!(out.starts_with(&format!(
                "# {} models · bundled table (offline test)\n",
                provider.account()
            )));
            let usable = out
                .lines()
                .filter(|line| line.contains("\tusable\t"))
                .count();
            assert_eq!(usable, models(provider).len());
        }
        let anthropic = listing(PlatformProvider::Anthropic);
        assert_eq!(anthropic.lines().count(), 15, "{anthropic}");
        assert!(anthropic.contains(
            "claude-haiku-5-5\tusable\tefforts=low,medium,high,xhigh,max\twindow=100000\tfull_window=1000000\tmax_output=128000\tcomposition=direct,js-ptc,py-ptc\tplakous-default=direct\n"
        ), "{anthropic}");
        assert!(anthropic.contains(
            "claude-sonnet-5-5\tusable\tefforts=low,medium,high,xhigh,max\twindow=1000000\tfull_window=1000000\tmax_output=128000\t"
        ), "{anthropic}");
        assert!(anthropic.contains(
            "claude-sonnet-4-5-20250929\tneeds review\tefforts=\twindow=200000\tfull_window=200000\tmax_output=64000\treason=Claude 4.5 takes manual thinking budgets"
        ), "{anthropic}");
        let openai = listing(PlatformProvider::OpenAi);
        assert!(
            openai.contains(
                "gpt-6-luna\tusable\tefforts=none,low,medium,high,xhigh,max\twindow=272000\tfull_window=922000\tmax_output=128000\t"
            ),
            "{openai}"
        );
        assert!(openai.contains("gpt-6-astra\tneeds review\t"), "{openai}");
        assert!(openai.contains("tts-1\tnot a coding model\t"), "{openai}");
    }

    #[test]
    fn api_full_window_config_selects_the_full_window_for_new_sessions() {
        use crate::product_config::ProductConfig;
        use plakous_hosted::platform::ApiWindow;
        let config = ProductConfig::parse("[api]\nfull_window = true\n").unwrap();
        assert_eq!(config.session_policy().api_window(), ApiWindow::Full);
        assert_eq!(
            ProductConfig::default_product()
                .session_policy()
                .api_window(),
            ApiWindow::Working
        );
        assert!(ProductConfig::parse("[api]\nfull_window = 1\n").is_err());
        assert!(ProductConfig::parse("[api]\nwindow = true\n").is_err());
    }

    #[test]
    fn api_parse_admission_names_closest_ids_and_the_models_ladder() {
        let unknown = validate(
            PlatformProvider::Anthropic,
            "claude-haiku-55",
            Some(ReasoningEffort::Medium),
            ProductCompositionSelection::Direct,
        )
        .unwrap_err();
        assert!(unknown.contains("closest: claude-haiku-5-5"), "{unknown}");
        validate(
            PlatformProvider::Anthropic,
            "claude-haiku-5-5",
            Some(ReasoningEffort::Max),
            ProductCompositionSelection::Direct,
        )
        .unwrap();
        let ladder = validate(
            PlatformProvider::Anthropic,
            "claude-opus-4-6",
            Some(ReasoningEffort::XHigh),
            ProductCompositionSelection::Direct,
        )
        .unwrap_err();
        assert!(ladder.contains("--effort low|medium|high|max"), "{ladder}");
    }

    #[test]
    fn product_sqlite_reopen_keeps_the_exact_api_target() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("home"));
        for provider in [
            PlatformProvider::OpenAi,
            PlatformProvider::Anthropic,
            PlatformProvider::Xai,
        ] {
            let mut record =
                crate::store::sessions::SessionRecord::new(root.path().join("workspace"));
            record.account = Some(provider.account().into());
            record.model = Some(models(provider)[0].clone());
            record.effort = Some("medium".into());
            record.composition = ProductCompositionSelection::PyPtc;
            record.target_selection =
                crate::product_config::ProductTargetSelection::Platform { provider };
            let mut store = crate::store::ProductStore::open(&paths).unwrap();
            store.commit(&mut record).unwrap();
            drop(store);
            let loaded = crate::store::ProductStore::open(&paths)
                .unwrap()
                .load(&record.id)
                .unwrap();
            assert_eq!(loaded.target_selection, record.target_selection);
            assert_eq!(loaded.account, record.account);
            assert_eq!(loaded.model, record.model);
            assert_eq!(loaded.composition, ProductCompositionSelection::PyPtc);
            assert_eq!(loaded.effort.as_deref(), Some("medium"));
        }
    }
}
