//! Binding and session-info projections.

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;

use crate::binding::RootTemplateFact;
use crate::binding::{OpenRouterRoutingSelection, ProtocolSelection, TargetSelection};
use crate::sessions::SessionRecord;

use super::rebind::durable_tree_id;
use super::{HostConfig, HostEvent, HostTargetAuthority, Sender, SessionInfoFacts, TopologyRows};

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
        TerminalPhase::RecoveryPending => ("error", "needs recovery · next prompt recovers"),
        TerminalPhase::Idle => ("idle", "turn complete"),
        TerminalPhase::AdmissionRejected => ("idle", "prompt not admitted"),
        // Status is a latest-state observation: publishing `idle` here would
        // replace the `interrupted` phase before any consumer could read it.
        TerminalPhase::Interrupted => return,
    };
    let _ = events.send(HostEvent::Status {
        phase: phase.into(),
        queue_len,
        detail: Some(detail.into()),
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
    opened: &crate::agent::AttachedCodingSession,
) {
    let _ = events.send(HostEvent::LastUserPromptAt(
        opened.last_operator_input_unix_ms(),
    ));
}

pub(super) fn send_binding<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    config: &HostConfig<P>,
) {
    let _ = events.send(HostEvent::Binding {
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
    let _ = events.send(HostEvent::SessionInfo(session_info_view(config)));
}

/// Builds the Session projection from the bound session (spec 02 §5.5).
pub(super) fn session_info_view<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
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
        #[cfg(any(test, feature = "test-support"))]
        HostTargetAuthority::Injected { .. } => None,
    };
    plakous_host_protocol::views::presentation::SessionInfoView {
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
        let _ = events.send(HostEvent::Status {
            phase: "preparing".into(),
            queue_len: config.session.pending_queue.len(),
            detail: Some(line.to_owned()),
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    use crate::sessions::SessionRecord;
    use plakous::subscriptions::NativeProvider;

    /// Spec 02 §5.5: the Session sub-tab states the bound route's kernel, the
    /// resolved tool policy and the admitted options for every vertical.
    #[test]
    fn session_info_facts_cover_every_vertical() {
        use crate::binding::{
            OpenRouterRoutingSelection, ProtocolSelection, TargetSelection, WireBinding,
        };

        let codex = session_info_facts(
            &TargetSelection::Native,
            Some(NativeProvider::Codex),
            "direct",
            Some("medium"),
            "gpt-5.6-luna",
        );
        assert_eq!(codex.api_kernel, Some("Responses"));
        assert_eq!(
            codex.thinking,
            "reasoning summaries (auto) requested at medium"
        );
        assert_eq!(codex.tool_policy, "Direct");
        assert_eq!(codex.admitted_options, vec!["effort = medium".to_owned()]);

        let fast_codex = session_info_facts(
            &TargetSelection::NativePriority,
            Some(NativeProvider::Codex),
            "native-ptc",
            Some("low"),
            "gpt-5.6-luna",
        );
        assert_eq!(fast_codex.tool_policy, "Code Mode (native-ptc)");
        assert_eq!(
            fast_codex.admitted_options,
            vec![
                "effort = low".to_owned(),
                "Fast (extra = priority)".to_owned()
            ]
        );

        let grok = session_info_facts(
            &TargetSelection::Native,
            Some(NativeProvider::Xai),
            "direct",
            None,
            "grok-4.6",
        );
        assert_eq!(grok.api_kernel, Some("Responses"));
        assert!(grok.admitted_options.is_empty());
        // No effort means no reasoning configuration on the wire, and the row
        // says so instead of implying a thought block is coming.
        assert_eq!(
            grok.thinking,
            "not offered by grok-4.6 at default effort (choose an effort to request it)"
        );

        let claude = session_info_facts(
            &TargetSelection::Native,
            Some(NativeProvider::Claude),
            "js-ptc",
            Some("high"),
            "claude-sonnet-5",
        );
        assert_eq!(claude.api_kernel, Some("Messages"));
        assert_eq!(claude.tool_policy, "js-ptc");
        assert_eq!(
            claude.thinking,
            "not offered by claude-sonnet-5 at high: the Claude OAuth route returns thinking without a body (adaptive thinking is still requested)"
        );

        for (selection, kernel, option) in [
            (
                ProtocolSelection::Auto,
                "Chat Completions",
                "protocol = chat (auto)",
            ),
            (
                ProtocolSelection::Chat,
                "Chat Completions",
                "protocol = chat",
            ),
            (
                ProtocolSelection::Responses,
                "Responses",
                "protocol = responses",
            ),
            (
                ProtocolSelection::Messages,
                "Messages",
                "protocol = messages",
            ),
        ] {
            let openrouter = session_info_facts(
                &TargetSelection::OpenRouter {
                    wire: WireBinding::resolve(selection),
                    routing: OpenRouterRoutingSelection::GatewayManaged,
                },
                None,
                "py-ptc",
                None,
                "openrouter/auto",
            );
            assert_eq!(openrouter.api_kernel, Some(kernel), "{selection:?}");
            assert_eq!(openrouter.tool_policy, "py-ptc");
            assert_eq!(
                openrouter.admitted_options,
                vec![
                    option.to_owned(),
                    "openrouter routing = gateway-managed".to_owned()
                ],
                "{selection:?}"
            );
        }
    }

    /// The Session context row states the admitted envelope and its evidence
    /// class, never a guessed hard cap.
    #[test]
    fn session_context_facts_state_window_admitted_input_and_evidence() {
        use plakous::model::{CapacityRule, ContextEvidenceClass, SafeLimit};

        let reference = session_context_facts(
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(1_000_000),
                maximum_projected_input: 1_000_000,
                auto_compact_input: None,
            },
            &ContextEvidenceClass::ReleaseVerified,
        );
        assert_eq!(reference.reported_window, Some(1_000_000));
        assert_eq!(reference.admitted_input, Some(1_000_000));
        assert_eq!(reference.evidence, "release-verified");

        let shared = session_context_facts(
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(272_000),
            },
            &ContextEvidenceClass::ReleaseVerified,
        );
        assert_eq!(shared.reported_window, Some(272_000));
        assert_eq!(shared.admitted_input, Some(272_000));

        let separate = session_context_facts(
            CapacityRule::Separate {
                input: SafeLimit::GuaranteedLowerBound(128_000),
                output: SafeLimit::Exact(16_000),
            },
            &ContextEvidenceClass::ReleaseVerified,
        );
        assert_eq!(separate.reported_window, None);
        assert_eq!(separate.admitted_input, Some(128_000));
    }

    /// Spec 02 §5.5's Session "Root template" is a template *name* — the one
    /// the epoch froze, else the file's `[root] agent` when the session is
    /// actually bound to it. It is never the session title, which is the
    /// operator's first prompt truncated to 48 chars and only looks like a name.
    #[tokio::test]
    async fn the_session_root_template_is_a_template_name_never_the_session_title() {
        use crate::agent_tree_protocol::WriterMode;

        let root = tempfile::tempdir().unwrap();
        let paths = crate::runtime::test_kit::MemoryRows::new();
        let mut session = SessionRecord::new(root.path().to_path_buf());
        session.title = Some("add a regression test for the compose row".to_owned());

        // No config, no tree: unknown. Never the title.
        assert_eq!(session_root_template(&paths, &session, None), None);

        let fact = Some(RootTemplateFact {
            name: "scout".to_owned(),
            account: "codex".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
        });

        // A session bound by explicit --account/--model is not running `[root]`,
        // so naming that template would be an invented claim.
        session.account = Some("claude".to_owned());
        session.model = Some("claude-opus-5".to_owned());
        assert_eq!(session_root_template(&paths, &session, fact.as_ref()), None);

        // The session's own tuple is the `[root] agent` template's: name it.
        session.account = Some("codex".to_owned());
        session.model = Some("gpt-5.6-luna".to_owned());
        assert_eq!(
            session_root_template(&paths, &session, fact.as_ref()).as_deref(),
            Some("scout")
        );

        // A running epoch is authority over the file for what is bound now.
        let tree_id = durable_tree_id(&paths, &session);
        let mut tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
            &tree_id,
            "root",
            "owner",
            root.path().to_path_buf(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let mut snapshot = tree.snapshot.clone();
        snapshot.root_template = Some("frozen-root".to_owned());
        tree.install_snapshot(snapshot);
        crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
        assert_eq!(
            session_root_template(&paths, &session, fact.as_ref()).as_deref(),
            Some("frozen-root")
        );
        crate::topology_runtime::force_release(&tree_id).await;
    }
}
