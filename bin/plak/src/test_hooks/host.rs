//! Product host configuration over the selected offline transport.

use super::{
    Arc, AtomicBool, BoundaryController, CrashBoundary, NativeProvider, OfflineSelection, Path,
    PathBuf, ReasoningEffort, RootTools, SELECTOR, attach_product_children, prepare_scripted_tree,
    product_agent, product_child_binding, product_config, product_host, product_paths,
    product_sessions, profile_ready_catalog, provider_release, scripted_artifact_runtime_binding,
    scripted_code_mode_runtime_binding_with_extensions, scripted_compaction_runtime_binding,
    scripted_runtime_binding_with_extensions, scripted_runtime_binding_with_telemetry,
    scripted_topology_runtime_binding, seeded_create_cut, tree_registrations,
};

/// Which face is asking for the host configuration.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum OfflineFace {
    /// The attached multi-turn faces: the terminal application and `plak agent`.
    Attached,
    /// The single-turn `plak run` face.
    OneShot,
    /// The exact-reopen probe: the scripted tree is read, never saved or
    /// attached, as the product's verification compiles over a detached copy.
    Verify,
}

impl OfflineSelection {
    /// The complete injected host configuration for one face.
    ///
    /// Every field outside the injected target is read from the same product
    /// sources the real path reads, so what the gate exercises is the product's
    /// own composition with one substituted model.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "The offline host fixture mirrors production session, catalog, effort, workspace, and resume-publication inputs across its scripted routes."
    )]
    pub(crate) fn host_config(
        &self,
        face: OfflineFace,
        paths: &product_paths::PlakPaths,
        state_roots: &product_agent::ProductStateRoots,
        session: product_sessions::SessionRecord,
        effort: ReasoningEffort,
        effort_explicit: bool,
        cwd: &Path,
        ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
        resume_publication: Option<Arc<AtomicBool>>,
    ) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, crate::CliFailure>
    {
        if face != OfflineFace::Verify && session.plakous_locator.is_some() {
            crate::carry::upgrade::receipt::ensure_open(paths, &session)?;
        }
        let controller = if let Some(release) = provider_release() {
            BoundaryController::quiet_gated_from_evidence_path(
                CrashBoundary::ProviderDispatchOne,
                &self.evidence,
                PathBuf::from(release),
            )
        } else {
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &self.evidence)
        }
        .map_err(|error| format!("offline evidence log: {error}"))?;
        let workspace = cwd.to_path_buf();
        let mut profile_plan = (*self.plan).clone();
        profile_plan.profile_effort = Some(effort);
        let plan = Arc::new(profile_plan);
        let root_tools = Arc::new(RootTools::new(paths, state_roots, &session, face)?);
        // One session, one script position. The host may build more than one
        // runtime for a session, and a per-binding cursor would restart the
        // script at step one each time, which is silent and would look like the
        // model repeating itself.
        let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
        // One tree per selection, not per runtime: a child spawned through one
        // runtime has to be visible to the next one the host builds, and a
        // fresh tree would give each its own root.
        let prepared = if plan.tree {
            let mut prepared = prepare_scripted_tree(
                paths,
                cwd,
                &session.id,
                &plan,
                state_roots.session_records(),
                state_roots.clone(),
                face == OfflineFace::Verify,
            )?;
            attach_product_children(
                &mut prepared,
                &plan,
                &session.id,
                cwd,
                state_roots,
                &self.evidence,
                product_config::SessionPolicy::load(paths)?.resume_after_loss
                    == plakous_hosted::runtime::ResumeAfterLoss::Continue,
            )?;
            Some(prepared)
        } else {
            None
        };
        let child_runtime_hub = prepared
            .as_ref()
            .map(|prepared| Arc::clone(&prepared.runtime));
        // The rebind target is metadata read off a binding; it shares the same
        // cursor as every other runtime this selection builds, so constructing
        // it cannot advance or strand the script.
        let target = if let Some(prepared) = prepared.as_ref() {
            scripted_topology_runtime_binding(
                cwd,
                controller.clone(),
                Arc::clone(&plan),
                Arc::clone(&cursor),
                tree_registrations(prepared)?,
            )
        } else if plan.code_mode {
            let sidecar = crate::sidecars::js_host::locate().map_err(|reason| {
                format!("offline Code Mode needs the JavaScript sidecar: {reason}")
            })?;
            scripted_code_mode_runtime_binding_with_extensions(
                cwd,
                controller.clone(),
                Arc::clone(&plan),
                Arc::clone(&cursor),
                &sidecar,
                None,
                root_tools.registrations()?,
            )
        } else if plan.seal_family_artifacts {
            scripted_artifact_runtime_binding(cwd, controller.clone(), &plan)
        } else {
            scripted_runtime_binding_with_extensions(
                cwd,
                controller.clone(),
                Arc::clone(&plan),
                Arc::clone(&cursor),
                root_tools.registrations()?,
            )
        }
        .map_err(|error| error.to_string())?
        .target_binding();
        let request_slots = session
            .account
            .as_deref()
            .map(plakous_hosted::request_slots::for_account);
        let runtime_controller = controller;
        let runtime_tree = prepared;
        let telemetry = state_roots.telemetry_emitter();
        let transport_roots = state_roots.clone();
        let runtime: Arc<plakous_hosted::runtime::test_kit::ScriptedProfileRuntime> =
            Arc::new(move |role: product_host::RuntimeRole, profile_effort| {
                let mut profile_plan = (*plan).clone();
                profile_plan.profile_effort = profile_effort;
                let plan = Arc::new(profile_plan);
                if role == product_host::RuntimeRole::Compact {
                    let tree_tools = runtime_tree.as_ref().map(tree_registrations).transpose()?;
                    let sidecar = if plan.code_mode && tree_tools.is_none() {
                        Some(crate::sidecars::js_host::locate().map_err(|reason| {
                            format!("offline Code Mode needs the JavaScript sidecar: {reason}")
                        })?)
                    } else {
                        None
                    };
                    scripted_compaction_runtime_binding(
                        &workspace,
                        runtime_controller.clone(),
                        &plan,
                        tree_tools,
                        sidecar.as_deref(),
                        if runtime_tree.is_some() || plan.seal_family_artifacts {
                            Vec::new()
                        } else {
                            root_tools.registrations()?
                        },
                    )
                } else if let Some(prepared) = runtime_tree.as_ref() {
                    // The same driver every time, so a child spawned on one runtime
                    // is still on the tree the next one compiles against.
                    scripted_topology_runtime_binding(
                        &workspace,
                        runtime_controller.clone(),
                        Arc::clone(&plan),
                        Arc::clone(&cursor),
                        tree_registrations(prepared)?,
                    )
                } else if plan.code_mode {
                    // The product's own sidecar discovery, so the test binds what
                    // the product would bind rather than a path of its own.
                    match crate::sidecars::js_host::locate() {
                        Ok(sidecar) => scripted_code_mode_runtime_binding_with_extensions(
                            &workspace,
                            runtime_controller.clone(),
                            Arc::clone(&plan),
                            Arc::clone(&cursor),
                            &sidecar,
                            telemetry.clone(),
                            root_tools.registrations()?,
                        ),
                        Err(reason) => {
                            return Err(format!(
                                "offline Code Mode needs the JavaScript sidecar: {reason}"
                            ));
                        }
                    }
                } else if plan.seal_family_artifacts {
                    scripted_artifact_runtime_binding(&workspace, runtime_controller.clone(), &plan)
                } else {
                    scripted_runtime_binding_with_telemetry(
                        &workspace,
                        runtime_controller.clone(),
                        Arc::clone(&plan),
                        Arc::clone(&cursor),
                        root_tools.registrations()?,
                        telemetry.clone(),
                    )
                }
                .map(|runtime| {
                    let runtime = transport_roots.bind_process_transport(runtime);
                    match &request_slots {
                        Some(slots) => runtime.with_request_slots(slots.clone()),
                        None => runtime,
                    }
                })
                .map_err(|error| error.to_string())
            });
        let primary = Arc::clone(&runtime);
        let injection = Arc::new(product_host::ScriptedRuntimeFactory {
            runtime: Arc::new(move |role| primary(role, Some(effort))),
            rebind_target: target,
            observe: Arc::new(seeded_create_cut(self.evidence.with_file_name("carry-cut"))),
            child_runtime: self
                .plan
                .product_child_script
                .clone()
                .zip(child_runtime_hub)
                .map(|(script, runtime)| {
                    product_child_binding(
                        script,
                        self.evidence.with_file_name("child-evidence.jsonl"),
                        runtime,
                    )
                }),
            profile_route: Some(
                plakous_hosted::runtime::test_kit::ScriptedProfileRoute::new(
                    "codex".to_owned(),
                    SELECTOR.to_owned(),
                    runtime,
                ),
            ),
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let policy = product_config::SessionPolicy::load(paths)?;
        let lease_holder = Some(crate::store::holders::lease_holder(paths, &session.id));
        Ok(product_host::HostConfig {
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: state_roots.session_records(),
            state_roots: state_roots.clone(),
            session,
            target: plakous_hosted::runtime::test_kit::registered_authority(NativeProvider::Codex),
            model_id: SELECTOR.to_owned(),
            effort: Some(effort),
            effort_explicit,
            compaction_enabled: policy.compaction_enabled,
            max_model_rounds: policy.max_model_rounds,
            topology_enabled: policy.topology_enabled,
            auto_continue: policy.auto_continue,
            resume_after_loss: policy.resume_after_loss,
            topology_admission: Some(product_config::topology_admission_probe(paths)),
            surface: match face {
                OfflineFace::Attached => Some(crate::shell::settings::ProductSurface::boxed(paths)),
                OfflineFace::OneShot | OfflineFace::Verify => None,
            },
            root_template: policy.root_template,
            attached_one_shot: face != OfflineFace::Attached,
            interactive: face == OfflineFace::Attached,
            cwd: cwd.to_path_buf(),
            ready_catalog: profile_ready_catalog(ready_catalog),
            resume_publication,
            runtime_factory: injection,
            ready_path: product_host::ReadyPathTiming::default(),
            seed_history: None,
            seed_line: None,
            source_releases: product_host::SourceReleases::default(),
            idle_release: None,
            lease_holder,
            continuation: (face == OfflineFace::Attached)
                .then(crate::reload_request::take_reload_continuation)
                .flatten(),
            event_renderer: None,
        })
    }
}
