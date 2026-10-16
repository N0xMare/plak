//! Attach-time host configuration: catalog eligibility, gated model resolution
//! and the multi-turn host worker spawn the TUI and the attached one-shot
//! commands share.

use std::path::Path;
use std::sync::Arc;
use std::thread;

use plakous::agent::ReasoningEffort;

use super::application::PlakousApplication;
use super::support::{
    application_state_home, native_processing_tier, persist_miss_native_tool_mode,
    product_subscription_request_limits,
};
use crate::home::paths as product_paths;
use crate::presentation::TuiNativeModelFact;
use crate::presentation::tui_presentation_bundle;
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous_hosted::agent as product_agent;
use plakous_hosted::runtime as product_host;

pub fn attached_target_accepts_effort(
    target: &product_config::ProductTargetSelection,
    ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
    account: &str,
    model: &str,
    effort_explicit: bool,
) -> bool {
    tui_attach_eligibility(
        target,
        ready_catalog,
        Some(account),
        Some(model),
        effort_explicit,
    ) == TuiAttachEligibility::Ready
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiAttachEligibility {
    Ready,
    EffortRequired,
    Unavailable,
}

pub fn tui_attach_eligibility(
    target: &product_config::ProductTargetSelection,
    ready_catalog: &plakous_host_protocol::views::ReadyCatalog,
    account: Option<&str>,
    model: Option<&str>,
    effort_explicit: bool,
) -> TuiAttachEligibility {
    let Some((account, model)) = account.zip(model) else {
        return TuiAttachEligibility::Unavailable;
    };
    // The repository gate's offline model carries its own sealed target
    // authority, so no Ready catalog lists it: it is eligible on an explicit
    // binding alone, as an injected attacher is, and then prepares on the same
    // deferred path a provider launch takes. Only the selector's presence
    // decides it; reading and checking the script stays with the attach.
    #[cfg(feature = "test-hooks")]
    if crate::test_hooks::requested() {
        return if effort_explicit {
            TuiAttachEligibility::Ready
        } else {
            TuiAttachEligibility::Unavailable
        };
    }
    match target.route(account) {
        Ok(plakous_hosted::binding::AttachRoute::OpenRouter { .. }) => {
            if model.trim().is_empty() {
                TuiAttachEligibility::Unavailable
            } else {
                TuiAttachEligibility::Ready
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
                Some(true) if !effort_explicit => TuiAttachEligibility::EffortRequired,
                Some(_) => TuiAttachEligibility::Ready,
                None => TuiAttachEligibility::Unavailable,
            }
        }
        Err(plakous_hosted::binding::RouteRefusal::AccountMismatch) => {
            TuiAttachEligibility::Unavailable
        }
    }
}

/// The shell reason a TUI attach that was not attempted reports. A native
/// account whose stored login cannot run is named by `credential_refusal`,
/// the CLI's words for it, rather than as a model missing from the catalog.
pub fn deferred_attach_refusal_reason(
    eligibility: TuiAttachEligibility,
    product_needs_setup: bool,
    credential_refusal: Option<String>,
) -> String {
    match eligibility {
        TuiAttachEligibility::Ready => "canonical attach was not attempted".to_owned(),
        TuiAttachEligibility::EffortRequired => "effort_required".to_owned(),
        TuiAttachEligibility::Unavailable if product_needs_setup => "missing_defaults".to_owned(),
        TuiAttachEligibility::Unavailable => credential_refusal.map_or_else(
            || {
                "resolve_failed: selected model is absent from the discovered Ready catalog"
                    .to_owned()
            },
            |refusal| credential_unusable_reason(&refusal),
        ),
    }
}

/// Shell reason for a stored login a model run would refuse.
pub fn credential_unusable_reason(refusal: &str) -> String {
    format!("credential_unusable: {}", refusal.replace('`', ""))
}

/// The CLI's refusal for the selected native account's stored login, read
/// offline; `None` for other accounts or a login that can run.
pub fn native_credential_refusal(account: Option<&str>) -> Option<String> {
    let provider = plakous_hosted::binding::oauth_provider_for_account(account?)?;
    let state_home = crate::home::paths::application_state_home().ok()?;
    let mut accounts = PlakousApplication::for_accounts(state_home).ok()?;
    let status = super::application::AccountOps::offline_status(&mut accounts, provider);
    let refusal = super::support::credential_refusal(provider, status)?;
    tracing::warn!(
        account = provider.as_str(),
        reason = %refusal,
        "TUI attach refused: stored subscription login cannot run"
    );
    Some(refusal)
}

/// Attempt to resolve a gated native model and spawn the multi-turn host worker.
///
/// Returns channels + join handle when ready; `Err(reason)` falls back to shell TUI
/// with an operator-visible attach failure reason.
pub type HostedAgentChannels = (
    crate::surface_protocol::UserActionSender,
    crate::surface_protocol::HostEventReceiver,
    thread::JoinHandle<crate::CliResult<()>>,
);

pub trait TuiAttacher {
    /// Whether this attacher can be constructed inside Plak's deferred controller.
    ///
    /// Test/custom attachers own injected runtime state and therefore retain the
    /// synchronous path unless they explicitly implement an equivalent handoff.
    fn supports_deferred_preparation(&self) -> bool {
        false
    }

    fn attach(
        &self,
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
    ) -> Result<HostedAgentChannels, String>;
}

pub struct NativeTuiAttacher;

impl TuiAttacher for NativeTuiAttacher {
    fn supports_deferred_preparation(&self) -> bool {
        true
    }

    fn attach(
        &self,
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
    ) -> Result<HostedAgentChannels, String> {
        try_spawn_hosted_agent(
            paths,
            state_roots,
            session,
            account,
            model_id,
            effort,
            effort_explicit,
            cwd,
            ready_catalog,
            resume_publication,
        )
    }
}

pub fn try_spawn_hosted_agent(
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
) -> Result<HostedAgentChannels, String> {
    let native_models = tui_presentation_bundle(paths)
        .map_err(|error| error.message().to_owned())?
        .native_models;
    let config = build_host_config(
        paths,
        state_roots,
        session,
        account,
        model_id,
        effort,
        effort_explicit,
        cwd,
        ready_catalog,
        resume_publication,
        &native_models,
    )?;
    product_host::spawn_host_worker(config).map_err(|failure| failure.message().to_owned())
}

pub fn build_host_config(
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
    native_models: &[TuiNativeModelFact],
) -> Result<product_host::HostConfig<crate::surface_protocol::PlakExtension>, String> {
    let policy = product_config::SessionPolicy::load(paths)?;
    // The repository gate's offline model, when this build carries the hook and
    // the process was started with one. It is resolved before any credential or
    // catalog authority is consulted, so a scripted lifecycle never touches a
    // provider, and a broken selector refuses the launch instead of silently
    // falling through to one.
    #[cfg(feature = "test-hooks")]
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
        );
    }
    let account = account
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no account bound — choose a profile with /agents".to_owned())?;
    let model_id = model_id
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "no model bound — choose a profile with /agents".to_owned())?;
    let state_home = application_state_home().map_err(|e| format!("state home: {e}"))?;
    let mut app = PlakousApplication::for_accounts(state_home)
        .map_err(|e| format!("plakous app: {}", e.message()))?;
    app.subscriptions = state_roots
        .native_subscriptions()
        .map_err(|e| format!("native subscriptions: {}", e.message()))?;
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
                            subscriptions: app.subscriptions.clone(),
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
                            subscriptions: app.subscriptions.clone(),
                        },
                        Some(effort),
                    )
                }
                plakous_hosted::binding::AttachRoute::NativeOauth { .. } => unreachable!(),
            };
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
                    ),
                session,
                target,
                model_id: model_id.to_owned(),
                effort: api_effort,
                effort_explicit: api_effort.is_some(),
                compaction_enabled: policy.compaction_enabled,
                max_model_rounds: policy.max_model_rounds,
                topology_enabled: policy.topology_enabled,
                auto_continue: policy.auto_continue,
                topology_admission: Some(product_config::topology_admission_probe(paths)),
                surface: Some(crate::shell::settings::ProductSurface::boxed(paths)),
                root_template: policy.root_template,
                attached_one_shot: false,
                interactive: true,
                cwd: cwd.to_path_buf(),
                ready_catalog: ready_catalog.clone(),
                resume_publication,
                test_injection: None,
                ready_path: product_host::ReadyPathTiming::default(),
                seed_history: None,
                source_releases: product_host::SourceReleases::default(),
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
        match app
            .executor
            .block_on(app.subscriptions.model_catalog_persisted(provider))
        {
            Ok(Ok(snapshot)) => {
                let entry = snapshot
                    .models()
                    .iter()
                    .find(|entry| entry.exact_id() == model_id)
                    .ok_or_else(|| {
                        format!(
                            "model `{model_id}` is not in the `{account}` catalog; run `plak models {account}` to list the models it offers"
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
            _ => persist_miss_native_tool_mode(account)?,
        }
    };
    if session.target_selection == product_config::ProductTargetSelection::NativePriority
        && !priority_admitted
    {
        return Err(format!(
            "Fast is absent from the authenticated `{model_id}` catalog row"
        ));
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
    let resolved = app
        .executor
        .block_on(
            app.subscriptions
                .resolve_turn_scoped_provider_for_attach(selected, limits),
        )
        .map_err(|e| format!("resolve {account}/{model_id}: {e:?}"))?
        .map_err(|e| {
            format!("resolve {account}/{model_id}: {e:?} — check `plak login {account}` and `plak models {account}`")
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
            ),
        session,
        target: product_host::HostTargetAuthority::Native {
            provider,
            resolved,
            subscriptions: app.subscriptions.clone(),
            catalog_candidates: native_models.to_vec().into(),
        },
        model_id: model_id.to_owned(),
        effort: effort_explicit.then_some(effort),
        effort_explicit,
        compaction_enabled: policy.compaction_enabled,
        max_model_rounds: policy.max_model_rounds,
        topology_enabled: policy.topology_enabled,
        auto_continue: policy.auto_continue,
        topology_admission: Some(product_config::topology_admission_probe(paths)),
        surface: Some(crate::shell::settings::ProductSurface::boxed(paths)),
        root_template: policy.root_template,
        attached_one_shot: false,
        interactive: true,
        cwd: cwd.to_path_buf(),
        ready_catalog: ready_catalog.clone(),
        resume_publication,
        test_injection: None,
        ready_path: product_host::ReadyPathTiming::default(),
        seed_history: None,
        source_releases: product_host::SourceReleases::default(),
    })
}
