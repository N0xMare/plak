//! Model rebinding: immutable requests, canonical dispatch and feed projection.

use std::time::{Duration, Instant};

use plakous::model::{ModelId, ProcessingTierDisposition, ProviderServiceTierId};
use plakous::sessions::{
    AgentSessionsErrorKind, RebindSessionOutcome, SessionOperationId, SessionRuntimeBinding,
};
use plakous::subscriptions::NativeProvider;

use crate::agent::{
    AttachedCodingSession, ProductAgentSpec, coding_session_target_binding,
    selected_coding_provider_configuration_with_processing_tier,
};
use crate::binding::TargetSelection;
use crate::interrupt::{InterruptWatch, watched};
use crate::sessions::{self, SessionRecord};
use crate::{CliFailure, CliResult};

use super::admission::{emit_queue_snapshot, emit_unavailable_context};
use super::factory::observe_boundary;
use super::info::{effort_label, send_binding, send_last_user_prompt_at};
use super::models::{effort_from_config, native_compact_facet, oauth_provider_for_account};
use super::shell::{api_root_binding, profile_target_selection};
use super::source_release::{SourceRelease, release_source_attachment};
use super::startup::{
    api_runtime_for_record, bounded_transition_stage, observe_phase, open_session_for_host,
};
use super::usage::report_context;
use super::{HostBoundary, HostWorkflow};
use super::{
    HostConfig, HostLifecyclePhase, HostTargetAuthority, RebindBefore, RebindRoute,
    ResolvedRebindTarget, RuntimeRole, Sender, TopologyRows, TreeRows, topology_state,
};
use crate::sessions::{NoticeScope, TranscriptRow};

mod complete;
mod succession;

pub(super) use complete::complete_rebind;
pub(super) use succession::{Succession, settle_before_open, withdraw_after_open};

async fn report_rebind_context<P: plakous_host_protocol::ProtocolExtension>(
    opened: &AttachedCodingSession,
    events: &Sender<P>,
    config: &HostConfig<P>,
) {
    events.remember_request_tools(&[]);
    if let Err(error) = report_context(
        opened,
        events,
        "after rebind",
        config
            .target
            .native_provider()
            .is_some_and(native_compact_facet),
        config.session.context_counters(),
    )
    .await
    {
        emit_unavailable_context(events);
        let _ = events.notice(
            TranscriptRow::error(NoticeScope::Settings, error.message().to_owned())
                .with_optional_remedy(error.remedy()),
        );
    }
}

pub(super) async fn restore_detached_source<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    opened: &mut AttachedCodingSession,
    locator: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<()> {
    stderr_buf.clear();
    let replacement = open_session_for_host(
        config,
        &config.session,
        locator,
        stderr_buf,
        RuntimeRole::Primary,
    )
    .await
    .map_err(|error| *error.failure)?;
    let previous = std::mem::replace(opened, replacement);
    previous
        .release()
        .await
        .map_err(|error| CliFailure::new(format!("detached source release failed: {error}")))?;
    Ok(())
}

/// The tree a rebind compiles against, from the one own-tree decision the
/// caller already made. A profile fork's seeded target uses that id; every
/// other rebind keeps the source's tree. This does not read the store again.
pub(super) fn decided_topology_tree_id<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    own_tree: bool,
    target_product_id: &str,
) -> String {
    if own_tree {
        target_product_id.to_owned()
    } else {
        durable_tree_id(&config.topology_rows(), &config.session)
    }
}

/// A native rebind target's runtime, and the tree id when this build took an
/// `AttachRoot` lease.
///
/// The lease id is returned even when the binding fails after the attach, so
/// the caller can drop exactly that lease. `None` means this path took none:
/// a lookup, an injected runtime, or a failure before attach. The caller
/// releases only that id with [`release_profile_build_lease`].
pub(super) async fn native_rebind_runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    route: &RebindRoute,
    own_tree: bool,
    stderr_buf: &mut Vec<u8>,
) -> (CliResult<SessionRuntimeBinding>, Option<String>) {
    let attach = if own_tree {
        crate::topology_runtime::TopologyHubAttach::AttachRoot
    } else {
        crate::topology_runtime::TopologyHubAttach::LookupOnly
    };
    let spec = match route {
        RebindRoute::Native { spec, .. } => spec.as_deref(),
        RebindRoute::Api(_) => None,
    };
    config
        .runtime_factory
        .rebind_runtime(
            config,
            spec,
            attach,
            matches!(route, RebindRoute::Native { .. }),
            stderr_buf,
        )
        .await
}

/// Drops the lease a profile fork's runtime build took on the tree it
/// attached. Callers pass only an id this path acquired. The opened branch
/// holds its own lease; a build that took none is not released here.
pub(super) async fn release_profile_build_lease<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    tree_id: &str,
) -> CliResult<()> {
    let released = crate::topology_runtime::release_root_lease(tree_id).await;
    if let Err(error) = released {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Settings,
            format!("the profile tree's build lease could not be released: {error}"),
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "Native rebind resolution names the source authority, target tuple, plan namespace, and selected topology explicitly."
)]
pub(super) async fn resolve_rebind_target<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    plan_state_authority: &str,
    composition: crate::binding::CompositionSelection,
    target_selection: &TargetSelection,
    topology_tree_id: &str,
) -> CliResult<ResolvedRebindTarget> {
    let topology_enabled = config.live_topology_enabled()?;
    if let Some((provider, binding)) = config.runtime_factory.target_binding() {
        let (parsed_effort, explicit) = effort_from_config(effort);
        if effort.is_some() && !explicit {
            return Err(CliFailure::new("rebind target effort is invalid"));
        }
        return Ok(ResolvedRebindTarget {
            route: RebindRoute::Native {
                provider,
                resolved: None,
                spec: None,
            },
            effort: explicit.then_some(parsed_effort),
            effort_explicit: explicit,
            binding,
        });
    }
    let provider = oauth_provider_for_account(account)
        .ok_or_else(|| CliFailure::new("rebind target account is not a native OAuth provider"))?;
    let (parsed_effort, explicit) = effort_from_config(effort);
    if effort.is_some() && !explicit {
        return Err(CliFailure::new("rebind target effort is invalid"));
    }
    let model_id = ModelId::new(model.to_owned())
        .map_err(|_| CliFailure::new("rebind target model identity is invalid"))?;
    let subscriptions = config
        .target
        .native_subscriptions()
        .ok_or_else(|| CliFailure::new("subscription authority is missing"))?;
    let (reasoning_efforts, service_tiers, native_tool_mode) =
        rebind_model_capabilities(watch, config, subscriptions, provider, account, model).await?;
    admit_rebind_effort(&reasoning_efforts, explicit, parsed_effort)?;
    let target_effort = explicit.then_some(parsed_effort);
    let processing_tier = rebind_processing_tier(target_selection, provider, &service_tiers)?;
    let selected = selected_coding_provider_configuration_with_processing_tier(
        provider,
        model_id,
        target_effort,
        native_tool_mode,
        composition,
        processing_tier,
    )?;
    let limits = crate::agent::product_subscription_request_limits()
        .map_err(|_| CliFailure::new("rebind subscription gate is invalid"))?;
    let resolved = watched(
        watch,
        Box::pin(bounded_rebind_lookup(
            subscriptions.resolve_turn_scoped_provider(
                selected,
                limits,
                plakous::model::SessionLineage::Root,
            ),
        )),
    )
    .await
    .map_err(|()| CliFailure::new("rebind target resolution timed out; source unchanged"))?
    .map_err(|error| {
        CliFailure::new(format!("rebind target resolution failed: {}", error.kind()))
    })?;
    let resolved = std::sync::Arc::new(resolved);
    let binding = coding_session_target_binding(resolved.as_ref());
    let spec = ProductAgentSpec {
        prompt_host_facts: config.prompt_host_facts(),
        product_home: Some(config.product_home.clone()),
        tool_selection: super::startup::root_tool_selection(&config.session),
        provider,
        resolved: std::sync::Arc::clone(&resolved),
        effort: target_effort,
        effort_explicit: explicit,
        workspace_root: config.session.workspace_root.clone(),
        cwd: config.cwd.clone(),
        plan_state_authority: plan_state_authority.to_owned(),
        composition,
        topology_tree_id: Some(topology_tree_id.to_owned()),
        topology_caller_id: None,
        state_roots: config.state_roots.clone(),
        scheduled_compaction: config.compaction_enabled,
        max_model_rounds: config.max_model_rounds,
        topology_enabled,
        continue_after_loss: config.resume_after_loss == crate::runtime::ResumeAfterLoss::Continue,
    };
    Ok(ResolvedRebindTarget {
        route: RebindRoute::Native {
            provider,
            resolved: Some(resolved),
            spec: Some(Box::new(spec)),
        },
        effort: target_effort,
        effort_explicit: explicit,
        binding,
    })
}

/// The refusal of a rebind whose target route cannot serve one of the
/// session's frozen tools. A rebind keeps those tools, so canonical tool
/// authority never changes across it; this refuses before any durable work.
fn frozen_tools_rebind_refusal<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    account: &str,
    target_selection: &TargetSelection,
) -> Option<String> {
    let tools = super::startup::root_tool_selection(&config.session);
    if tools.selection.is_empty() {
        return None;
    }
    let route = match target_selection.api_target() {
        Some(target) => Some(crate::tool_plan::ToolRoute::api(target)),
        None => oauth_provider_for_account(account).and_then(crate::tool_plan::ToolRoute::native),
    };
    let Some(route) = route else {
        return Some(format!(
            "this session keeps its tools across a model change, and {account} serves none of them; start a fresh conversation on that model"
        ));
    };
    let options = match crate::frozen_tables::retrieval_options(&tools.tool_sections) {
        Ok(options) => options,
        Err(error) => return Some(error),
    };
    crate::tool_plan::bind_tool_plan(&tools.selection, route, account)
        .and_then(|_| {
            crate::tool_plan::check_retrieval_options(&tools.selection, route, account, &options)
        })
        .err()
        .map(|refusal| refusal.rebind_line())
}

/// The transcript line a completed rebind prints.
///
/// A profile whose root differs from the source only in effort still forks.
/// Only a genuine model or provider change is a model switch; an effort-only
/// change says so and drops the
/// "native reasoning from the previous model" caveat, which does not apply
/// when the model has not moved.
pub(super) fn rebind_note(before: RebindBefore<'_>, after: RebindBefore<'_>) -> String {
    let account = after.account.unwrap_or("-");
    if before.account == after.account && before.model == after.model {
        let effort = after
            .effort
            .map_or_else(|| "default".to_owned(), effort_label);
        return format!(
            "Effort changed \u{b7} {account} \u{b7} {} \u{b7} {effort}\nThis chat continues on the same model.",
            after.model
        );
    }
    format!(
        "Model switched \u{b7} {account} \u{b7} {}\nThis chat continues; native reasoning from the previous model is not sent.\nPrevious session remains available to resume.",
        after.model
    )
}

pub(super) fn durable_tree_id(rows: &dyn TopologyRows, session: &SessionRecord) -> String {
    topology_state::durable_tree_id(rows, session)
}

#[cfg(test)]
pub(super) fn tree_epoch_blocks_rebind(rows: &dyn TopologyRows, session: &SessionRecord) -> bool {
    tree_epoch_rebind_refusal(rows, session).is_some()
}

pub(super) fn tree_epoch_rebind_refusal(
    rows: &dyn TopologyRows,
    session: &SessionRecord,
) -> Option<crate::agent_tree::EpochRebindRefusal> {
    topology_state::epoch_rebind_refusal(rows, &durable_tree_id(rows, session))
}

pub(super) fn session_rebind_boundary_ready(session: &SessionRecord) -> bool {
    session.pending_queue.is_empty() && !session.queue_commands.has_pending()
}

pub(super) fn interrupted_before_dispatch(operation: &str) -> CliFailure {
    CliFailure::new(format!(
        "{operation} was interrupted before its canonical dispatch; the exact source is unchanged"
    ))
}

pub(super) async fn abandon_prepared_rebind<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    operation: &str,
) -> CliResult<()> {
    let sessions = config.state_roots.runtime_sessions()?;
    let source = config
        .session
        .plakous_locator
        .ok_or_else(|| CliFailure::new("rebind source locator is missing"))?;
    super::projection::project_to_tail(&sessions, &config.store, &config.session.id, source)
        .await
        .map_err(CliFailure::new)?;
    config
        .store
        .discard_unadmitted_workflow(&mut config.session, operation)
        .map_err(CliFailure::new)
}

/// Abandons a prepared rebind whose publication definitely did not land,
/// and a switch's pending tree epoch with it.
pub(super) async fn abandon_switch<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    operation: &str,
    succession: Option<&Succession>,
) -> CliResult<()> {
    let abandoned = abandon_prepared_rebind(config, operation).await;
    if let Some(succession) = succession {
        succession.withdraw(config, operation).await?;
    }
    abandoned
}

/// How a rebind that did not fail ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Rebound {
    /// The attached session moved to a new branch or a resumed ancestor.
    Switched,
    /// The root node continued on its own line as the profile (Switch now).
    Succeeded,
    /// The requested binding was already active; nothing changed.
    Unchanged,
}

/// Whether a profile's cards are the ones this session's tree already froze.
fn profile_cards_active<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    roster: &crate::orchestration::RosterInput,
) -> CliResult<bool> {
    if !config.live_topology_enabled()? {
        // No spawn tool is compiled, so the cards cannot differ.
        return Ok(true);
    }
    let rows = config.topology_rows();
    let tree_id = durable_tree_id(&rows, &config.session);
    let frozen = match crate::topology_runtime::lookup(&tree_id) {
        Some(runtime) => runtime
            .read(|tree| tree.current_epoch().templates_digest.clone())
            .ok(),
        None => match rows.tree(&tree_id) {
            Ok(Some(tree)) => Some(tree.current_epoch().templates_digest.clone()),
            // A topology-admitted fork must seed its own tree.
            Ok(None) => return Ok(false),
            Err(_) => None,
        },
    };
    Ok(frozen.is_some_and(|digest| digest == roster.templates_digest))
}

/// What a profile fork resolved before any runtime is built.
enum ProfileTarget {
    Native(Box<ResolvedRebindTarget>),
    Api(Box<SessionRecord>),
}

/// Forks the attached session onto a profile's root: the branch opens a
/// tree of its own frozen on `roster`, so the source keeps its tree and cards.
#[allow(
    clippy::too_many_arguments,
    reason = "Profile rebinding receives the attached source, interrupt watch, target tuple, and frozen roster as distinct authorities."
)]
pub(super) async fn handle_rebind<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    extra: Option<&str>,
    composition: Option<crate::binding::CompositionSelection>,
    protocol: Option<crate::binding::ProtocolSelection>,
    roster: &crate::orchestration::RosterInput,
    switch: Option<&str>,
) -> CliResult<Rebound> {
    let rebound: CliResult<Rebound> = Box::pin(async {
        // A switch hands running processes to the successor on the same
        // line; a fork leaves them with the source.
        if switch.is_none() {
            super::jobs_transition_guard(config, opened)?;
        }
        watch.begin();
        let started = Instant::now();
        refuse_unready_rebind(config, switch.is_some())?;
        let account = account.to_owned();
        // A Code Mode (native-ptc) prefix is replayable onto every presentation:
        // each kernel re-projects `exec`/`wait` cells truthfully at dispatch
        // (Responses/Chat carry the original string input, Messages wraps it as
        // `{"input": ...}`), so the rebind branches at the canonical checkpoint
        // like any other and nothing is refused up front.
        let (target_selection, api_root, effort_label, composition) = profile_rebind_selection(
            config,
            &account,
            model,
            effort,
            extra,
            composition,
            protocol,
        )?;
        // refuse before the first resolution round-trip,
        // while no durable rebind work exists to roll back.
        if watch.observe_now() {
            return Err(interrupted_before_dispatch("rebind"));
        }
        let same_binding = config.session.account.as_deref() == Some(account.as_str())
            && config.session.model.as_deref() == Some(model)
            && config.session.effort.as_deref() == effort_label.as_deref()
            && config.session.composition == composition
            && config.session.target_selection == target_selection;
        if same_binding && profile_cards_active(config, roster)? {
            return Ok(Rebound::Unchanged);
        }
        if let Some(refusal) = frozen_tools_rebind_refusal(config, &account, &target_selection) {
            return Err(CliFailure::new(refusal)
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {}));
        }
        // A rebind or switch keeps canonical tool authority: its successor
        // inherits whether the source's skills are on, whatever tuple it
        // moves to, and never takes the live setting (spec 02 §6.1.1).
        let inherited = source_skills_on(opened).await?;
        config.state_roots.set_inherited_skills(inherited);
        let (operation, target_product_id, own_tree, topology_tree_id, succession) =
            stage_rebind(config, opened, switch, &account, model, composition, roster).await?;
        let target = resolve_profile_target(
            watch,
            config,
            ProfileTargetInput {
                api_root,
                account: &account,
                model,
                effort_label: &effort_label,
                composition,
                target_selection: &target_selection,
                target_product_id: &target_product_id,
                operation: &operation,
                topology_tree_id: &topology_tree_id,
            },
        )
        .await?;
        observe_phase(
            events,
            HostLifecyclePhase::RebindResolveTarget,
            started,
            config.session.pending_queue.len(),
        );
        let instructions_before = instruction_sources(opened);
        let mut attached_tree = None;
        let branched = attach_profile_branch(
            watch,
            config,
            opened,
            stderr_buf,
            events,
            ProfileBranchInput {
                target,
                own_tree,
                profile: sessions::RootProfile::from_roster(roster),
                topology_tree_id: &topology_tree_id,
                effort_label: &effort_label,
                attached_tree: &mut attached_tree,
                started,
                account: &account,
                model,
                composition,
                target_selection,
                operation: &operation,
                succession: succession.as_ref(),
            },
        )
        .await;
        if let Some(tree_id) = attached_tree {
            release_profile_build_lease(events, &tree_id).await?;
        }
        if matches!(branched, Ok(Rebound::Switched | Rebound::Succeeded)) {
            note_reread_instructions(events, instructions_before, opened);
        }
        branched
    })
    .await;
    config.state_roots.set_inherited_skills(None);
    config.state_roots.set_pending_epoch(None);
    rebound
}

/// Whether the attached source session has its skills on, from its sealed
/// registry. `None` when it has no sealed checkpoint to inherit from: the
/// rebind's or fork's own load of the source then refuses it.
pub(super) async fn source_skills_on(opened: &AttachedCodingSession) -> CliResult<Option<bool>> {
    let read = crate::skills::frozen_skills(opened.sessions(), opened.locator()).await;
    match read {
        Ok(frozen) => Ok(Some(frozen.on)),
        Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliFailure::new(format!(
            "the session's skills setting could not be read: {error}"
        ))),
    }
}

struct ProfileTargetInput<'a> {
    api_root: bool,
    account: &'a String,
    model: &'a str,
    effort_label: &'a Option<String>,
    composition: crate::binding::CompositionSelection,
    target_selection: &'a TargetSelection,
    target_product_id: &'a String,
    operation: &'a String,
    topology_tree_id: &'a str,
}

struct ProfileBranchInput<'a> {
    target: ProfileTarget,
    own_tree: bool,
    profile: sessions::RootProfile,
    topology_tree_id: &'a str,
    effort_label: &'a Option<String>,
    attached_tree: &'a mut Option<String>,
    started: Instant,
    account: &'a str,
    model: &'a str,
    composition: crate::binding::CompositionSelection,
    target_selection: TargetSelection,
    operation: &'a str,
    succession: Option<&'a Succession>,
}

/// Refuses a rebind the session cannot take now. A switch continues the
/// root node, so its running children and processes do not block it; a
/// fork needs a quiescent tree.
fn refuse_unready_rebind<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    switch: bool,
) -> CliResult<()> {
    if let Some(reason) = tree_epoch_rebind_refusal(&config.topology_rows(), &config.session)
        && !switch
    {
        return Err(CliFailure::new(reason.message()));
    }
    if !session_rebind_boundary_ready(&config.session) {
        return Err(CliFailure::new(
            "rebind requires an idle session with no queued or unresolved product work",
        ));
    }
    Ok(())
}

fn profile_rebind_selection<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    extra: Option<&str>,
    composition: Option<crate::binding::CompositionSelection>,
    protocol: Option<crate::binding::ProtocolSelection>,
) -> CliResult<(
    TargetSelection,
    bool,
    Option<String>,
    crate::binding::CompositionSelection,
)> {
    let target_selection =
        profile_target_selection(account, extra, protocol).map_err(CliFailure::new)?;
    let api_root = target_selection.api_target().is_some();
    let (effort_label, composition) = if api_root {
        // An API root carries no catalog row to follow; it binds as a new
        // session from the same template would.
        api_root_binding(
            &target_selection,
            model,
            effort,
            composition.unwrap_or(crate::binding::CompositionSelection::FollowCatalog),
        )
        .map_err(CliFailure::new)?
    } else {
        (
            validate_rebind_effort(&config.ready_catalog.accounts, account, model, effort)?,
            composition.unwrap_or(config.session.composition),
        )
    };
    Ok((target_selection, api_root, effort_label, composition))
}

/// The operation, successor id, tree decision and, for a switch, its tree
/// half. A switch's successor root compiles against the epoch it is
/// published with, which the tree takes only after the publication.
async fn stage_rebind<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    switch: Option<&str>,
    account: &str,
    model: &str,
    composition: crate::binding::CompositionSelection,
    roster: &crate::orchestration::RosterInput,
) -> CliResult<(String, String, bool, String, Option<Succession>)> {
    let Some(profile) = switch else {
        let (operation, target, own_tree, tree_id) =
            stage_profile_fork(config, opened, account, model, composition, roster).await?;
        return Ok((operation, target, own_tree, tree_id, None));
    };
    let (operation, target, own_tree, tree_id) = stage_switch(config, opened);
    let succession = Succession::plan(config, profile, roster)?;
    config
        .state_roots
        .set_pending_epoch(succession.pending_epoch());
    Ok((operation, target, own_tree, tree_id, Some(succession)))
}

/// A switch's operation and successor: the root node stays on its tree.
fn stage_switch<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    opened: &AttachedCodingSession,
) -> (String, String, bool, String) {
    let operation = format!("plak:switch:{}", uuid::Uuid::new_v4());
    let target_product_id = sessions::projector::WorkflowRequest {
        operation_id: operation.clone(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Fork,
    }
    .successor_product_id();
    let topology_tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    (operation, target_product_id, false, topology_tree_id)
}

async fn stage_profile_fork<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    opened: &AttachedCodingSession,
    account: &str,
    model: &str,
    composition: crate::binding::CompositionSelection,
    roster: &crate::orchestration::RosterInput,
) -> CliResult<(String, String, bool, String)> {
    let operation = format!("plak:rebind:{}", uuid::Uuid::new_v4());
    let target_product_id = sessions::projector::WorkflowRequest {
        operation_id: operation.clone(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Fork,
    }
    .successor_product_id();
    let own_tree = composition != crate::binding::CompositionSelection::PySdk
        && config.live_topology_enabled()?;
    if own_tree {
        crate::agent::seed_genesis_tree(
            &config.state_roots,
            &target_product_id,
            &config.session.workspace_root,
            model,
            account,
            roster.clone(),
        )
        .await?;
    }
    let topology_tree_id = if composition == crate::binding::CompositionSelection::PySdk {
        target_product_id.clone()
    } else {
        decided_topology_tree_id(config, own_tree, &target_product_id)
    };
    Ok((operation, target_product_id, own_tree, topology_tree_id))
}

async fn resolve_profile_target<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    input: ProfileTargetInput<'_>,
) -> CliResult<ProfileTarget> {
    let ProfileTargetInput {
        api_root,
        account,
        model,
        effort_label,
        composition,
        target_selection,
        target_product_id,
        operation,
        topology_tree_id,
    } = input;
    let target = if api_root {
        let mut record = SessionRecord::new(config.session.workspace_root.clone());
        record.id.clone_from(target_product_id);
        record.node = sessions::SessionNode::genesis(target_product_id);
        record.account = Some(account.clone());
        record.model = Some(model.to_owned());
        record.effort.clone_from(effort_label);
        record.composition = composition;
        record.target_selection = target_selection.clone();
        record.source_product_session_id = Some(config.session.id.clone());
        record.source_workflow_operation_id = Some(operation.clone());
        record.frozen_tools.clone_from(&config.session.frozen_tools);
        record.validate_for_store().map_err(CliFailure::new)?;
        ProfileTarget::Api(Box::new(record))
    } else {
        ProfileTarget::Native(Box::new(
            resolve_rebind_target(
                watch,
                config,
                account,
                model,
                effort_label.as_deref(),
                target_product_id,
                composition,
                target_selection,
                topology_tree_id,
            )
            .await?,
        ))
    };
    Ok(target)
}

async fn attach_profile_branch<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    input: ProfileBranchInput<'_>,
) -> CliResult<Rebound> {
    let ProfileBranchInput {
        target,
        own_tree,
        profile,
        topology_tree_id,
        effort_label,
        attached_tree,
        started,
        account,
        model,
        composition,
        target_selection,
        operation,
        succession,
    } = input;
    let (runtime, target) = prepare_profile_runtime(
        watch,
        config,
        stderr_buf,
        target,
        ProfileRuntimeInput {
            own_tree,
            topology_tree_id,
            effort_label: effort_label.as_deref(),
        },
        attached_tree,
    )
    .await?;
    observe_phase(
        events,
        HostLifecyclePhase::RebindPrepareRuntime,
        started,
        config.session.pending_queue.len(),
    );
    // the target is resolved but nothing durable exists yet, so
    // an intent observed here refuses without creating a branch.
    if watch.observe_now() {
        return Err(interrupted_before_dispatch("rebind"));
    }
    // The tree the branch compiles against: a profile fork's seeded tree, or
    // a py-sdk root's own id with none, frozen on the profile's roster;
    // otherwise, and always for a switch, the source's tree.
    let own_tree = (own_tree || composition == crate::binding::CompositionSelection::PySdk)
        && succession.is_none();
    let selection = rebind_selection(
        &target,
        own_tree.then(|| profile.clone()),
        account,
        model,
        effort_label.as_deref(),
        composition,
        target_selection,
    )?;
    let workflow_kind = profile_workflow_kind(selection, succession)?;
    let attached_locator = publish_rebind_intent(
        watch,
        config,
        opened,
        events,
        RebindIntent {
            operation,
            workflow_kind,
            started,
        },
    )
    .await?;
    if let Some(succession) = succession
        && let Err(error) = succession.record(config, operation).await
    {
        abandon_prepared_rebind(config, operation).await?;
        return Err(error);
    }
    let source_locator = detach_prepared_rebind(
        watch,
        config,
        opened,
        stderr_buf,
        operation,
        attached_locator,
        succession,
    )
    .await?;
    Box::pin(complete_rebind(
        watch,
        config,
        opened,
        stderr_buf,
        events,
        operation,
        source_locator,
        target,
        runtime,
        started,
        succession,
    ))
    .await
    .map(|()| {
        if succession.is_some() {
            Rebound::Succeeded
        } else {
            Rebound::Switched
        }
    })
}

async fn rebind_model_capabilities<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    subscriptions: &plakous::subscriptions::NativeSubscriptions,
    provider: NativeProvider,
    account: &str,
    model: &str,
) -> CliResult<(
    Vec<plakous::model::ReasoningEffort>,
    Vec<String>,
    Option<plakous::subscriptions::NativeToolMode>,
)> {
    let candidate = match &config.target {
        HostTargetAuthority::Native {
            catalog_candidates, ..
        } => catalog_candidates
            .iter()
            .find(|candidate| candidate.account == account && candidate.exact_id == model),
        HostTargetAuthority::OpenRouter { .. } | HostTargetAuthority::Platform { .. } => None,

        HostTargetAuthority::Registered { .. } => None,
    };
    let (reasoning_efforts, service_tiers, native_tool_mode) = if let Some(candidate) = candidate {
        (
            candidate.reasoning_efforts.clone(),
            candidate
                .service_tiers
                .iter()
                .map(|tier| tier.0.clone())
                .collect::<Vec<_>>(),
            candidate.native_tool_mode,
        )
    } else {
        let catalog = watched(
            watch,
            bounded_rebind_lookup(subscriptions.model_catalog(provider)),
        )
        .await
        .map_err(|()| CliFailure::new("rebind catalog refresh timed out; source unchanged"))?
        .map_err(rebind_catalog_failure)?;
        let catalog_entry = catalog
            .models()
            .iter()
            .find(|entry| entry.exact_id() == model)
            .ok_or_else(|| {
                CliFailure::new("rebind model is absent from the authenticated catalog")
            })?;
        (
            catalog_entry.reasoning_efforts().to_vec(),
            catalog_entry
                .service_tiers()
                .iter()
                .map(|tier| tier.id().to_owned())
                .collect(),
            catalog_entry.native_tool_mode(),
        )
    };
    Ok((reasoning_efforts, service_tiers, native_tool_mode))
}

fn admit_rebind_effort(
    reasoning_efforts: &[plakous::model::ReasoningEffort],
    explicit: bool,
    parsed_effort: plakous::model::ReasoningEffort,
) -> CliResult<()> {
    if reasoning_efforts.is_empty() {
        if explicit {
            return Err(CliFailure::new(
                "rebind target does not admit a reasoning effort",
            ));
        }
    } else if !explicit {
        return Err(CliFailure::new(
            "reasoning rebind target requires an explicit effort",
        ));
    } else if !reasoning_efforts.contains(&parsed_effort) {
        return Err(CliFailure::new(
            "rebind target effort is absent from the authenticated catalog",
        ));
    }
    Ok(())
}

fn rebind_processing_tier(
    target_selection: &TargetSelection,
    provider: NativeProvider,
    service_tiers: &[String],
) -> CliResult<ProcessingTierDisposition> {
    let processing_tier = match target_selection {
        TargetSelection::Native => ProcessingTierDisposition::Standard,
        TargetSelection::NativePriority
            if provider == NativeProvider::Codex
                && service_tiers.iter().any(|tier| tier == "priority") =>
        {
            ProcessingTierDisposition::ProviderNamed(
                ProviderServiceTierId::new("priority")
                    .map_err(|_| CliFailure::new("Codex priority tier identity is invalid"))?,
            )
        }
        TargetSelection::NativePriority => {
            return Err(CliFailure::new(
                "Fast is absent from this exact authenticated Codex model row",
            ));
        }
        TargetSelection::OpenRouter { .. } | TargetSelection::Platform { .. } => {
            return Err(CliFailure::new(
                "OpenRouter target selection cannot be used for a native OAuth rebind",
            ));
        }
    };
    Ok(processing_tier)
}

struct ProfileRuntimeInput<'a> {
    own_tree: bool,
    topology_tree_id: &'a str,
    effort_label: Option<&'a str>,
}

async fn prepare_profile_runtime<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
    target: ProfileTarget,
    input: ProfileRuntimeInput<'_>,
    attached_tree: &mut Option<String>,
) -> CliResult<(SessionRuntimeBinding, ResolvedRebindTarget)> {
    let ProfileRuntimeInput {
        own_tree,
        topology_tree_id,
        effort_label,
    } = input;
    stderr_buf.clear();
    // A profile's own tree has no hub yet: attach one so the new
    // session's tools compile against it. Only a lease this build
    // took is dropped below.
    let (runtime, mut target) = match target {
        ProfileTarget::Native(target) => {
            let (runtime, tree) = watched(
                watch,
                native_rebind_runtime(config, &target.route, own_tree, stderr_buf),
            )
            .await;
            *attached_tree = tree;
            (runtime?, *target)
        }
        ProfileTarget::Api(record) => {
            let attach = if own_tree {
                crate::topology_runtime::TopologyHubAttach::AttachRoot
            } else {
                crate::topology_runtime::TopologyHubAttach::LookupOnly
            };
            let mut tree = None;
            let runtime = watched(
                watch,
                api_runtime_for_record(
                    config,
                    &record,
                    false,
                    attach,
                    Some(topology_tree_id),
                    &mut tree,
                ),
            )
            .await;
            *attached_tree = tree;
            let runtime = runtime?;
            let (effort, effort_explicit) = effort_from_config(effort_label);
            let binding = runtime.target_binding();
            let target = ResolvedRebindTarget {
                route: RebindRoute::Api(record),
                effort: effort_explicit.then_some(effort),
                effort_explicit,
                binding,
            };
            (runtime, target)
        }
    };
    target.binding = runtime.target_binding();
    Ok((runtime, target))
}

/// A profile fork's `Rebind` workflow, or a switch's `Switch` one.
fn profile_workflow_kind(
    selection: sessions::RebindSelection,
    succession: Option<&Succession>,
) -> CliResult<sessions::SessionWorkflowKind> {
    match succession {
        Some(succession) => succession.workflow_kind(selection),
        None => Ok(sessions::SessionWorkflowKind::Rebind(Box::new(selection))),
    }
}

fn rebind_selection(
    target: &ResolvedRebindTarget,
    own_tree: Option<sessions::RootProfile>,
    account: &str,
    model: &str,
    effort_label: Option<&str>,
    composition: crate::binding::CompositionSelection,
    target_selection: TargetSelection,
) -> CliResult<sessions::RebindSelection> {
    let provider = match &target.route {
        RebindRoute::Native { provider, .. } => format!("{provider:?}").to_ascii_lowercase(),
        RebindRoute::Api(_) => account.to_owned(),
    };
    Ok(sessions::RebindSelection {
        provider,
        account: account.to_owned(),
        model: model.to_owned(),
        effort: effort_label.map(str::to_owned),
        composition,
        target_selection,
        protocol: target.binding.protocol().as_str().to_owned(),
        route: target.binding.route().as_str().to_owned(),
        requested_model: target.binding.model().as_str().to_owned(),
        selected_configuration: serde_json::to_string(
            &target.binding.selected_provider_configuration(),
        )
        .map_err(|_| CliFailure::new("selected configuration could not be persisted"))?,
        effective_configuration: serde_json::to_string(
            &target.binding.effective_provider_configuration(),
        )
        .map_err(|_| CliFailure::new("effective configuration could not be persisted"))?,
        account_binding: serde_json::to_string(&target.binding.account_binding())
            .map_err(|_| CliFailure::new("account binding could not be persisted"))?,
        own_tree,
    })
}

struct RebindIntent<'a> {
    operation: &'a str,
    workflow_kind: sessions::SessionWorkflowKind,
    started: Instant,
}

async fn publish_rebind_intent<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    events: &Sender<P>,
    intent: RebindIntent<'_>,
) -> CliResult<plakous::sessions::SessionLocator> {
    let RebindIntent {
        operation,
        workflow_kind,
        started,
    } = intent;
    let attached_locator = opened.locator();
    if config.session.plakous_locator != Some(attached_locator) {
        return Err(CliFailure::new(
            "rebind source does not match the exact durable product binding",
        ));
    }
    if config.session.store_revision == 0 {
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)?;
    }
    watched(
        watch,
        super::projection::project_to_tail_interruptible(
            opened.sessions(),
            &config.store,
            &config.session.id,
            attached_locator,
            || watch.observe_now(),
            |_, _| {},
        ),
    )
    .await
    .map_err(|error| {
        if watch.interrupted() {
            interrupted_before_dispatch("rebind")
        } else {
            CliFailure::new(error)
        }
    })?;
    let intents = config
        .store
        .load_intents(&config.session.id)
        .map_err(CliFailure::new)?;
    let views = config
        .store
        .load_views(&config.session.id)
        .map_err(CliFailure::new)?
        .ok_or_else(|| CliFailure::new("rebind requires projected views"))?;
    if intents.workflow_requests.iter().any(|request| {
        views
            .state
            .workflows
            .get(&request.operation_id)
            .is_none_or(|workflow| workflow.settled.is_none())
    }) {
        return Err(CliFailure::new(
            "rebind requires a safe boundary with no unresolved workflow request",
        ));
    }
    config
        .store
        .submit_workflow_request(
            &mut config.session,
            &sessions::projector::WorkflowRequest {
                operation_id: operation.to_owned(),
                source_locator: attached_locator,
                kind: workflow_kind,
            },
        )
        .map_err(CliFailure::new)?;
    observe_phase(
        events,
        HostLifecyclePhase::RebindPublishIntent,
        started,
        config.session.pending_queue.len(),
    );
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Rebind,
        HostBoundary::Prepared,
    );
    Ok(attached_locator)
}

async fn detach_prepared_rebind<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    operation: &str,
    attached_locator: plakous::sessions::SessionLocator,
    succession: Option<&Succession>,
) -> CliResult<plakous::sessions::SessionLocator> {
    // A switch releases the source for transfer, so its running processes
    // stay with the line for the successor to take.
    let released = if succession.is_some() {
        watched(watch, opened.detach_for_transfer()).await
    } else {
        watched(watch, opened.detach()).await
    };
    let source_locator = match released {
        Ok(locator) => locator,
        Err(error) => {
            abandon_switch(config, operation, succession).await?;
            return Err(CliFailure::new(format!(
                "rebind source release failed after durable preparation ({:?}): {error}",
                error.kind()
            )));
        }
    };
    if source_locator != attached_locator {
        restore_detached_source(config, opened, source_locator, stderr_buf).await?;
        abandon_switch(config, operation, succession).await?;
        return Err(CliFailure::new(
            "rebind source advanced after durable preparation; exact reconciliation is required",
        ));
    }
    Ok(source_locator)
}

/// The attached agent's enrolled instruction sources, captured before a
/// rebind detaches it so the switched branch can be compared against them.
pub(super) fn instruction_sources(
    opened: &AttachedCodingSession,
) -> Option<Vec<plakous::agent::ResolvedContextSourceFact>> {
    opened
        .agent()
        .map(|agent| agent.harness_plan().instructions().sources().to_vec())
}

/// After a branch switched, names each `AGENTS.md` its target re-read.
pub(super) fn note_reread_instructions<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    before: Option<Vec<plakous::agent::ResolvedContextSourceFact>>,
    opened: &AttachedCodingSession,
) {
    if let (Some(before), Some(agent)) = (before, opened.agent())
        && let Some(note) =
            reread_instructions_note(&before, agent.harness_plan().instructions().sources())
    {
        let _ = events.notice(TranscriptRow::info(NoticeScope::Settings, note));
    }
}

/// The same note for a branch that was saved but not attached, read from its
/// stored plan. A note that cannot be read is omitted.
pub(super) async fn note_reread_instructions_at<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    events: &Sender<P>,
    before: Option<Vec<plakous::agent::ResolvedContextSourceFact>>,
    target: plakous::sessions::SessionLocator,
) {
    let Some(before) = before else { return };
    let Ok(sessions) = config.state_roots.runtime_sessions() else {
        return;
    };
    if let Ok(after) = sessions.created_instruction_sources(target).await
        && let Some(note) = reread_instructions_note(&before, &after)
    {
        let _ = events.notice(TranscriptRow::info(NoticeScope::Settings, note));
    }
}

/// What a branch that recompiled for its target re-read from disk: each
/// enrolled `AGENTS.md` (and the skills listing) whose bytes differ from the
/// source's, plus any the target gained or lost. `None` when the instructions
/// are unchanged.
fn reread_instructions_note(
    before: &[plakous::agent::ResolvedContextSourceFact],
    after: &[plakous::agent::ResolvedContextSourceFact],
) -> Option<String> {
    let label = plakous::prompts::instructions::instruction_source_scope_label;
    let previous = |id: &str| before.iter().find(|source| source.source_id() == id);
    let mut changes = Vec::new();
    for source in after {
        match previous(source.source_id()) {
            Some(old) if old.content_digest() == source.content_digest() => {}
            Some(_) => changes.push(format!("{} changed", label(source.source_id()))),
            None => changes.push(format!("{} added", label(source.source_id()))),
        }
    }
    for source in before {
        if !after
            .iter()
            .any(|kept| kept.source_id() == source.source_id())
        {
            changes.push(format!("{} removed", label(source.source_id())));
        }
    }
    let skills = plakous::prompts::skills::SKILLS_SOURCE_ID;
    let read = if before
        .iter()
        .chain(after)
        .any(|source| source.source_id() == skills)
    {
        "AGENTS.md and skills"
    } else {
        "AGENTS.md"
    };
    (!changes.is_empty()).then(|| format!("{read} re-read for this branch: {}", changes.join(", ")))
}

pub(super) fn validate_rebind_effort(
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
    account: &str,
    model: &str,
    effort: Option<&str>,
) -> CliResult<Option<String>> {
    let (_, explicit) = effort_from_config(effort);
    if effort.is_some() && !explicit {
        return Err(CliFailure::new("rebind target effort is invalid"));
    }
    let entry = ready_catalog
        .iter()
        .find(|section| section.ready && section.account == account)
        .and_then(|section| section.entries.iter().find(|entry| entry.id == model))
        .ok_or_else(|| CliFailure::new("rebind target is absent from the exact Ready catalog"))?;
    if entry.effort_options.is_empty() {
        if explicit {
            return Err(CliFailure::new(
                "rebind target does not admit a reasoning effort",
            ));
        }
        return Ok(None);
    }
    if !explicit && entry.effort_options.iter().any(|option| option == "medium") {
        return Ok(Some("medium".to_owned()));
    }
    if !explicit {
        return Err(CliFailure::new(
            "reasoning rebind target requires an explicit effort because medium is not admitted",
        ));
    }
    let effort = effort.ok_or_else(|| CliFailure::new("rebind target effort is missing"))?;
    if !entry.effort_options.iter().any(|option| option == effort) {
        return Err(CliFailure::new(
            "rebind target effort is absent from the exact Ready catalog",
        ));
    }
    Ok(Some(effort.to_owned()))
}

#[cfg(test)]
mod tests;

// Native catalog operations allow 120 s; leave margin for the compatibility flock.
const REBIND_LOOKUP_TIMEOUT: Duration = Duration::from_secs(150);

async fn bounded_rebind_lookup<T>(operation: impl Future<Output = T>) -> Result<T, ()> {
    bounded_transition_stage(REBIND_LOOKUP_TIMEOUT, operation).await
}

fn rebind_catalog_failure(error: plakous::subscriptions::NativeSubscriptionError) -> CliFailure {
    match error {
        plakous::subscriptions::NativeSubscriptionError::ModelLookup { .. } => CliFailure::new(
            format!("rebind catalog refresh failed: {error}; source unchanged"),
        ),
        _ => CliFailure::new("rebind catalog refresh failed; source unchanged"),
    }
}

#[cfg(test)]
mod lookup_retry_tests {
    #[test]
    fn rebind_catalog_keeps_lookup_attempt_count_and_cause() {
        let error = super::rebind_catalog_failure(
            plakous::subscriptions::NativeSubscriptionError::ModelLookup {
                attempts: 3,
                cause: "HTTP 503",
                retry_after: None,
            },
        );
        assert!(
            error
                .message()
                .contains("model lookup failed after 3 attempts: HTTP 503")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rebind_lookup_allows_stalled_first_attempt_and_retry() {
        let mut attempts = 0;
        let result = super::bounded_rebind_lookup(async {
            attempts += 1;
            let first = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                std::future::pending::<()>(),
            )
            .await;
            assert!(first.is_err());
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            attempts += 1;
            "second lookup attempt succeeded"
        })
        .await;
        assert_eq!(result, Ok("second lookup attempt succeeded"));
        assert_eq!(attempts, 2);
    }
}
