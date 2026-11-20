//! Attach-time host configuration: catalog eligibility, gated model resolution
//! shared by the server and attached one-shot commands.

use std::path::Path;
use std::sync::Arc;

use plakous::agent::ReasoningEffort;

use super::support::{
    application_execution_failure, native_processing_tier, persist_miss_native_tool_mode,
    product_subscription_request_limits,
};
use crate::home::paths as product_paths;
use crate::presentation::HostLaunchNativeModelFact;
use crate::presentation::catalog_bundle;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::agent as product_agent;
use plakous_hosted::runtime as product_host;

pub(super) fn binding_presentation(
    paths: &product_paths::PlakPaths,
    custody: &crate::gateway::HostCustody,
    account: &plakous_hosted::account::AccountRef,
) -> crate::CliResult<crate::presentation::HostCatalogBundle> {
    if account.is_named() {
        return Ok(crate::presentation::HostCatalogBundle {
            providers: Vec::new(),
            sections: plakous_host_protocol::views::ReadyCatalog::default(),
            native_models: Vec::new(),
        });
    }
    catalog_bundle(paths, custody)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachEligibility {
    Ready,
    EffortRequired,
    Unavailable,
}

pub fn attach_eligibility(
    target: &product_config::ProductTargetSelection,
    ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
    account: Option<&str>,
    model: Option<&str>,
    effort_explicit: bool,
) -> AttachEligibility {
    let Some((account, model)) = account.zip(model) else {
        return AttachEligibility::Unavailable;
    };
    // The repository gate's offline model carries its own sealed target
    // authority, so no Ready catalog lists it: it is eligible on an explicit
    // binding alone, as an injected attacher is, and then prepares on the same
    // deferred path a provider launch takes. Only the selector's presence
    // decides it; reading and checking the script stays with the attach.
    #[cfg(feature = "test-support")]
    if crate::test_hooks::requested() {
        return if effort_explicit {
            AttachEligibility::Ready
        } else {
            AttachEligibility::Unavailable
        };
    }
    match target.route(account) {
        Ok(plakous_hosted::binding::AttachRoute::OpenRouter { .. }) => {
            if model.trim().is_empty() {
                AttachEligibility::Unavailable
            } else {
                AttachEligibility::Ready
            }
        }
        // A native target on a non-OAuth account still asks the Ready catalog:
        // eligibility is a catalog question, and the route refusal is the
        // attach's to report.
        Ok(plakous_hosted::binding::AttachRoute::Platform { .. })
        | Ok(plakous_hosted::binding::AttachRoute::NativeOauth { .. })
        | Err(plakous_hosted::binding::RouteRefusal::NotOauth) => {
            match product_host::ready_catalog_model_needs_effort(
                &ready_catalog.accounts,
                account,
                model,
            ) {
                Some(true) if !effort_explicit => AttachEligibility::EffortRequired,
                Some(_) => AttachEligibility::Ready,
                None => AttachEligibility::Unavailable,
            }
        }
        Err(plakous_hosted::binding::RouteRefusal::AccountMismatch) => {
            AttachEligibility::Unavailable
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "Host construction binds the recorded session, catalog facts, explicit effort, workspace, and publication fence in admission order."
)]
pub(crate) fn build_host_config(
    paths: &product_paths::PlakPaths,
    state_roots: &product_agent::ProductStateRoots,
    session: product_sessions::SessionRecord,
    account: Option<&str>,
    model_id: Option<&str>,
    effort: ReasoningEffort,
    effort_explicit: bool,
    cwd: &Path,
    ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
    resume_publication: Option<Arc<std::sync::atomic::AtomicBool>>,
    native_models: &[HostLaunchNativeModelFact],
    custody: &crate::gateway::HostCustody,
) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, crate::CliFailure> {
    if session.plakous_locator.is_some() {
        crate::carry::upgrade::receipt::ensure_open(paths, &session)?;
    }
    let mut policy = product_config::SessionPolicy::load(paths)?;
    if let Some(profile) = &session.root_profile {
        let config = product_config::ProductConfig::load(paths).map_err(crate::CliFailure::new)?;
        let selected = crate::profiles::selection_config(
            &config,
            &crate::profiles::SessionSelection {
                root: profile.root_template.clone(),
                working_set: profile.working_set.clone(),
            },
        )
        .map_err(crate::CliFailure::new)?;
        policy.root_template = selected.root_template_fact().map(|mut fact| {
            if let Some(account) = &session.account {
                fact.account.clone_from(account);
            }
            if let Some(model) = &session.model {
                fact.model.clone_from(model);
            }
            fact
        });
    }
    policy.install_request_budget();
    let lease_holder = Some(crate::store::holders::lease_holder(paths, &session.id));
    // The repository gate's offline model, when this build carries the hook and
    // the process was started with one. It is resolved before any credential or
    // catalog authority is consulted, so a scripted lifecycle never touches a
    // provider, and a broken selector refuses the launch instead of silently
    // falling through to one.
    #[cfg(feature = "test-support")]
    if let Some(selection) = crate::test_hooks::selected() {
        return selection?.host_config(
            crate::test_hooks::OfflineFace::Attached,
            paths,
            state_roots,
            session,
            effort,
            effort_explicit,
            cwd,
            ready_catalog,
            resume_publication,
            custody,
        );
    }
    let account = account
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no account bound — choose a profile with /agents".to_owned())?;
    let model_id = model_id
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no model bound — choose a profile with /agents".to_owned())?;
    let executor = plakous::application::ApplicationExecutor::new()
        .map_err(|_| "application executor construction failed".to_owned())?;
    let selected_account: plakous_hosted::account::AccountRef = account.parse()?;
    let subscriptions = if selected_account.vertical().is_api() {
        state_roots.native_subscriptions()
    } else {
        state_roots.native_subscriptions_for(&selected_account)
    }
    .map_err(|e| format!("native subscriptions: {}", e.message()))?
    .with_gateway_catalog_cache(paths.home());
    let route = session
        .target_selection
        .route(account)
        .map_err(|refusal| refusal.message(account))?;
    let provider = match route {
        route @ (plakous_hosted::binding::AttachRoute::OpenRouter { .. }
        | plakous_hosted::binding::AttachRoute::Platform { .. }) => {
            let (target, api_effort) = match route {
                plakous_hosted::binding::AttachRoute::OpenRouter { wire, .. } => {
                    wire.validate()?;
                    (
                        product_host::HostTargetAuthority::OpenRouter {
                            subscriptions: subscriptions.clone(),
                            evidence: None,
                        },
                        product_host::effort_from_record(session.effort.as_deref())
                            .map_err(|error| error.message().to_owned())?,
                    )
                }
                plakous_hosted::binding::AttachRoute::Platform { provider } => {
                    crate::platform_catalog::validate(
                        provider,
                        model_id,
                        effort_explicit.then_some(effort),
                        session.composition,
                    )?;
                    (
                        product_host::HostTargetAuthority::Platform {
                            provider,
                            subscriptions: subscriptions.clone(),
                        },
                        Some(effort),
                    )
                }
                plakous_hosted::binding::AttachRoute::NativeOauth { .. } => unreachable!(),
            };
            executor
                .block_on(custody.require_api_key(account))
                .map_err(application_execution_failure)??;
            return Ok(product_host::HostConfig {
                product_home: paths.home().to_path_buf(),
                home: paths.session_home(),
                store: state_roots.session_records(),
                state_roots: state_roots
                    .clone()
                    .with_job_config(
                        plakous::tools::JobConfig::new(
                            u64::from(policy.jobs_foreground_seconds),
                            u64::from(policy.jobs_max_runtime_hours),
                        )
                        .map_err(|error| error.to_string())?,
                    )
                    .with_api_models(
                        plakous_hosted::platform::ApiModelLists::Live,
                        policy.api_window(),
                    )
                    .with_skills(policy.skills()),
                session,
                target,
                model_id: model_id.to_owned(),
                effort: api_effort,
                effort_explicit: api_effort.is_some(),
                compaction_enabled: policy.compaction_enabled,
                max_model_rounds: policy.max_model_rounds,
                topology_enabled: policy.topology_enabled,
                auto_continue: policy.auto_continue,
                resume_after_loss: policy.resume_after_loss,
                topology_admission: Some(product_config::topology_admission_probe(paths)),
                surface: Some(crate::shell::settings::ProductSurface::boxed(
                    paths,
                    custody.clone(),
                )),
                root_template: policy.root_template,
                attached_one_shot: false,
                interactive: true,
                cwd: cwd.to_path_buf(),
                ready_catalog: ready_catalog.clone(),
                resume_publication,
                runtime_factory: Arc::new(plakous_hosted::runtime::ProductionRuntimeFactory),
                ready_path: product_host::ReadyPathTiming::default(),
                seed_history: None,
                seed_line: None,
                source_releases: product_host::SourceReleases::default(),
                idle_release: None,
                lease_holder: lease_holder.clone(),
                continuation: None,
                event_renderer: Some(crate::log_text::render),
            });
        }
        plakous_hosted::binding::AttachRoute::NativeOauth { provider, .. } => provider,
    };

    let model = plakous::model::ModelId::new(model_id.to_owned())
        .map_err(|e| format!("model selector `{model_id}`: {e:?}"))?;
    let selected_fact = native_models
        .iter()
        .find(|fact| fact.account == account && fact.exact_id == model_id);
    let (native_tool_mode, priority_admitted) = if let Some(fact) = selected_fact {
        (
            fact.native_tool_mode,
            fact.service_tiers.iter().any(|tier| tier.0 == "priority"),
        )
    } else {
        match executor.block_on(subscriptions.model_catalog_persisted(provider)) {
            Ok(Ok(snapshot)) => {
                let entry = snapshot
                    .models()
                    .iter()
                    .find(|entry| entry.exact_id() == model_id)
                    .ok_or_else(|| {
                        format!(
                            "model `{model_id}` is not in the `{account}` catalog; run `plakd models {account}` to list the models it offers"
                        )
                    })?;
                (
                    entry.native_tool_mode(),
                    entry
                        .service_tiers()
                        .iter()
                        .any(|tier| tier.id() == "priority"),
                )
            }
            Ok(Err(plakous::subscriptions::NativeSubscriptionError::ModelUnavailable))
                if custody.gateway().is_some()
                    && provider == plakous::subscriptions::NativeProvider::Codex =>
            {
                return Err(super::support::gateway_catalog_remedy(account).into());
            }
            Ok(Err(error))
                if custody.gateway().is_some()
                    && provider == plakous::subscriptions::NativeProvider::Codex =>
            {
                return Err(super::support::native_subscription_failure(
                    error,
                    Some(provider),
                    paths.home(),
                ));
            }
            _ => persist_miss_native_tool_mode(account)?,
        }
    };
    if session.target_selection == product_config::ProductTargetSelection::NativePriority
        && !priority_admitted
    {
        return Err(
            format!("Fast is absent from the authenticated `{model_id}` catalog row").into(),
        );
    }
    let selected = product_agent::selected_coding_provider_configuration_with_processing_tier(
        provider,
        model,
        effort_explicit.then_some(effort),
        native_tool_mode,
        session.composition,
        native_processing_tier(&session.target_selection)
            .map_err(|error| error.message().to_owned())?,
    )
    .map_err(|e| format!("provider configuration `{model_id}`: {}", e.message()))?;
    let limits = product_subscription_request_limits()
        .map_err(|()| "product gate limits invalid".to_owned())?;
    let resolved = executor
        .block_on(
            subscriptions
                .resolve_turn_scoped_provider_for_attach(selected, limits),
        )
        .map_err(|e| format!("resolve {account}/{model_id}: {e:?}"))?
        .map_err(|e| {
            format!("resolve {account}/{model_id}: {e} — check `plakd login {account}` and `plakd models {account}`")
        })?;

    let resolved = Arc::new(resolved);
    Ok(product_host::HostConfig {
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: state_roots.session_records(),
        state_roots: state_roots
            .clone()
            .with_job_config(
                plakous::tools::JobConfig::new(
                    u64::from(policy.jobs_foreground_seconds),
                    u64::from(policy.jobs_max_runtime_hours),
                )
                .map_err(|error| error.to_string())?,
            )
            .with_api_models(
                plakous_hosted::platform::ApiModelLists::Live,
                policy.api_window(),
            )
            .with_skills(policy.skills()),
        session,
        target: product_host::HostTargetAuthority::Native {
            provider,
            resolved,
            subscriptions: subscriptions.clone(),
            catalog_candidates: native_models.to_vec().into(),
        },
        model_id: model_id.to_owned(),
        effort: effort_explicit.then_some(effort),
        effort_explicit,
        compaction_enabled: policy.compaction_enabled,
        max_model_rounds: policy.max_model_rounds,
        topology_enabled: policy.topology_enabled,
        auto_continue: policy.auto_continue,
        resume_after_loss: policy.resume_after_loss,
        topology_admission: Some(product_config::topology_admission_probe(paths)),
        surface: Some(crate::shell::settings::ProductSurface::boxed(
            paths,
            custody.clone(),
        )),
        root_template: policy.root_template,
        attached_one_shot: false,
        interactive: true,
        cwd: cwd.to_path_buf(),
        ready_catalog: ready_catalog.clone(),
        resume_publication,
        runtime_factory: Arc::new(plakous_hosted::runtime::ProductionRuntimeFactory),
        ready_path: product_host::ReadyPathTiming::default(),
        seed_history: None,
        seed_line: None,
        source_releases: product_host::SourceReleases::default(),
        idle_release: None,
        lease_holder,
        continuation: None,
        event_renderer: Some(crate::log_text::render),
    })
}
