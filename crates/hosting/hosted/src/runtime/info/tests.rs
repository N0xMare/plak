#![allow(
    clippy::too_many_lines,
    reason = "Session-info scenarios populate every usage vertical to verify the complete typed information projection."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

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
    assert_eq!(grok.admitted_options, [] as [String; 0]);
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
        tools: crate::tool_plan::TemplateToolSelection::default(),
        tool_sections: std::collections::BTreeMap::new(),
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
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None)
        .await
        .unwrap();
    assert_eq!(
        session_root_template(&paths, &session, fact.as_ref()).as_deref(),
        Some("frozen-root")
    );
    crate::topology_runtime::force_release(&tree_id).await;
}
