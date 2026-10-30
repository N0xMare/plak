//! Binding and session-info projections.

use crate::surface_protocol::ContentEvent;
use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;
use plakous_host_protocol::status::{DiagnosticText, HostPhase, HostStatusDetail};

use crate::binding::RootTemplateFact;
use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, TargetSelection};
use crate::sessions::SessionRecord;

use super::rebind::durable_tree_id;
use super::{HostConfig, HostTargetAuthority, Sender, SessionInfoFacts, TopologyRows};

/// The resting phase a finished turn leaves on the status strip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalPhase {
    /// The turn completed or failed ordinarily; the host is idle.
    Idle,
    /// No turn was admitted for the submitted prompt.
    AdmissionRejected,
    /// The turn ended by interrupt; the settled-cancel `interrupted` status
    /// already published is the resting phase and must not be overwritten.
    Interrupted,
    /// The turn's outcome is uncertain; the next action recovers the session.
    RecoveryPending,
}

pub(super) fn emit_terminal_status<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    phase: TerminalPhase,
    queue_len: usize,
) {
    let (phase, detail) = match phase {
        TerminalPhase::RecoveryPending => (HostPhase::Error, HostStatusDetail::RecoveryPending),
        TerminalPhase::Idle => (HostPhase::Idle, HostStatusDetail::TurnComplete),
        TerminalPhase::AdmissionRejected => (HostPhase::Idle, HostStatusDetail::PromptNotAdmitted),
        // Status is a latest-state observation: publishing `idle` here would
        // replace the `interrupted` phase before any consumer could read it.
        TerminalPhase::Interrupted => return,
    };
    let _ = events.send(ContentEvent::Status {
        phase,
        queue_len,
        detail: Some(detail),
    });
}

pub fn effort_label(effort: ReasoningEffort) -> String {
    match effort {
        ReasoningEffort::Low => "low".into(),
        ReasoningEffort::Medium => "medium".into(),
        ReasoningEffort::High => "high".into(),
        ReasoningEffort::XHigh => "xhigh".into(),
        ReasoningEffort::Max => "max".into(),
        ReasoningEffort::Ultra => "ultra".into(),
        ReasoningEffort::None => "none".into(),
        ReasoningEffort::TokenBudget(n) => format!("tokens:{n}"),
        _ => "medium".into(),
    }
}

/// Seeds the idle clock of a surface after an attach (spec 02 §5.4).
pub(super) fn send_last_user_prompt_at<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &HostConfig<P>,
    opened: &crate::agent::AttachedCodingSession,
) {
    let _ = events.send(ContentEvent::LastUserPromptAt(fork_scoped_prompt_at(
        config.store.as_ref(),
        &config.session,
        opened.last_operator_input_unix_ms(),
    )));
}

/// A fork inherits canonical history, not its source's operator clock. The
/// projected lineage and rows prove an operator has written since that fork;
/// rebinds preserve the boundary, and host-authored wakes create no user row.
fn fork_scoped_prompt_at(
    store: &dyn crate::session_store::SessionRecordStore,
    session: &SessionRecord,
    canonical: Option<u64>,
) -> Option<u64> {
    use crate::sessions::TranscriptRow;
    use crate::sessions::projector::WorkflowSettlement;
    let mut current = session.clone();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.id.clone()) {
            return None;
        }
        let (Some(source), Some(operation)) = (
            current.source_product_session_id.as_deref(),
            current.source_workflow_operation_id.as_deref(),
        ) else {
            return canonical;
        };
        if current.carried_forward_from().is_some() {
            return canonical;
        }
        let source_intents = store.load_intents(source).ok()?;
        let request = source_intents.workflow_request(operation)?;
        match request.kind {
            crate::sessions::SessionWorkflowKind::Fork => {
                let source_views = store.load_views(source).ok()??;
                let WorkflowSettlement::Published { target } = source_views
                    .state
                    .workflows
                    .get(operation)?
                    .settled
                    .as_ref()?
                else {
                    return None;
                };
                let views = store.load_views(&session.id).ok()??;
                let boundary = views
                    .state
                    .lineage
                    .iter()
                    .find(|step| step.branch_id == target.branch_id)?
                    .from_seq;
                return views
                    .rows
                    .iter()
                    .any(|(key, row)| {
                        key.anchor > boundary
                            && matches!(
                                row,
                                TranscriptRow::User { .. } | TranscriptRow::Steer { .. }
                            )
                    })
                    .then_some(canonical)
                    .flatten();
            }
            crate::sessions::SessionWorkflowKind::Rebind(_) => {
                current = store.load(source).ok()?;
            }
            _ => return canonical,
        }
    }
}

pub(super) fn send_binding<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &HostConfig<P>,
) {
    let _ = events.send(ContentEvent::Binding {
        session_id: Some(config.session.id.clone()),
        account: config.session.account.clone(),
        model: Some(config.model_id.clone()),
        effort: config.session.effort.clone(),
        extra: (config.session.target_selection == TargetSelection::NativePriority)
            .then(|| "priority".to_owned()),
        composition: config.session.composition.into(),
        protocol: match &config.session.target_selection {
            TargetSelection::OpenRouter { wire, .. } => match wire.selection {
                ProtocolSelection::Auto => plakous_host_protocol::ProtocolSelection::Auto,
                ProtocolSelection::Chat => plakous_host_protocol::ProtocolSelection::Chat,
                ProtocolSelection::Responses => plakous_host_protocol::ProtocolSelection::Responses,
                ProtocolSelection::Messages => plakous_host_protocol::ProtocolSelection::Messages,
            },
            TargetSelection::Platform { provider } => match provider.protocol() {
                crate::binding::ResolvedProtocol::Responses => {
                    plakous_host_protocol::ProtocolSelection::Responses
                }
                crate::binding::ResolvedProtocol::Messages => {
                    plakous_host_protocol::ProtocolSelection::Messages
                }
                crate::binding::ResolvedProtocol::Chat => {
                    plakous_host_protocol::ProtocolSelection::Chat
                }
            },
            TargetSelection::Native | TargetSelection::NativePriority => {
                plakous_host_protocol::ProtocolSelection::Auto
            }
        },
        effective_presentation: crate::agent::effective_presentation_label(
            config.session.composition,
            config.target.native_provider(),
            config
                .target
                .native_resolved()
                .and_then(|resolved| resolved.native_tool_mode()),
        )
        .to_owned(),
    });
    send_session_info(events, config);
}

/// Projects the typed session facts the Usage Session section renders
/// (spec 02 §5.5). Published beside every binding and again after every admitted
/// root turn, so attach, rebind and the turn counter all stay exact.
pub(super) fn send_session_info<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &HostConfig<P>,
) {
    let _ = events.send(ContentEvent::SessionInfo(session_info_view(config)));
}

pub(super) fn send_session_info_with_storage<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &HostConfig<P>,
    storage: &crate::session_store::StorageAdmission,
) {
    let _ = events.send(ContentEvent::SessionInfo(session_info_view_with_storage(
        config, storage,
    )));
}

/// Builds the Session projection from the bound session (spec 02 §5.5).
pub(super) fn session_info_view<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> plakous_host_protocol::views::presentation::SessionInfoView {
    session_info_view_with_storage(config, &config.store.storage_admission())
}

fn session_info_view_with_storage<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    storage: &crate::session_store::StorageAdmission,
) -> plakous_host_protocol::views::presentation::SessionInfoView {
    let facts = session_info_facts(
        &config.session.target_selection,
        config.target.native_provider(),
        config.resolved_composition_label(),
        config.session.effort.as_deref(),
        &config.model_id,
    );
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let tree_epoch = crate::topology_runtime::lookup(&tree_id)
        .and_then(|runtime| runtime.read(|tree| tree.state).ok())
        .map(|state| state.as_str().to_owned());
    let context = match &config.target {
        HostTargetAuthority::Native { resolved, .. } => resolved
            .configuration()
            .effective()
            .descriptor()
            .context_contract()
            .map(|contract| {
                session_context_facts(
                    contract.capacity().rule(),
                    contract.capacity().evidence_class(),
                )
            }),
        HostTargetAuthority::OpenRouter { evidence, .. } => evidence.as_deref().map(|evidence| {
            plakous_host_protocol::views::presentation::SessionContextView {
                reported_window: Some(evidence.evidence.context_length()),
                admitted_input: None,
                evidence: crate::context::EvidenceClass::CustomEnvelope
                    .label()
                    .to_owned(),
            }
        }),
        HostTargetAuthority::Platform { .. } => None,

        HostTargetAuthority::Registered { .. } => None,
    };
    plakous_host_protocol::views::presentation::SessionInfoView {
        storage_level: match storage {
            crate::session_store::StorageAdmission::Available => {
                plakous_host_protocol::views::presentation::StorageLevel::Available
            }
            crate::session_store::StorageAdmission::Warning(_) => {
                plakous_host_protocol::views::presentation::StorageLevel::Warning
            }
            crate::session_store::StorageAdmission::Refused(_) => {
                plakous_host_protocol::views::presentation::StorageLevel::Refused
            }
        },
        host_version: plakous::VERSION.to_owned(),
        session_id: config.session.id.clone(),
        working_dir: config.cwd.display().to_string(),
        // Spec 02 §5.5's "root template" is the template name the
        // running epoch froze, not the session title — which is the
        // operator's first prompt truncated to 48 chars. A session bound by
        // flags froze no template name, and that is reported as unknown
        // rather than filled in with something that reads like one.
        root_template: session_root_template(
            &config.topology_rows(),
            &config.session,
            config.root_template.as_ref(),
        ),
        root_model: Some(config.model_id.clone()),
        root_effort: config.session.effort.clone(),
        thinking: facts.thinking,
        tool_policy: Some(facts.tool_policy.to_owned()),
        admitted_options: facts.admitted_options,
        api_kernel: facts.api_kernel.map(ToOwned::to_owned),
        turn_number: Some(config.session.root_turn_count),
        tree_epoch,
        context,
    }
}

pub(super) fn session_info_facts(
    target_selection: &TargetSelection,
    native_provider: Option<NativeProvider>,
    resolved_composition_label: &str,
    effort: Option<&str>,
    model: &str,
) -> SessionInfoFacts {
    // The label is the resolved selector, so `plakous-default` never
    // reaches here; an unknown label is reported verbatim rather than guessed.
    let tool_policy = match resolved_composition_label {
        "direct" => "Direct",
        "native-ptc" => "Code Mode (native-ptc)",
        "js-ptc" => "js-ptc",
        "py-ptc" => "py-ptc",
        "py-sdk" => "py-sdk",
        _ => "not resolved",
    };
    let mut admitted_options = Vec::new();
    if let Some(effort) = effort.filter(|value| !value.is_empty()) {
        admitted_options.push(format!("effort = {effort}"));
    }
    if *target_selection == TargetSelection::NativePriority {
        admitted_options.push("Fast (extra = priority)".to_owned());
    }
    // Auth selects one exact URL/API profile, so the kernel is a property of
    // the resolved route, never a guess about the model. Plak resolves an
    // OpenRouter `auto` once, to Chat, before calling Plakous.
    let api_kernel = match target_selection {
        TargetSelection::Native | TargetSelection::NativePriority => {
            native_provider.and_then(|provider| match provider {
                NativeProvider::Codex | NativeProvider::Xai => Some("Responses"),
                NativeProvider::Claude => Some("Messages"),
                // A provider this build does not name is left unstated
                // rather than guessed.
                _ => None,
            })
        }
        TargetSelection::Platform { provider } => Some(match provider.protocol() {
            crate::binding::ResolvedProtocol::Responses => "Responses",
            crate::binding::ResolvedProtocol::Messages => "Messages",
            crate::binding::ResolvedProtocol::Chat => "Chat Completions",
        }),
        TargetSelection::OpenRouter { wire, routing } => {
            admitted_options.push(format!(
                "protocol = {}{}",
                match wire.resolved {
                    crate::binding::ResolvedProtocol::Chat => "chat",
                    crate::binding::ResolvedProtocol::Responses => "responses",
                    crate::binding::ResolvedProtocol::Messages => "messages",
                },
                if wire.selection == ProtocolSelection::Auto {
                    " (auto)"
                } else {
                    ""
                }
            ));
            match routing {
                OpenRouterRoutingSelection::GatewayManaged => {
                    admitted_options.push("openrouter routing = gateway-managed".to_owned());
                }
            }
            Some(match wire.resolved {
                crate::binding::ResolvedProtocol::Chat => "Chat Completions",
                crate::binding::ResolvedProtocol::Responses => "Responses",
                crate::binding::ResolvedProtocol::Messages => "Messages",
            })
        }
    };
    SessionInfoFacts {
        tool_policy,
        admitted_options,
        api_kernel,
        thinking: thinking_request_statement(target_selection, native_provider, effort, model),
    }
}

/// What the bound route is asked for by way of visible reasoning, stated from
/// the same request policy the provider configuration is compiled from
/// (`crate::agent`, `ResponsesRequestPolicy`, `translate_reasoning`), so the
/// Session tab can say why a thought block did or did not appear.
///
/// Without a chosen effort the harness sends no reasoning configuration at all
/// (`ReasoningPolicy::Disabled`), so nothing visible is requested; the model may
/// still reason privately at its default. A route whose kernel streams no
/// reasoning surface is stated as not offered rather than left silent.
pub(super) fn thinking_request_statement(
    target_selection: &TargetSelection,
    native_provider: Option<NativeProvider>,
    effort: Option<&str>,
    model: &str,
) -> String {
    let Some(effort) = effort.filter(|value| !value.is_empty()) else {
        return format!(
            "not offered by {model} at default effort (choose an effort to request it)"
        );
    };
    match target_selection {
        TargetSelection::Native | TargetSelection::NativePriority => {
            match native_provider {
                Some(NativeProvider::Codex) => {
                    format!("reasoning summaries (auto) requested at {effort}")
                }
                Some(NativeProvider::Xai) => {
                    format!("reasoning summaries (concise) requested at {effort}")
                }
                // A model can bill 56 reasoning tokens
                // while every `thinking` block arrives empty with only a
                // signature. The Messages stream binds such a block `Omitted`
                // and the host suppresses it, so no thought block
                // can render on this route however much the model thinks.
                Some(NativeProvider::Claude) => format!(
                    "not offered by {model} at {effort}: the Claude OAuth route returns thinking without a body (adaptive thinking is still requested)"
                ),
                _ => format!("not offered by {model} at {effort}"),
            }
        }
        // OpenRouter requests effort only; no summary is asked for, and the
        // route streams reasoning only when the upstream model emits it.
        TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::OpenAi,
        } => {
            format!("reasoning summaries (auto) requested at {effort}")
        }
        TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::Anthropic,
        } => {
            format!("adaptive thinking requested at {effort}; shown if returned")
        }
        TargetSelection::OpenRouter { .. }
        | TargetSelection::Platform {
            provider: crate::binding::PlatformProvider::Xai,
        } => {
            format!("not requested from {model} at {effort}; shown only if the route streams it")
        }
    }
}

/// The bound model's context envelope as the Session sub-tab states it.
pub(super) fn session_context_facts(
    rule: plakous::model::CapacityRule,
    evidence_class: &plakous::model::ContextEvidenceClass,
) -> plakous_host_protocol::views::presentation::SessionContextView {
    use plakous::model::{CapacityRule, ContextEvidenceClass};
    let (reported_window, admitted_input) = match rule {
        CapacityRule::ProviderReservedInput {
            reported_context_window,
            maximum_projected_input,
            ..
        } => (reported_context_window, Some(maximum_projected_input)),
        CapacityRule::SharedWindow { total }
        | CapacityRule::SharedWindowWithOutputCap { total, .. } => {
            (Some(total.lower_bound()), Some(total.lower_bound()))
        }
        CapacityRule::Separate { input, .. } => (None, Some(input.lower_bound())),
        _ => (None, None),
    };
    let evidence = match evidence_class {
        ContextEvidenceClass::ReleaseVerified => crate::context::EvidenceClass::ReleaseVerified,
        ContextEvidenceClass::ReferenceClientEnvelope { .. } => {
            crate::context::EvidenceClass::ReferenceClientEnvelope
        }
        ContextEvidenceClass::CustomEnvelope { .. } => {
            crate::context::EvidenceClass::CustomEnvelope
        }
        _ => crate::context::EvidenceClass::Unavailable,
    };
    plakous_host_protocol::views::presentation::SessionContextView {
        reported_window,
        admitted_input,
        evidence: evidence.label().to_owned(),
    }
}

/// The template name behind this session's root bind (spec 02 §5.5).
///
/// Two sources, in authority order. A running epoch froze a root template name,
/// and that is what the epoch is actually bound to. Without one — no tree in the
/// hub, or a tree bound before the name was frozen — the file's `[root] agent`
/// answers, but **only** when the session's own bound tuple matches that
/// template: a session bound by explicit `--account` / `--model` is not running
/// `[root]`, and naming it would be an invented claim (§5.1).
///
/// It is never the session title. That is the operator's first prompt truncated
/// to 48 chars, which merely looks like a name under a §5.5 label.
pub(super) fn session_root_template(
    rows: &dyn TopologyRows,
    session: &SessionRecord,
    configured: Option<&RootTemplateFact>,
) -> Option<String> {
    if let Some(frozen) = crate::topology_runtime::lookup(&durable_tree_id(rows, session))
        .and_then(|runtime| {
            runtime
                .read(|tree| tree.snapshot.root_template.clone())
                .ok()
        })
        .flatten()
    {
        return Some(frozen);
    }
    let template = configured?;
    let matches_session = session.account.as_deref() == Some(template.account.as_str())
        && session.model.as_deref() == Some(template.model.as_str());
    matches_session.then(|| template.name.clone())
}

pub(super) fn plan_state_authority(
    store: &dyn crate::session_store::SessionRecordStore,
    session: &SessionRecord,
) -> crate::CliResult<String> {
    let mut current = session.clone();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.id.clone()) {
            return Err(crate::CliFailure::new(
                "fork plan authority has cyclic lineage",
            ));
        }
        let (Some(source), Some(operation)) = (
            current.source_product_session_id.as_deref(),
            current.source_workflow_operation_id.as_deref(),
        ) else {
            return Ok(current.id);
        };
        if current.carried_forward_from().is_some() {
            return Ok(current.id);
        }
        let intents = store.load_intents(source).map_err(crate::CliFailure::new)?;
        if !intents
            .workflow_request(operation)
            .is_some_and(|request| request.kind == crate::sessions::SessionWorkflowKind::Fork)
        {
            return Ok(current.id);
        }
        current = store.load(source).map_err(crate::CliFailure::new)?;
    }
}

pub(super) fn emit_agent_plan_notes<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &HostConfig<P>,
    stderr_buf: &[u8],
) {
    if stderr_buf.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(stderr_buf);
    for line in text.lines() {
        let _ = events.send(ContentEvent::Status {
            phase: HostPhase::Preparing,
            queue_len: config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::PlanDiagnostic {
                diagnostic: DiagnosticText::new(line.to_owned()),
            }),
        });
    }
}

#[cfg(test)]
mod tests;
