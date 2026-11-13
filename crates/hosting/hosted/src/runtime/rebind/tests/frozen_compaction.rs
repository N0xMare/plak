//! The host dispatch's production compact build must inherit source presentation data.
use super::*;

#[test]
fn production_host_compact_build_keeps_a_previous_presentation_on_native_and_api() {
    for native in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, current) = actual_api_topology_host(root.path(), &executor);
        let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
        let persisted = crate::topology_runtime::lookup(&tree_id)
            .unwrap()
            .read(crate::agent_tree::ProductAgentTree::to_persisted)
            .unwrap();
        let tree = Arc::new(std::sync::Mutex::new(
            crate::agent_tree::ProductAgentTree::reopen(persisted),
        ));
        let extensions =
            crate::topology_extensions::topology_tool_extensions(tree, &tree_id).unwrap();
        let previous =
            crate::agent::previous_send_extensions(&extensions, ProductComposition::Direct, false)
                .unwrap();
        let authority = info::plan_state_authority(&*config.store, &config.session).unwrap();
        let seed = if native {
            let spec = crate::agent::native_fixture_spec(
                &config.session.workspace_root,
                config.state_roots.clone(),
                &tree_id,
            )
            .unwrap();
            config.target = HostTargetAuthority::Native {
                provider: spec.provider,
                resolved: spec.resolved.clone(),
                subscriptions: config.state_roots.native_subscriptions().unwrap(),
                catalog_candidates: Arc::from([]),
            };
            config.session.account = Some("claude".into());
            config.session.model = Some("claude-opus-5-5".into());
            config.model_id = "claude-opus-5-5".into();
            config.session.effort = Some("high".into());
            config.effort = Some(ReasoningEffort::High);
            config.session.target_selection = TargetSelection::Native;
            crate::agent::frozen_native_fixture_runtime(
                &previous,
                &config.session.workspace_root,
                &authority,
            )
            .unwrap()
        } else {
            let spec = crate::agent::build::api::ApiRuntimeSpec {
                runner_services: None,
                job_config: config.state_roots.job_config(),
                account: "openai".parse().unwrap(),
                sidecars: &crate::agent::roots::doubles::NoSidecars,
                product_home: Some(&config.product_home),
                literal_model: &config.model_id,
                effort: config.effort,
                composition: ProductComposition::Direct,
                wire: crate::binding::PlatformProvider::OpenAi.protocol(),
                workspace_root: &config.session.workspace_root,
                cwd: &config.cwd,
                plan_state_authority: &authority,
                opening_durable_session: false,
                tool_fence: crate::agent::SessionToolFence::Full,
                scheduled_compaction: false,
                max_model_rounds: config.max_model_rounds,
                topology_extensions: &previous,
                telemetry: None,
                spawned_child: false,
            };
            executor
                .block_on(crate::platform::tests::configured_fixture_runtime(
                    &spec,
                    config.state_roots.platform_credentials(),
                ))
                .unwrap()
                .unwrap()
        };
        let sessions = config.state_roots.runtime_sessions().unwrap();
        let create = |binding| {
            executor
                .block_on(
                    sessions.create(plakous::sessions::CreateSessionRequest::new(
                        SessionOperationId::new(format!("compact-old:{}", uuid::Uuid::new_v4()))
                            .unwrap(),
                        binding,
                    )),
                )
                .unwrap()
                .unwrap()
        };
        let plakous::sessions::CreateSessionOutcome::Created { session: seed, .. } = create(seed)
        else {
            panic!("seed")
        };
        let presentation = executor
            .block_on(sessions.created_tool_presentation(seed.locator()))
            .unwrap()
            .unwrap();
        let mut source = seed;
        if native {
            config.state_roots = config
                .state_roots
                .clone()
                .with_frozen_presentation(Arc::new(presentation));
            let runtime = executor
                .block_on(ProductionRuntimeFactory.runtime(
                    &config,
                    RuntimeRole::Primary,
                    &mut Vec::new(),
                ))
                .unwrap()
                .unwrap();
            executor.block_on(source.release()).unwrap().unwrap();
            let plakous::sessions::CreateSessionOutcome::Created { session, .. } = create(runtime)
            else {
                panic!("production source")
            };
            source = session;
        }
        config.state_roots = config.state_roots.without_frozen_presentation();
        let locator = source.locator();
        let compact = executor
            .block_on(compact::frozen_compact_runtime(
                &mut config,
                &mut Vec::new(),
                locator,
            ))
            .unwrap()
            .unwrap();
        assert!(
            config.state_roots.frozen_presentation().is_none(),
            "construction must restore prior roots"
        );
        let frozen = executor
            .block_on(sessions.created_tool_presentation(locator))
            .unwrap()
            .unwrap();
        assert_eq!(
            compact.target_binding().selected_provider_configuration(),
            frozen.configuration().selected_identity()
        );
        assert_eq!(
            compact.target_binding().effective_provider_configuration(),
            frozen.configuration().effective_identity()
        );
        let answer = executor
            .block_on(
                sessions.compact(plakous::sessions::CompactSessionRequest::new(
                    SessionOperationId::new("compact-old:manual").unwrap(),
                    locator,
                    plakous::sessions::CompactReason::Manual,
                    compact,
                )),
            )
            .unwrap();
        if native {
            let error =
                answer.expect_err("non-dispatching native fixture has no Streaming support");
            assert!(
                error
                    .to_string()
                    .contains("compaction source measurement failed"),
                "{error}"
            );
        } else {
            assert!(matches!(
                answer.unwrap(),
                plakous::sessions::CompactSessionOutcome::NoEligibleRange { .. }
            ));
        }
        executor.block_on(source.release()).unwrap().unwrap();
        executor.block_on(current.release()).unwrap().unwrap();
    }
}
