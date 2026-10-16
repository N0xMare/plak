//! One-shot CLI preparation and presentation over the hosted session.

use super::application::{PlakousApplication, catalog_summaries};
use super::session::load_exact_product_session;
use super::support::{
    application_execution_failure, cli_provider_name, native_processing_tier,
    product_subscription_request_limits, require_ready, run_composition,
};
use crate::cli_output::{TerminalMode, safe_terminal_block};
use crate::product_config;
use crate::store::sessions as product_sessions;
use plakous::agent::ReasoningEffort;
use plakous::application::ApplicationExecutor;
use plakous::subscriptions::{NativeProvider, NativeSubscriptionError};
use plakous_hosted::agent as product_agent;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};
use plakous_hosted::runtime as product_host;
use std::env;
use std::{io, sync::Arc};

impl PlakousApplication {
    pub(crate) fn run(
        &mut self,
        config: &product_config::ProductConfig,
        provider: NativeProvider,
        model: &str,
        effort: Option<ReasoningEffort>,
        composition: Option<product_config::ProductCompositionSelection>,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        stdout: &mut dyn io::Write,
        stderr: &mut dyn io::Write,
        limits: product_host::RunBounds,
    ) -> CliResult<ProcessOutcome> {
        if run_expired(limits) {
            return run_timeout(stderr);
        }
        let configured = config.default_binding();
        let composition = run_composition(composition, &configured, provider, model);
        // The repository gate's offline model; see `crate::test_hooks`. A
        // scripted lifecycle binds the requested tuple without a credential
        // refresh or a catalog row, because there is no provider to admit.
        #[cfg(feature = "test-hooks")]
        if crate::test_hooks::selected().is_some() {
            let cwd = env::current_dir()
                .map_err(|_| CliFailure::new("working directory is unavailable"))?;
            let mut session = product_sessions::SessionRecord::new(cwd);
            session.account = Some(cli_provider_name(provider).to_owned());
            session.model = Some(model.to_owned());
            session.effort = effort.map(product_host::effort_label);
            session.composition = composition;
            session.target_selection = product_config::ProductTargetSelection::Native;
            return self.run_hosted(
                config,
                session,
                product_host::HostTargetAuthority::Injected { provider },
                prompt,
                print_session_id,
                false,
                stdout,
                stderr,
                limits,
            );
        }
        let priority_requested =
            provider == NativeProvider::Codex && configured.prefers_priority("codex", model);
        // Product path: refresh, then fail-closed catalog revalidation.
        // Catalog discovery uses a 0-AuthRefresh gate, so an expired access
        // token with durable Ready status must be rotated here first.
        let Some(refreshed) =
            run_preparation(&self.executor, limits, self.subscriptions.refresh(provider))?
        else {
            return run_timeout(stderr);
        };
        let refreshed = self.refreshed_credential(provider, refreshed)?;
        require_ready(provider, refreshed)?;
        let model = plakous::model::ModelId::new(model.to_owned()).map_err(|_| {
            CliFailure::classified(CliFailureKind::Usage, "run model id is invalid")
        })?;
        let Some(catalog) = run_preparation(
            &self.executor,
            limits,
            self.subscriptions.model_catalog(provider),
        )?
        else {
            return run_timeout(stderr);
        };
        let catalog = catalog.map_err(|error| self.subscription_failure(error, Some(provider)))?;
        let catalog_models = catalog_summaries(provider, &catalog)?;
        let account = cli_provider_name(provider);
        let catalog_entry = catalog_models
            .iter()
            .find(|entry| entry.exact_id == model.as_str())
            .ok_or_else(|| {
                CliFailure::classified(
                    CliFailureKind::Usage,
                    format!(
                        "model `{model}` is absent from the authenticated `{account}` catalog; run `plak models {account}` to list the models it offers"
                    ),
                )
            })?;
        let selected_effort = if catalog_entry.reasoning_efforts.is_empty() {
            if effort.is_some() {
                return Err(CliFailure::classified(
                    CliFailureKind::Usage,
                    format!("model `{model}` does not admit a reasoning effort"),
                ));
            }
            None
        } else {
            let effort = effort.ok_or_else(|| {
                CliFailure::classified(
                    CliFailureKind::Usage,
                    format!(
                        "model `{model}` is a reasoning model and needs an explicit effort: --effort <{}>",
                        plakous_hosted::effort::levels_pipe_list()
                    ),
                )
            })?;
            if !catalog_entry.reasoning_efforts.contains(&effort) {
                return Err(CliFailure::classified(
                    CliFailureKind::Usage,
                    "effort is absent from the authenticated model catalog",
                ));
            }
            Some(effort)
        };
        if priority_requested
            && !catalog_entry
                .service_tiers
                .iter()
                .any(|(id, _, _)| id == "priority")
        {
            return Err(CliFailure::classified(
                CliFailureKind::Usage,
                "Fast is absent from the authenticated run model row",
            ));
        }
        let native_tool_mode = catalog_entry.native_tool_mode;
        let target_selection = if priority_requested {
            product_config::ProductTargetSelection::NativePriority
        } else {
            product_config::ProductTargetSelection::Native
        };
        let selected = product_agent::selected_coding_provider_configuration_with_processing_tier(
            provider,
            model,
            selected_effort,
            native_tool_mode,
            composition,
            native_processing_tier(&target_selection)?,
        )
        .map_err(|failure| failure.or_kind(CliFailureKind::Usage))?;
        // Product coding turns need real inference allowance; catalog-only gates may use zero.
        let request_limits = product_subscription_request_limits()
            .map_err(|()| CliFailure::new("subscription request limits invalid"))?;
        let Some(resolved) = run_preparation(
            &self.executor,
            limits,
            self.subscriptions.resolve_turn_scoped_provider(
                selected,
                request_limits,
                plakous::model::SessionLineage::Root,
            ),
        )?
        else {
            return run_timeout(stderr);
        };
        let resolved = resolved.map_err(|error| self.resolution_failure(error.kind(), provider))?;
        let resolved = Arc::new(resolved);
        let cwd =
            env::current_dir().map_err(|_| CliFailure::new("working directory is unavailable"))?;
        let mut session = product_sessions::SessionRecord::new(cwd);
        session.account = Some(cli_provider_name(provider).to_owned());
        session.model = Some(resolved.selected_model().as_str().to_owned());
        session.effort = selected_effort.map(product_host::effort_label);
        session.composition = composition;
        session.target_selection = target_selection;
        let target = product_host::HostTargetAuthority::Native {
            provider,
            resolved,
            subscriptions: self.subscriptions.clone(),
            catalog_candidates: Arc::from([]),
        };
        self.run_hosted(
            config,
            session,
            target,
            prompt,
            print_session_id,
            false,
            stdout,
            stderr,
            limits,
        )
    }

    /// A refused turn-scoped resolution. A model, effort or composition the
    /// provider does not offer is a usage error, like a model absent from the
    /// catalog; every other failure keeps its own class.
    fn resolution_failure(
        &self,
        error: NativeSubscriptionError,
        provider: NativeProvider,
    ) -> CliFailure {
        let selection = matches!(
            error,
            NativeSubscriptionError::ModelUnavailable
                | NativeSubscriptionError::ProviderConfigurationRejected
                | NativeSubscriptionError::ProviderConfigurationPolicyMismatch(_)
                | NativeSubscriptionError::ProviderConfigurationCompileRejected(_)
        );
        let failure = self.subscription_failure(error, Some(provider));
        if selection {
            failure.or_kind(CliFailureKind::Usage)
        } else {
            failure
        }
    }

    pub(crate) fn run_api(
        &mut self,
        config: &product_config::ProductConfig,
        model: &str,
        effort: Option<ReasoningEffort>,
        composition: product_config::ProductCompositionSelection,
        selection: product_config::ProductTargetSelection,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        stdout: &mut dyn io::Write,
        stderr: &mut dyn io::Write,
        limits: product_host::RunBounds,
    ) -> CliResult<ProcessOutcome> {
        if run_expired(limits) {
            return run_timeout(stderr);
        }
        let api = selection
            .api_target()
            .ok_or_else(|| CliFailure::new("API run requires an API target"))?;
        let cwd =
            env::current_dir().map_err(|_| CliFailure::new("working directory is unavailable"))?;
        let session = api_run_record(cwd, model, effort, composition, selection)?;
        self.product_paths()?
            .ensure_layout()
            .map_err(CliFailure::new)?;
        let target = match api {
            plakous_hosted::binding::ApiTarget::OpenRouter { .. } => {
                product_host::HostTargetAuthority::OpenRouter {
                    subscriptions: self.subscriptions.clone(),
                    evidence: None,
                }
            }
            plakous_hosted::binding::ApiTarget::Platform { provider } => {
                product_host::HostTargetAuthority::Platform {
                    provider,
                    subscriptions: self.subscriptions.clone(),
                }
            }
        };
        self.run_hosted(
            config,
            session,
            target,
            prompt,
            print_session_id,
            false,
            stdout,
            stderr,
            limits,
        )
    }

    pub(crate) fn continue_session(
        &mut self,
        config: &product_config::ProductConfig,
        session_id: &str,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        resume_tree: bool,
        stdout: &mut dyn io::Write,
        stderr: &mut dyn io::Write,
        limits: product_host::RunBounds,
    ) -> CliResult<ProcessOutcome> {
        if run_expired(limits) {
            return run_timeout(stderr);
        }
        let session = load_exact_product_session(self.session_records()?.as_ref(), session_id)?;
        // The repository gate's offline model; see `crate::test_hooks`. A
        // resume reopens the persisted record on the scripted binding.
        #[cfg(feature = "test-hooks")]
        if crate::test_hooks::selected().is_some() {
            return self.run_hosted(
                config,
                session,
                product_host::HostTargetAuthority::Injected {
                    provider: NativeProvider::Codex,
                },
                prompt,
                print_session_id,
                resume_tree,
                stdout,
                stderr,
                limits,
            );
        }
        if session.plakous_locator.is_none() {
            return Err(CliFailure::new(
                "session has no exact canonical Plakous locator",
            ));
        }
        if let Some(api) = session.target_selection.api_target() {
            let account = session
                .account
                .as_deref()
                .ok_or_else(|| CliFailure::new("session account binding is absent"))?;
            session
                .target_selection
                .route(account)
                .map_err(|refusal| CliFailure::new(refusal.message(account)))?;
            let target = match api {
                plakous_hosted::binding::ApiTarget::OpenRouter { .. } => {
                    product_host::HostTargetAuthority::OpenRouter {
                        subscriptions: self.subscriptions.clone(),
                        evidence: None,
                    }
                }
                plakous_hosted::binding::ApiTarget::Platform { provider } => {
                    product_host::HostTargetAuthority::Platform {
                        provider,
                        subscriptions: self.subscriptions.clone(),
                    }
                }
            };
            return self.run_hosted(
                config,
                session,
                target,
                prompt,
                print_session_id,
                resume_tree,
                stdout,
                stderr,
                limits,
            );
        }
        let account = session
            .account
            .clone()
            .ok_or_else(|| CliFailure::new("session has no exact account binding"))?;
        let provider = product_host::oauth_provider_for_account(&account).ok_or_else(|| {
            CliFailure::new(format!(
                "session account `{account}` is not an OAuth coding account"
            ))
        })?;
        let model_id = session
            .model
            .clone()
            .ok_or_else(|| CliFailure::new("session has no exact model binding"))?;
        let (effort, effort_explicit) = product_host::effort_from_config(session.effort.as_deref());
        let model = plakous::model::ModelId::new(model_id.clone()).map_err(|_| {
            CliFailure::classified(CliFailureKind::Usage, "session model id is invalid")
        })?;
        let Some(catalog) = run_preparation(
            &self.executor,
            limits,
            self.subscriptions.model_catalog(provider),
        )?
        else {
            return run_timeout(stderr);
        };
        let catalog = catalog.map_err(|error| self.subscription_failure(error, Some(provider)))?;
        let catalog_models = catalog_summaries(provider, &catalog)?;
        let catalog_entry = catalog_models
            .iter()
            .find(|entry| entry.exact_id == model.as_str())
            .ok_or_else(|| {
                CliFailure::classified(
                    CliFailureKind::Usage,
                    "model is absent from the authenticated native catalog",
                )
            })?;
        if catalog_entry.reasoning_efforts.is_empty() {
            if effort_explicit {
                return Err(CliFailure::new(
                    "persisted effort contradicts the authenticated model catalog",
                ));
            }
        } else if !effort_explicit || !catalog_entry.reasoning_efforts.contains(&effort) {
            return Err(CliFailure::new(
                "session has no exact admitted effort binding",
            ));
        }
        if session.target_selection == product_config::ProductTargetSelection::NativePriority
            && !catalog_entry
                .service_tiers
                .iter()
                .any(|(id, _, _)| id == "priority")
        {
            return Err(CliFailure::new(
                "Fast is absent from the authenticated session model row",
            ));
        }
        let native_tool_mode = catalog_entry.native_tool_mode;
        let selected = product_agent::selected_coding_provider_configuration_with_processing_tier(
            provider,
            model,
            effort_explicit.then_some(effort),
            native_tool_mode,
            session.composition,
            native_processing_tier(&session.target_selection)?,
        )
        .map_err(|failure| failure.or_kind(CliFailureKind::Usage))?;
        let request_limits = product_subscription_request_limits()
            .map_err(|()| CliFailure::new("subscription request limits invalid"))?;
        let Some(resolved) = run_preparation(
            &self.executor,
            limits,
            self.subscriptions.resolve_turn_scoped_provider(
                selected,
                request_limits,
                plakous::model::SessionLineage::Root,
            ),
        )?
        else {
            return run_timeout(stderr);
        };
        let resolved = resolved.map_err(|error| self.resolution_failure(error.kind(), provider))?;
        let target = product_host::HostTargetAuthority::Native {
            provider,
            resolved: Arc::new(resolved),
            subscriptions: self.subscriptions.clone(),
            catalog_candidates: Arc::from([]),
        };
        self.run_hosted(
            config,
            session,
            target,
            prompt,
            print_session_id,
            resume_tree,
            stdout,
            stderr,
            limits,
        )
    }

    fn run_hosted(
        &self,
        admitted: &product_config::ProductConfig,
        session: product_sessions::SessionRecord,
        target: product_host::HostTargetAuthority,
        prompt: plakous_host_protocol::Draft,
        print_session_id: bool,
        resume_tree: bool,
        stdout: &mut dyn io::Write,
        stderr: &mut dyn io::Write,
        limits: product_host::RunBounds,
    ) -> CliResult<ProcessOutcome> {
        let paths = self.product_paths()?;
        paths.ensure_layout().map_err(CliFailure::new)?;
        let roots = self.state_roots()?;
        let tree_id = crate::topology_product::durable_topology_tree_id(&roots, &session);
        let resume_tree =
            crate::topology_product::requested_tree_resume(paths, &tree_id, resume_tree)?;
        if !resume_tree
            && product_host::requires_explicit_resume(
                &product_host::HostRows::new(&roots, self.session_records()?.as_ref()),
                &tree_id,
            )
        {
            return Err(CliFailure::new(product_agent::RESTORED_PAUSED_RESUME_HINT));
        }
        let policy = admitted.session_policy();
        let child_agents = face::child_agents(admitted, paths);
        let (effort, explicit) = product_host::effort_from_config(session.effort.as_deref());
        // The repository gate's offline model; see `crate::test_hooks`. The
        // one-shot face builds its own configuration, so it carries the hook
        // itself rather than reaching through the attached builder.
        #[cfg(feature = "test-hooks")]
        if let Some(selection) = crate::test_hooks::selected() {
            let cwd = env::current_dir()
                .map_err(|_| CliFailure::new("working directory is unavailable"))?;
            let config = selection
                .and_then(|selection| {
                    selection.host_config(
                        crate::test_hooks::OfflineFace::OneShot,
                        paths,
                        &roots,
                        session,
                        effort,
                        explicit,
                        &cwd,
                        &plakous_host_protocol::views::ReadyCatalog::default(),
                        None,
                    )
                })
                .map_err(CliFailure::new)?;
            return run_config(
                &self.executor,
                config,
                prompt,
                print_session_id,
                resume_tree,
                child_agents,
                TerminalMode::of_stdout(),
                stdout,
                stderr,
                limits,
            );
        }
        let config = product_host::HostConfig {
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: self.session_records()?,
            state_roots: roots,
            model_id: session
                .model
                .clone()
                .ok_or_else(|| CliFailure::new("session has no exact model binding"))?,
            effort: explicit.then_some(effort),
            effort_explicit: explicit,
            target,
            session,
            compaction_enabled: policy.compaction_enabled,
            max_model_rounds: policy.max_model_rounds,
            topology_enabled: policy.topology_enabled,
            auto_continue: policy.auto_continue,
            topology_admission: Some(product_config::topology_admission_probe(paths)),
            root_template: policy.root_template,
            attached_one_shot: true,
            interactive: false,
            cwd: env::current_dir()
                .map_err(|_| CliFailure::new("working directory is unavailable"))?,
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
            surface: None,
            resume_publication: None,
            test_injection: None,
            ready_path: product_host::ReadyPathTiming::default(),
            seed_history: None,
            source_releases: product_host::SourceReleases::default(),
        };
        run_config(
            &self.executor,
            config,
            prompt,
            print_session_id,
            resume_tree,
            child_agents,
            TerminalMode::of_stdout(),
            stdout,
            stderr,
            limits,
        )
    }
}

fn run_expired(limits: product_host::RunBounds) -> bool {
    limits
        .deadline
        .is_some_and(|deadline| std::time::Instant::now() >= deadline)
}

fn run_timeout(stderr: &mut dyn io::Write) -> CliResult<ProcessOutcome> {
    write_stderr_line(stderr, "plak: run stopped at timeout")
        .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Bounded)
}

fn run_preparation<F: Future>(
    executor: &ApplicationExecutor,
    limits: product_host::RunBounds,
    future: F,
) -> CliResult<Option<F::Output>> {
    if run_expired(limits) {
        return Ok(None);
    }
    executor
        .block_on(async {
            match limits.deadline {
                Some(deadline) => tokio::time::timeout_at(deadline.into(), future).await.ok(),
                None => Some(future.await),
            }
        })
        .map_err(application_execution_failure)
}

/// Product presentation stays on the calling thread; only the hosted worker owns execution.
pub(crate) fn run_config(
    executor: &ApplicationExecutor,
    config: product_host::HostConfig<crate::surface_protocol::PlakExtension>,
    prompt: plakous_host_protocol::Draft,
    print_session_id: bool,
    resume_tree: bool,
    child_agents: face::ChildAgents,
    stdout_mode: TerminalMode,
    stdout: &mut dyn io::Write,
    stderr: &mut dyn io::Write,
    limits: product_host::RunBounds,
) -> CliResult<ProcessOutcome> {
    use crate::surface_protocol::{HostEvent, UserAction};
    let face = std::cell::RefCell::new(face::CliFace::new(
        face::CliFace::verbose_from_environment(),
        child_agents,
    ));
    if print_session_id {
        writeln!(stdout, "session_id={}", config.session.id)
            .map_err(|_| CliFailure::new("stdout failed"))?;
        stdout
            .flush()
            .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    let signal_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_signals = signal_count.clone();
    let interrupt = executor
        .block_on(async move {
            tokio::spawn(async move {
                while tokio::signal::ctrl_c().await.is_ok() {
                    observed_signals.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })
            .abort_handle()
        })
        .map_err(application_execution_failure)?;
    let mut delivered_signals = 0;
    let mut output_error = None;
    let observe = |event: Option<&HostEvent>,
                   controls: &crate::surface_protocol::UserActionSender| {
        // Ctrl-C is counted here; a supervisor's stop, or a closed terminal, is
        // counted by the watcher installed before this face was built. Both
        // raise the same interrupt, so the session is left resumable either
        // way. The watcher cannot be installed here: by the time the face
        // exists, admission and the store open have already run, and a stop
        // inside that window met the default disposition and killed the
        // process outright.
        let received = signal_count
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_add(crate::signals::signalled_stops());
        while delivered_signals < received {
            let action = if delivered_signals == 0 {
                UserAction::Interrupt
            } else {
                UserAction::Quit
            };
            delivered_signals += 1;
            let _ = controls.send(action);
        }
        // Every stderr line this face writes is decided in `face`, so the text
        // a user reads is pinned by tests rather than by a live provider.
        let written = event
            .map(|event| face.borrow_mut().lines(event))
            .unwrap_or_default()
            .iter()
            .try_for_each(|line| write_stderr_line(stderr, line))
            .and_then(|()| stderr.flush());
        if written.is_err() && output_error.is_none() {
            output_error = Some(CliFailure::new("stderr failed"));
        }
    };
    let result =
        product_host::OneShot::run_until_idle(config, prompt, resume_tree, limits, observe);
    interrupt.abort();
    let (assistant, outcome) = match result? {
        product_host::OneShotOutcome::Completed { assistant } => {
            (assistant, Ok(ProcessOutcome::Success))
        }
        product_host::OneShotOutcome::Refused { assistant, detail } => (
            assistant,
            Err(CliFailure::classified(
                CliFailureKind::Agent,
                detail.map_or_else(
                    || "model refused the request".to_owned(),
                    |detail| format!("model refused the request: {detail}"),
                ),
            )),
        ),
        product_host::OneShotOutcome::Interrupted { assistant } => {
            (assistant, Ok(ProcessOutcome::Interrupted))
        }
        product_host::OneShotOutcome::Bounded { assistant, .. } => {
            (assistant, Ok(ProcessOutcome::Bounded))
        }
    };
    // Report tool failures alongside the turn's terminal lifecycle outcome;
    // that outcome does not establish whether the requested task was fulfilled.
    if let Some(line) = face
        .borrow()
        .closing_line(matches!(outcome, Ok(ProcessOutcome::Success)))
        && write_stderr_line(stderr, &line)
            .and_then(|()| stderr.flush())
            .is_err()
        && output_error.is_none()
    {
        output_error = Some(CliFailure::new("stderr failed"));
    }
    // Preview events may diverge from the canonical result. Stdout is emitted
    // once, after terminal publication and release, so it can never need repainting.
    let assistant = assistant_stdout(assistant, stdout_mode);
    stdout
        .write_all(assistant.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|_| CliFailure::new("stdout failed"))?;
    if let Some(error) = output_error {
        return Err(error);
    }
    outcome
}

/// Every face line is untrusted text (tool reasons, host notes), so stderr
/// never carries a control sequence, whatever the stream is attached to.
fn write_stderr_line(stderr: &mut dyn io::Write, line: &str) -> io::Result<()> {
    writeln!(stderr, "{}", safe_terminal_block(line))
}

/// The canonical answer as stdout receives it. A terminal gets it without
/// control sequences; a pipe gets it byte-exact so scripts see what the model
/// wrote. It ends with a line break so a shell prompt never lands on it.
fn assistant_stdout(assistant: String, stdout_mode: TerminalMode) -> String {
    let mut assistant = match stdout_mode {
        TerminalMode::Interactive => safe_terminal_block(&assistant),
        TerminalMode::Piped => assistant,
    };
    if !assistant.is_empty() && !assistant.ends_with('\n') {
        assistant.push('\n');
    }
    assistant
}

/// The record a `plak run` on an API target saves: the exact selection, and
/// the composition `platform::api_composition` decides on a direct API
/// account (`plakous-default` binds `direct` on OpenRouter).
pub(crate) fn api_run_record(
    cwd: std::path::PathBuf,
    model: &str,
    effort: Option<ReasoningEffort>,
    composition: product_config::ProductCompositionSelection,
    selection: product_config::ProductTargetSelection,
) -> CliResult<product_sessions::SessionRecord> {
    selection
        .validate()
        .map_err(|message| CliFailure::classified(CliFailureKind::Usage, message))?;
    let api = selection
        .api_target()
        .ok_or_else(|| CliFailure::new("API run requires an API target"))?;
    if let plakous_hosted::binding::ApiTarget::Platform { provider } = api {
        crate::platform_catalog::validate(provider, model, effort, composition)
            .map_err(|message| CliFailure::classified(CliFailureKind::Usage, message))?;
    }
    let mut session = product_sessions::SessionRecord::new(cwd);
    session.account = Some(api.account().to_owned());
    session.model = Some(model.to_owned());
    session.effort = effort.map(product_host::effort_label);
    session.composition = match (api, composition) {
        (plakous_hosted::binding::ApiTarget::Platform { provider }, _) => {
            plakous_hosted::platform::api_composition(provider, model, composition)
                .map_err(|message| CliFailure::classified(CliFailureKind::Usage, message))?
        }
        (_, product_config::ProductCompositionSelection::FollowCatalog) => {
            product_config::ProductCompositionSelection::Direct
        }
        (_, other) => other,
    };
    session.target_selection = selection;
    Ok(session)
}

mod face;

#[cfg(test)]
mod tests;
