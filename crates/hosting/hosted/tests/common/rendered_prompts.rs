//! Assembled model-visible prompt goldens for product routes.
//!
//! Included from `agent.rs` under `#[cfg(test)]`. The inner `#[cfg(test)]`
//! module keeps `tools/checks/check_workspace.py` from treating golden helpers as
//! production CLI code. No production behaviour.

#[cfg(test)]
#[allow(
    clippy::too_many_lines,
    reason = "offline golden renderer walks the full product matrix"
)]
pub(crate) mod suite {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use plakous::agent::{HarnessProfileSpec, ReasoningEffort, ResolvedContextSourceFact};
    use plakous::model::{
        Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ModelId, ToolName,
    };
    use plakous::subscriptions::NativeProvider;
    use plakous::tools::CodingPresentationExt as _;
    use plakous::tools::{
        CodexMinimalLocalCodeModeV1, CodingPresentation, CompiledSurface, FrozenToolRegistry,
        PROCESS_TOOL_PROJECTION_REVISION, ProductJavaScriptCompositionV1,
        ProviderModelProjectionIdentityV2, PythonPtcPresentationV1, PythonPtcV1, ToolProjectionMap,
    };

    use super::super::rendered_wire::suite as wire;
    use super::super::{
        assignment_start_prompt, coding_tool_registry_with_extensions, enroll_product_instructions,
        selected_harness_profile,
    };
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::{
        Delegation, MAX_DIRECT_CHILDREN, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES,
        TemplateName,
    };
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    use crate::binding::{
        CompositionSelection as ProductCompositionSelection, PlatformProvider,
        ResolvedProtocol as ProductResolvedProtocol,
    };
    use crate::cell_budget::DEFAULT_MAX_AGENTS_PER_CELL;
    use crate::child_prompts::ChildAssignmentStartKind;
    use crate::orchestration::{
        BindHints, EpochBounds, OrchestrationSnapshot, RootBind, RosterBinding, RosterInput,
    };
    use crate::tools::{ProductToolExtensions, product_surface_with_fence};
    use crate::topology_extensions::{topology_tool_extensions, topology_tool_extensions_in};

    pub(crate) const SYNTHETIC_ROOT: &str = "/work/repo";
    pub(crate) const SYNTHETIC_CWD: &str = "/work/repo/src";
    const SYNTHETIC_OS: &str = "macos";
    pub(crate) const WORKSPACE_AGENTS: &str = "repo specific AGENTS.md file content here";
    const CHILD_TASK: &str = "Review src/parser.rs for missing error paths and return a summary.";
    const NESTED_PARENT_TASK: &str =
        "Coordinate implement and review children for src/parser.rs and return a summary.";
    const CHILD_SCHEMA: &str =
        r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#;
    const CHILD_TURN_TOKEN: u64 = 1;
    pub(crate) const OPENROUTER_MODEL: &str = "openrouter/synthetic-model";
    pub(crate) const FIRST_TURN_USER: &str = "hello";

    pub(crate) struct RouteSpec {
        pub(crate) file_stem: String,
        #[allow(
            dead_code,
            reason = "matrix metadata; scenario line uses provider_wire + composition"
        )]
        pub(crate) title: String,
        pub(crate) composition: ProductCompositionSelection,
        pub(crate) provider_wire: String,
        pub(crate) provider: ProviderKind,
        pub(crate) occupancy: Occupancy,
        pub(crate) model: &'static str,
        pub(crate) code_mode: bool,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ProviderKind {
        Codex,
        Grok,
        Claude,
        OpenRouter {
            wire: ProductResolvedProtocol,
        },
        /// A direct API account, through the production API profile.
        Platform(PlatformProvider),
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Occupancy {
        /// Product root with the tree off: coding catalog only.
        SingleAgent,
        /// Product root with the tree on: spawn granted.
        Root,
        /// Spawned child that can spawn: `spawn_agent` + `submit_result`.
        NestedParent,
        /// Spawned leaf: `submit_result`, no `spawn_agent`.
        Child,
    }

    pub(crate) struct OccupancyTools {
        pub root: ProductToolExtensions,
        pub nested_parent: ProductToolExtensions,
        pub child: ProductToolExtensions,
        /// The same hubs on a Grok node: the await and interrupt slots spelled
        /// as Grok's output and kill tools.
        pub grok_root: ProductToolExtensions,
        pub grok_nested_parent: ProductToolExtensions,
        /// The same hubs on a pi (OpenRouter) node: the await and interrupt
        /// slots spelled as pi's `wait` and `stop`.
        pub pi_root: ProductToolExtensions,
        pub pi_nested_parent: ProductToolExtensions,
    }

    impl ProviderKind {
        fn stem_prefix(self) -> &'static str {
            match self {
                Self::Codex => "codex",
                Self::Grok => "grok",
                Self::Claude => "claude",
                Self::OpenRouter {
                    wire: ProductResolvedProtocol::Chat,
                } => "openrouter-chat",
                Self::OpenRouter {
                    wire: ProductResolvedProtocol::Responses,
                } => "openrouter-responses",
                Self::OpenRouter {
                    wire: ProductResolvedProtocol::Messages,
                } => "openrouter-messages",
                Self::Platform(PlatformProvider::OpenAi) => "openai-api",
                Self::Platform(PlatformProvider::Xai) => "xai-api",
                Self::Platform(PlatformProvider::Anthropic) => "anthropic-api",
            }
        }

        pub(crate) fn native(self) -> Option<NativeProvider> {
            match self {
                Self::Codex => Some(NativeProvider::Codex),
                Self::Grok => Some(NativeProvider::Xai),
                Self::Claude => Some(NativeProvider::Claude),
                Self::OpenRouter { .. } | Self::Platform(_) => None,
            }
        }

        pub(crate) fn presentation(self) -> CodingPresentation {
            match self {
                Self::Codex => CodingPresentation::Codex,
                Self::Grok => CodingPresentation::Grok,
                Self::Claude => CodingPresentation::Claude,
                Self::OpenRouter { .. } => CodingPresentation::Pi,
                Self::Platform(provider) => crate::coding_routes::platform_presentation(provider),
            }
        }
    }

    pub(crate) fn child_assignment_start_prompt(
        composition: ProductCompositionSelection,
    ) -> String {
        assignment_start_prompt(
            ChildAssignmentStartKind::from_composition(crate::prompt_inputs::prompt_composition(
                composition,
            )),
            "reviewer",
            crate::agent_tree_protocol::Tier::ReadOnly,
            "reviewer",
            CHILD_TASK,
            CHILD_SCHEMA,
            CHILD_TURN_TOKEN,
            None,
            None,
        )
    }

    pub(crate) fn nested_parent_assignment_start_prompt(
        composition: ProductCompositionSelection,
    ) -> String {
        assignment_start_prompt(
            ChildAssignmentStartKind::from_composition(crate::prompt_inputs::prompt_composition(
                composition,
            )),
            "supervisor",
            crate::agent_tree_protocol::Tier::ReadOnly,
            "supervisor",
            NESTED_PARENT_TASK,
            CHILD_SCHEMA,
            CHILD_TURN_TOKEN,
            None,
            None,
        )
    }

    pub(crate) fn first_turn_user_text(spec: &RouteSpec) -> String {
        match spec.occupancy {
            Occupancy::Child => child_assignment_start_prompt(spec.composition),
            Occupancy::NestedParent => nested_parent_assignment_start_prompt(spec.composition),
            Occupancy::SingleAgent | Occupancy::Root => FIRST_TURN_USER.to_owned(),
        }
    }

    fn occupancy_scenario_phrase(occupancy: Occupancy) -> &'static str {
        match occupancy {
            Occupancy::SingleAgent => "single-agent root (no tree)",
            Occupancy::Root => "tree-enabled root",
            Occupancy::NestedParent => "nested parent (spawn-capable child)",
            Occupancy::Child => "spawned leaf assignment",
        }
    }

    fn occupancy_detail_line(occupancy: Occupancy) -> &'static str {
        match occupancy {
            Occupancy::SingleAgent => {
                "Coding catalog only; no orchestration tools. First user turn: `hello`."
            }
            Occupancy::Root => {
                "Spawn granted; frozen roster implement + review (leaves). First user turn: `hello`."
            }
            Occupancy::NestedParent => {
                "`spawn_agent` and `submit_result` exposed (spawn-capable `supervisor` template). First user turn is the genesis Start prompt."
            }
            Occupancy::Child => {
                "`submit_result` exposed; no `spawn_agent`. First user turn is the genesis Start prompt."
            }
        }
    }

    fn scenario_description(spec: &RouteSpec) -> String {
        format!(
            "{}, `{}`, {}, {}.\n{}\nSynthetic workspace `{SYNTHETIC_ROOT}`, cwd `{SYNTHETIC_CWD}`, OS `{SYNTHETIC_OS}`.",
            spec.provider_wire,
            spec.composition.as_str(),
            spec.model,
            occupancy_scenario_phrase(spec.occupancy),
            occupancy_detail_line(spec.occupancy),
        )
    }

    fn render_headers_table(spec: &RouteSpec) -> String {
        let mut body = String::from("| name | value |\n| --- | --- |\n");
        for (name, value) in wire::review_headers(spec) {
            body.push_str(&format!("| {name} | {value} |\n"));
        }
        body
    }

    fn extract_request_fence(markdown: &str) -> Option<String> {
        let marker = "## Request\n\n```jsonc\n";
        let start = markdown.find(marker)? + marker.len();
        let rest = &markdown[start..];
        let end = rest.rfind("\n```")?;
        Some(rest[..end].to_owned())
    }

    fn occupancy_label(occupancy: Occupancy) -> &'static str {
        match occupancy {
            Occupancy::SingleAgent => "single-agent root (no spawn)",
            Occupancy::Root => "tree root with spawn + implement/review roster",
            Occupancy::NestedParent => "nested parent (spawn + submit_result)",
            Occupancy::Child => "spawned leaf",
        }
    }

    fn occupancy_stem(occupancy: Occupancy) -> &'static str {
        match occupancy {
            Occupancy::SingleAgent => "root.single-agent",
            Occupancy::Root => "root",
            Occupancy::NestedParent => "nested-parent",
            Occupancy::Child => "child",
        }
    }

    fn composition_stem(composition: ProductCompositionSelection) -> &'static str {
        composition.as_str()
    }

    fn provider_display(provider: ProviderKind) -> &'static str {
        match provider {
            ProviderKind::Codex => "Codex OAuth",
            ProviderKind::Grok => "SuperGrok OAuth",
            ProviderKind::Claude => "Claude OAuth",
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            } => "OpenRouter Chat",
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            } => "OpenRouter Responses",
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            } => "OpenRouter Messages",
            ProviderKind::Platform(PlatformProvider::OpenAi) => "OpenAI API",
            ProviderKind::Platform(PlatformProvider::Xai) => "xAI API",
            ProviderKind::Platform(PlatformProvider::Anthropic) => "Anthropic API",
        }
    }

    fn composition_display(composition: ProductCompositionSelection) -> &'static str {
        match composition {
            ProductCompositionSelection::Direct | ProductCompositionSelection::FollowCatalog => {
                "Direct"
            }
            ProductCompositionSelection::NativePtc => "Code Mode",
            ProductCompositionSelection::JsPtc => "JS PTC",
            ProductCompositionSelection::PyPtc => "Python PTC",
        }
    }

    fn provider_wire_label(
        provider: ProviderKind,
        composition: ProductCompositionSelection,
    ) -> String {
        let extra = match (provider, composition) {
            (
                ProviderKind::Codex | ProviderKind::Platform(PlatformProvider::OpenAi),
                ProductCompositionSelection::NativePtc,
            ) => " (Code Mode exec/wait)",
            (_, ProductCompositionSelection::JsPtc) => " (exec_js/wait_js)",
            (_, ProductCompositionSelection::PyPtc) => " (exec_py)",
            _ => "",
        };
        match provider {
            ProviderKind::Codex => format!("Codex OAuth / Responses{extra}"),
            ProviderKind::Grok => format!("SuperGrok OAuth / Responses{extra}"),
            ProviderKind::Claude => format!("Claude OAuth / Messages{extra}"),
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            } => format!("OpenRouter Chat Completions{extra}"),
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            } => format!("OpenRouter Responses{extra}"),
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            } => format!("OpenRouter Messages{extra}"),
            ProviderKind::Platform(PlatformProvider::Anthropic) => {
                format!("Anthropic API / Messages{extra}")
            }
            ProviderKind::Platform(provider) => {
                format!(
                    "{} / Responses{extra}",
                    provider_display(ProviderKind::Platform(provider))
                )
            }
        }
    }

    fn route_title(
        provider: ProviderKind,
        composition: ProductCompositionSelection,
        occupancy: Occupancy,
        model: &str,
        file_stem: &str,
    ) -> String {
        match file_stem {
            "codex.direct.root.single-agent" => {
                format!("Codex OAuth Direct ({model}), single-agent root (no spawn)")
            }
            "codex.native-ptc.root" => {
                format!(
                    "Codex OAuth Code Mode ({model}), tree root with spawn + implement/review roster"
                )
            }
            "codex.native-ptc.child" => {
                format!("Codex OAuth Code Mode ({model}), spawned leaf")
            }
            "grok.direct.root.single-agent" => {
                "SuperGrok OAuth Direct (grok-4.6), single-agent root (no spawn)".to_owned()
            }
            "grok.js-ptc.root" => {
                "SuperGrok OAuth JS PTC (grok-4.6), tree root with spawn + implement/review roster"
                    .to_owned()
            }
            "grok.py-ptc.root.single-agent" => {
                "SuperGrok OAuth Python PTC (grok-4.6), single-agent root (no spawn)".to_owned()
            }
            "openrouter-chat.direct.root.single-agent" => {
                "OpenRouter Chat Direct, single-agent root (inspect_image omitted)".to_owned()
            }
            "openrouter-responses.direct.root.single-agent" => {
                "OpenRouter Responses Direct, single-agent root (inspect_image present)".to_owned()
            }
            "openrouter-chat.js-ptc.root" => {
                "OpenRouter Chat JS PTC, tree root with spawn + implement/review roster".to_owned()
            }
            _ => {
                let inspect = match (provider, composition, occupancy) {
                    (
                        ProviderKind::OpenRouter {
                            wire: ProductResolvedProtocol::Chat,
                        },
                        ProductCompositionSelection::Direct,
                        Occupancy::SingleAgent,
                    ) => " (inspect_image omitted)",
                    (
                        ProviderKind::OpenRouter {
                            wire: ProductResolvedProtocol::Responses,
                        },
                        ProductCompositionSelection::Direct,
                        Occupancy::SingleAgent,
                    ) => " (inspect_image present)",
                    _ => "",
                };
                format!(
                    "{provider} {composition} ({model}), {occupancy}{inspect}",
                    provider = provider_display(provider),
                    composition = composition_display(composition),
                    occupancy = occupancy_label(occupancy),
                )
            }
        }
    }

    pub(crate) fn routes() -> Vec<RouteSpec> {
        let mut specs = Vec::new();
        let providers = [
            ProviderKind::Codex,
            ProviderKind::Grok,
            ProviderKind::Claude,
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            },
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            },
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            },
        ];
        let compositions = [
            ProductCompositionSelection::Direct,
            ProductCompositionSelection::NativePtc,
            ProductCompositionSelection::JsPtc,
            ProductCompositionSelection::PyPtc,
        ];
        let occupancies = [
            Occupancy::SingleAgent,
            Occupancy::Root,
            Occupancy::NestedParent,
            Occupancy::Child,
        ];
        for provider in providers {
            for composition in compositions {
                if composition == ProductCompositionSelection::NativePtc
                    && provider != ProviderKind::Codex
                {
                    continue;
                }
                for occupancy in occupancies {
                    let (model, code_mode) = match (provider, composition) {
                        (ProviderKind::Codex, ProductCompositionSelection::NativePtc) => {
                            ("gpt-6.1-sol", true)
                        }
                        (ProviderKind::Codex, ProductCompositionSelection::Direct) => {
                            ("gpt-6.1-luna", false)
                        }
                        (ProviderKind::Codex, _) => ("gpt-5.6-luna", false),
                        (ProviderKind::Grok, _) => ("grok-4.6", false),
                        (ProviderKind::Claude, _) => ("claude-sonnet-5", false),
                        (ProviderKind::OpenRouter { .. }, _) => (OPENROUTER_MODEL, false),
                        (ProviderKind::Platform(_), _) => unreachable!("API rows: api_routes"),
                    };
                    let file_stem = format!(
                        "{}.{}.{}",
                        provider.stem_prefix(),
                        composition_stem(composition),
                        occupancy_stem(occupancy)
                    );
                    let title = route_title(provider, composition, occupancy, model, &file_stem);
                    specs.push(RouteSpec {
                        file_stem,
                        title,
                        composition,
                        provider_wire: provider_wire_label(provider, composition),
                        provider,
                        occupancy,
                        model,
                        code_mode,
                    });
                }
            }
        }
        specs.extend(api_routes());
        specs
    }

    /// Direct API accounts, single-agent root only: each composition the
    /// product's API table admits, through the production API profile and
    /// the route's Platform wire.
    fn api_routes() -> Vec<RouteSpec> {
        use ProductCompositionSelection::{Direct, JsPtc, NativePtc, PyPtc};
        [
            (PlatformProvider::OpenAi, "gpt-5.6-luna", Direct),
            (PlatformProvider::OpenAi, "gpt-5.6-sol", NativePtc),
            (PlatformProvider::OpenAi, "gpt-5.6-luna", JsPtc),
            (PlatformProvider::OpenAi, "gpt-5.6-luna", PyPtc),
            (PlatformProvider::Xai, "grok-4.6", Direct),
            (PlatformProvider::Xai, "grok-4.6", JsPtc),
            (PlatformProvider::Xai, "grok-4.6", PyPtc),
            (PlatformProvider::Anthropic, "claude-sonnet-5", Direct),
            (PlatformProvider::Anthropic, "claude-sonnet-5", JsPtc),
            (PlatformProvider::Anthropic, "claude-sonnet-5", PyPtc),
        ]
        .into_iter()
        .map(|(provider, model, composition)| {
            assert_eq!(
                crate::platform::api_composition(provider, model, composition),
                Ok(composition),
                "API render rows are compositions the API table admits"
            );
            let kind = ProviderKind::Platform(provider);
            let occupancy = Occupancy::SingleAgent;
            let file_stem = format!(
                "{}.{}.{}",
                kind.stem_prefix(),
                composition_stem(composition),
                occupancy_stem(occupancy)
            );
            RouteSpec {
                title: route_title(kind, composition, occupancy, model, &file_stem),
                file_stem,
                composition,
                provider_wire: provider_wire_label(kind, composition),
                provider: kind,
                occupancy,
                model,
                code_mode: composition == NativePtc,
            }
        })
        .collect()
    }

    pub(crate) fn excluded_routes() -> &'static [(&'static str, &'static str)] {
        &[
            (
                "native-ptc × {grok, claude, openrouter-chat, openrouter-responses, openrouter-messages} × {root, root.single-agent, nested-parent, child}",
                "native-ptc is the vertical's own PTC system; only Codex Code Mode is enrolled, and Grok, Claude and OpenRouter have none",
            ),
            (
                "{openai-api, xai-api, anthropic-api} × {root, nested-parent, child}; native-ptc × {xai-api, anthropic-api}",
                "API rows render the single-agent root only; native-ptc (Codex Code Mode) is enrolled only on CodeModeOnly OpenAI API rows",
            ),
        ]
    }

    pub(crate) fn workspace_root() -> PathBuf {
        let cwd = std::env::current_dir().expect("cwd");
        let mut dir = cwd.clone();
        loop {
            if dir.join("AGENTS.md").is_file() && dir.join("crates/hosting/hosted").is_dir() {
                return dir;
            }
            assert!(
                dir.pop(),
                "could not find the plak workspace root from {}",
                cwd.display()
            );
        }
    }

    /// Local, git-ignored artifact root for rendered prompt/wire/lifecycle
    /// output. Never read by a test: renders are written only under
    /// `PLAK_UPDATE_GOLDENS=1`, and correctness is pinned in code.
    pub(crate) fn artifact_root() -> PathBuf {
        std::env::var_os("PLAK_PROMPT_ARTIFACT_DIR").map_or_else(
            || workspace_root().join("target/prompt-renders"),
            PathBuf::from,
        )
    }

    pub(crate) fn golden_dir() -> PathBuf {
        artifact_root().join("rendered")
    }

    pub(crate) fn wire_dir() -> PathBuf {
        golden_dir().join("wire")
    }

    /// Lowercase hex sha256 of a rendered artifact, used by the in-code pin
    /// tables in place of a committed file comparison.
    pub(crate) fn render_digest(text: &str) -> String {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn print_pins_requested() -> bool {
        std::env::var("PLAK_PRINT_PROMPT_PINS").is_ok_and(|value| value == "1")
    }

    pub(crate) fn pin_macos(text: &str) -> String {
        text.replace(
            &format!("OS: {}.", std::env::consts::OS),
            &format!("OS: {SYNTHETIC_OS}."),
        )
    }

    /// `pub(crate)` so lifecycle goldens can compile the same dummy projection
    /// target.
    pub(crate) fn synthetic_target(seed: &[u8]) -> ProviderModelProjectionIdentityV2 {
        let configuration: EffectiveProviderConfigurationId = serde_json::from_value(
            serde_json::to_value(
                Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[seed])
                    .expect("synthetic configuration digest"),
            )
            .expect("synthetic configuration encoding"),
        )
        .expect("synthetic configuration identity");
        ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Standard,
            "test.responses",
            "test.oauth",
            "test.product-composition.v1",
            "test-family",
            "test-model",
            "test-model",
            configuration,
            PROCESS_TOOL_PROJECTION_REVISION,
        )
        .expect("synthetic projection target")
    }

    pub(crate) fn instruction_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("instruction fixture");
        let home = tmp.path().join("home");
        let root = tmp.path().join("workspace");
        let cwd = root.join("src");
        std::fs::create_dir_all(&home).expect("product home");
        std::fs::create_dir_all(&cwd).expect("workspace cwd");
        std::fs::write(root.join("AGENTS.md"), WORKSPACE_AGENTS).expect("workspace AGENTS.md");
        (tmp, home, root, cwd)
    }

    /// One working-set card as the product freezes a `[templates.*]` file into
    /// the epoch: everything not written on the card is the template schema's
    /// default, which is exactly `RosterBinding::leaf`'s.
    pub(crate) fn golden_card(name: &str, model: &str, may_spawn: bool) -> RosterBinding {
        let mut card = RosterBinding::leaf("codex", model);
        card.template = name.to_owned();
        card.description = if may_spawn {
            "nested supervisor; coordinates implementer and reviewer".to_owned()
        } else {
            format!("{model} worker")
        };
        if may_spawn {
            card.spawns = ["implementer", "reviewer"]
                .map(|name| TemplateName::parse(name).expect("golden template name"))
                .to_vec();
        }
        card
    }

    /// The epoch a `config.toml` with exactly `cards` in its working set and a
    /// `[root]` that names none of them freezes to. The product resolves this
    /// in `roster_input::roster_input_from_config`; the goldens build it
    /// directly so the render fixtures name no product configuration schema.
    pub(crate) fn golden_epoch(cards: Vec<RosterBinding>) -> OrchestrationSnapshot {
        let mut templates = BTreeMap::new();
        let mut working_set = std::collections::BTreeSet::new();
        for card in cards {
            let name = TemplateName::parse(&card.template).expect("golden template name");
            working_set.insert(name.clone());
            templates.insert(name, card);
        }
        OrchestrationSnapshot::freeze(RosterInput {
            workspace_idle_seconds: 600,
            workspace_budget_mib: 4096,
            child_retention_days: 7,
            child_retention_count: 128,
            root: RootBind {
                delegation: Delegation::default(),
                template: None,
                account: "codex".to_owned(),
                tuple_id: "gpt-5.6-sol".to_owned(),
                effort: None,
            },
            writer_mode: WriterMode::SingleLease,
            bounds: EpochBounds {
                max_depth: u16::from(MAX_TREE_DEPTH),
                max_children: MAX_DIRECT_CHILDREN as u16,
                max_nodes: MAX_TREE_NODES as u16,
                max_attempts: MAX_TREE_ATTEMPTS as u16,
                max_agents_per_cell: DEFAULT_MAX_AGENTS_PER_CELL,
            },
            require_review_evidence: false,
            transient_resumes: 1,
            working_set,
            templates,
            templates_digest: String::new(),
            bind_hints: golden_bind_hints(),
            tool_sections: BTreeMap::new(),
        })
    }

    /// The product's account roster as the goldens freeze it.
    pub(crate) fn golden_bind_hints() -> BindHints {
        BindHints::from_names(
            &[
                "codex",
                "chatgpt",
                "grok",
                "supergrok",
                "xai",
                "xai-oauth",
                "claude",
                "openrouter",
            ],
            &["openrouter"],
        )
    }

    fn implement_review_cards() -> Vec<RosterBinding> {
        let mut reviewer = golden_card("reviewer", "gpt-5.6-sol", false);
        reviewer.max_tier = crate::agent_tree_protocol::Tier::ReadOnly;
        vec![golden_card("implementer", "gpt-5.6-sol", false), reviewer]
    }

    fn orchestrate_nest_cards() -> Vec<RosterBinding> {
        let mut cards = implement_review_cards();
        cards.push(golden_card("supervisor", "gpt-5.6-sol", true));
        cards
    }

    fn bootstrap_golden_tree(workspace: &Path, cards: Vec<RosterBinding>) -> ProductAgentTree {
        let mut tree = ProductAgentTree::bootstrap_root(
            "golden-tree",
            "root",
            "root",
            workspace.to_path_buf(),
            "gpt-5.6-sol",
            WriterMode::SingleLease,
        )
        .expect("tree");
        tree.install_snapshot(golden_epoch(cards));
        tree
    }

    pub(crate) fn occupancy_tools(workspace: &Path) -> OccupancyTools {
        let mut leaf_tree = bootstrap_golden_tree(workspace, implement_review_cards());
        let spawned_leaf = leaf_tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "reviewer",
                    crate::agent_tree_protocol::Tier::ReadOnly,
                    "reviewer",
                    "review the diff",
                ),
                None,
            )
            .expect("spawn review child");
        let leaf_shared = Arc::new(Mutex::new(leaf_tree));
        let root =
            topology_tool_extensions(Arc::clone(&leaf_shared), "root").expect("root extensions");
        let grok_root = topology_tool_extensions_in(
            Arc::clone(&leaf_shared),
            "root",
            crate::agent_tree_tools::TopologyVocabulary::TaskTools,
        )
        .expect("grok root extensions");
        let pi_root = topology_tool_extensions_in(
            Arc::clone(&leaf_shared),
            "root",
            crate::agent_tree_tools::TopologyVocabulary::WaitStop,
        )
        .expect("pi root extensions");
        let child =
            topology_tool_extensions(leaf_shared, spawned_leaf.agent_id).expect("child extensions");

        let mut nest_tree = bootstrap_golden_tree(workspace, orchestrate_nest_cards());
        let spawned_parent = nest_tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "supervisor",
                    crate::agent_tree_protocol::Tier::ReadOnly,
                    "supervisor",
                    "Coordinate implementer and reviewer children for src/parser.rs and return a summary.",
                ),
                None,
            )
            .expect("spawn supervisor child");
        let nest_shared = Arc::new(Mutex::new(nest_tree));
        let grok_nested_parent = topology_tool_extensions_in(
            Arc::clone(&nest_shared),
            spawned_parent.agent_id.clone(),
            crate::agent_tree_tools::TopologyVocabulary::TaskTools,
        )
        .expect("grok nested-parent extensions");
        let pi_nested_parent = topology_tool_extensions_in(
            Arc::clone(&nest_shared),
            spawned_parent.agent_id.clone(),
            crate::agent_tree_tools::TopologyVocabulary::WaitStop,
        )
        .expect("pi nested-parent extensions");
        let nested_parent = topology_tool_extensions(nest_shared, spawned_parent.agent_id)
            .expect("nested-parent extensions");
        OccupancyTools {
            root,
            nested_parent,
            child,
            grok_root,
            grok_nested_parent,
            pi_root,
            pi_nested_parent,
        }
    }

    fn enroll(
        profile: HarnessProfileSpec,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        enroll_product_instructions(&profile, Some(home), root, cwd).expect("enroll AGENTS.md")
    }

    /// The program runtime a route runs under, as production selects it.
    pub(crate) fn route_runtime(spec: &RouteSpec) -> Option<plakous::tools::CompositionRuntime> {
        if spec.code_mode {
            Some(plakous::tools::CompositionRuntime::CodexNative)
        } else {
            spec.composition.runtime()
        }
    }

    /// The callable surface of one route — the production `product_surface`
    /// over the route's registry — from which every profile here takes its
    /// earned paragraphs and its PTC base.
    pub(crate) fn route_surface(
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
    ) -> CompiledSurface {
        product_surface_with_fence(
            plakous::tools::JobConfig::default(),
            spec.provider.presentation(),
            registry_for(spec, extras),
            synthetic_target(spec.file_stem.as_bytes()),
            extras,
            if matches!(spec.occupancy, Occupancy::NestedParent | Occupancy::Child) {
                super::super::SessionToolFence::OccupancyReadOnly
            } else {
                super::super::SessionToolFence::Full
            },
            route_runtime(spec),
        )
        .expect("route surface")
    }

    pub(crate) fn native_profile(
        provider: NativeProvider,
        model: &str,
        code_mode: bool,
        extras: &ProductToolExtensions,
        composition: ProductCompositionSelection,
        surface: &CompiledSurface,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        let extra: Vec<_> = extras.names().cloned().collect();
        let model = ModelId::new(model).expect("model");
        let profile = selected_harness_profile(
            provider,
            &model,
            None::<ReasoningEffort>,
            false,
            code_mode,
            extra.as_slice(),
            !extra.is_empty(),
            composition,
            surface,
            Path::new(SYNTHETIC_ROOT),
            Path::new(SYNTHETIC_CWD),
        )
        .expect("harness profile")
        .expect("new session has a harness profile");
        enroll(profile, home, root, cwd)
    }

    pub(crate) fn openrouter_profile(
        composition: ProductCompositionSelection,
        wire: ProductResolvedProtocol,
        extras: &ProductToolExtensions,
        surface: &CompiledSurface,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        let profile = crate::openrouter::openrouter_product_profile(
            composition,
            None,
            wire,
            OPENROUTER_MODEL,
            !extras.is_empty(),
            Path::new(SYNTHETIC_ROOT),
            Path::new(SYNTHETIC_CWD),
            surface,
        )
        .expect("openrouter profile");
        enroll(profile, home, root, cwd)
    }

    pub(crate) fn registry(
        composition: ProductCompositionSelection,
        attach_native: bool,
        extras: &ProductToolExtensions,
        authority: &str,
    ) -> FrozenToolRegistry {
        coding_tool_registry_with_extensions(authority, composition, attach_native, extras)
            .expect("coding registry")
    }

    pub(crate) fn registry_for(
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
    ) -> FrozenToolRegistry {
        match spec.composition {
            ProductCompositionSelection::NativePtc => registry(
                ProductCompositionSelection::FollowCatalog,
                true,
                extras,
                catalog_authority(spec),
            ),
            composition => registry(composition, false, extras, catalog_authority(spec)),
        }
    }

    pub(crate) fn extras_for(spec: &RouteSpec, tools: &OccupancyTools) -> ProductToolExtensions {
        let (root, nested_parent) = match spec.provider {
            ProviderKind::Grok => (&tools.grok_root, &tools.grok_nested_parent),
            ProviderKind::OpenRouter { .. } => (&tools.pi_root, &tools.pi_nested_parent),
            _ => (&tools.root, &tools.nested_parent),
        };
        match spec.occupancy {
            // An attached root without spawn tools still wakes (spec 02 §6.3).
            Occupancy::SingleAgent => ProductToolExtensions::empty()
                .with_host_facts(plakous_host_protocol::PromptHostFacts { host_wakes: true }),
            Occupancy::Root => root
                .clone()
                .with_host_facts(plakous_host_protocol::PromptHostFacts { host_wakes: true }),
            // The tree host wakes every child (spec 03 §5.2).
            Occupancy::NestedParent => nested_parent
                .clone()
                .with_host_facts(plakous_host_protocol::PromptHostFacts { host_wakes: true }),
            Occupancy::Child => tools
                .child
                .clone()
                .with_host_facts(plakous_host_protocol::PromptHostFacts { host_wakes: true }),
        }
    }

    /// Live protocol/family/model tuple for wire lowering. Markdown goldens keep
    /// the dummy `test.responses` target so catalog digests stay stable.
    pub(crate) fn live_projection_target(
        spec: &RouteSpec,
        effective: EffectiveProviderConfigurationId,
    ) -> ProviderModelProjectionIdentityV2 {
        let (class, protocol, route, profile, family) = match spec.provider {
            ProviderKind::Codex => (
                IntegrationClass::Standard,
                "openai.codex.responses",
                "openai.codex_subscription",
                "openai.codex.compat.2026-07-31.r1",
                "responses-family",
            ),
            ProviderKind::Grok => (
                IntegrationClass::Standard,
                "xai.responses.grok-subscription.2026-07",
                "xai.grok_subscription",
                "xai.grok.compat.2026-07-31.r1",
                "responses-family",
            ),
            ProviderKind::Claude => (
                IntegrationClass::Standard,
                "anthropic.messages",
                "anthropic.claude_oauth",
                "anthropic.claude.oauth.compat.2026-08-20.r1",
                "messages-family",
            ),
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Chat,
            } => (
                IntegrationClass::Custom,
                "openrouter.chat-completions.v1",
                "openrouter.chat.platform:golden-wire",
                "openrouter.chat.dynamic-v1",
                "chat-completions-family",
            ),
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Responses,
            } => (
                IntegrationClass::Custom,
                "openrouter.responses.v1",
                "openrouter.responses.platform:golden-wire",
                "openrouter.responses.dynamic-custom-v1",
                "openrouter.responses",
            ),
            ProviderKind::OpenRouter {
                wire: ProductResolvedProtocol::Messages,
            } => (
                IntegrationClass::Custom,
                "openrouter.messages.v1",
                "openrouter.messages.platform:golden-wire",
                "openrouter.messages.dynamic-custom-v1",
                "openrouter.messages",
            ),
            ProviderKind::Platform(PlatformProvider::OpenAi) => (
                IntegrationClass::Standard,
                plakous_openai::OPENAI_RESPONSES_PROTOCOL_ID,
                plakous_openai::OPENAI_PLATFORM_ROUTE_ID,
                plakous::openai::OpenAiPlatformModelFacts::from_model_id(spec.model)
                    .expect("enrolled OpenAI API row")
                    .profile_id(),
                "responses-family",
            ),
            ProviderKind::Platform(PlatformProvider::Xai) => (
                IntegrationClass::Standard,
                "xai.platform.responses.2026-08",
                "xai.platform_api_key",
                "xai.platform.api-key",
                "responses-family",
            ),
            ProviderKind::Platform(PlatformProvider::Anthropic) => (
                IntegrationClass::Standard,
                "anthropic.messages",
                "anthropic.platform",
                plakous_anthropic::ANTHROPIC_API_ADAPTIVE_PROFILE_ID,
                "messages-family",
            ),
        };
        ProviderModelProjectionIdentityV2::new(
            class,
            protocol,
            route,
            profile,
            family,
            spec.model,
            spec.model,
            effective,
            PROCESS_TOOL_PROJECTION_REVISION,
        )
        .expect("live projection target")
    }

    pub(crate) fn extra_names(extras: &ProductToolExtensions) -> Vec<ToolName> {
        extras.names().cloned().collect()
    }

    pub(crate) fn composed_instruction_text(
        profile: &HarnessProfileSpec,
        facts: &[ResolvedContextSourceFact],
    ) -> String {
        let mut composed = pin_macos(profile.base_instructions());
        for fact in facts {
            if !composed.is_empty() {
                composed.push_str("\n\n");
            }
            composed.push_str(fact.content());
        }
        composed
    }

    pub(crate) fn catalog_authority(spec: &RouteSpec) -> &'static str {
        match (spec.provider, spec.composition) {
            (ProviderKind::Codex, ProductCompositionSelection::Direct) => "golden-codex-direct",
            (ProviderKind::Codex, ProductCompositionSelection::NativePtc) => {
                "golden-codex-native-ptc"
            }
            (ProviderKind::Codex, ProductCompositionSelection::JsPtc) => "golden-codex-js-ptc",
            (ProviderKind::Codex, ProductCompositionSelection::PyPtc) => "golden-codex-py-ptc",
            (ProviderKind::Grok, ProductCompositionSelection::Direct) => "golden-grok-direct",
            (ProviderKind::Grok, ProductCompositionSelection::JsPtc) => "golden-grok-js-ptc",
            (ProviderKind::Grok, ProductCompositionSelection::PyPtc) => "golden-grok-py-ptc",
            (ProviderKind::Claude, ProductCompositionSelection::Direct) => "golden-claude-direct",
            (ProviderKind::Claude, ProductCompositionSelection::JsPtc) => "golden-claude-js-ptc",
            (ProviderKind::Claude, ProductCompositionSelection::PyPtc) => "golden-claude-py-ptc",
            (ProviderKind::OpenRouter { .. }, ProductCompositionSelection::Direct) => {
                "golden-openrouter-direct"
            }
            (ProviderKind::OpenRouter { .. }, ProductCompositionSelection::JsPtc) => {
                "golden-openrouter-js-ptc"
            }
            (ProviderKind::OpenRouter { .. }, ProductCompositionSelection::PyPtc) => {
                "golden-openrouter-py-ptc"
            }
            (ProviderKind::Platform(provider), composition) => match (provider, composition) {
                (PlatformProvider::OpenAi, ProductCompositionSelection::Direct) => {
                    "golden-openai-api-direct"
                }
                (PlatformProvider::OpenAi, ProductCompositionSelection::NativePtc) => {
                    "golden-openai-api-native-ptc"
                }
                (PlatformProvider::OpenAi, ProductCompositionSelection::JsPtc) => {
                    "golden-openai-api-js-ptc"
                }
                (PlatformProvider::OpenAi, ProductCompositionSelection::PyPtc) => {
                    "golden-openai-api-py-ptc"
                }
                (PlatformProvider::Xai, ProductCompositionSelection::Direct) => {
                    "golden-xai-api-direct"
                }
                (PlatformProvider::Xai, ProductCompositionSelection::JsPtc) => {
                    "golden-xai-api-js-ptc"
                }
                (PlatformProvider::Xai, ProductCompositionSelection::PyPtc) => {
                    "golden-xai-api-py-ptc"
                }
                (PlatformProvider::Anthropic, ProductCompositionSelection::Direct) => {
                    "golden-anthropic-api-direct"
                }
                (PlatformProvider::Anthropic, ProductCompositionSelection::JsPtc) => {
                    "golden-anthropic-api-js-ptc"
                }
                (PlatformProvider::Anthropic, ProductCompositionSelection::PyPtc) => {
                    "golden-anthropic-api-py-ptc"
                }
                _ => "golden-unsupported",
            },
            _ => "golden-unsupported",
        }
    }

    /// The effort an API row binds: API accounts require `low` or `medium`.
    pub(crate) const API_EFFORT: ReasoningEffort = ReasoningEffort::Low;

    /// The production API profile (`api_product_profile`) over `surface`.
    pub(crate) fn api_profile(
        provider: PlatformProvider,
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
        surface: &CompiledSurface,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        let profile = crate::api_runtime::api_product_profile(
            provider.account(),
            spec.composition,
            Some(API_EFFORT),
            provider.protocol(),
            spec.model,
            !extras.is_empty(),
            Path::new(SYNTHETIC_ROOT),
            Path::new(SYNTHETIC_CWD),
            surface,
        )
        .expect("API profile");
        enroll(profile, home, root, cwd)
    }

    /// The harness profile one route assembles over `surface`.
    pub(crate) fn route_profile(
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
        surface: &CompiledSurface,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        match spec.provider {
            ProviderKind::Platform(provider) => {
                api_profile(provider, spec, extras, surface, home, root, cwd)
            }
            ProviderKind::OpenRouter { wire } => {
                openrouter_profile(spec.composition, wire, extras, surface, home, root, cwd)
            }
            ProviderKind::Codex | ProviderKind::Grok | ProviderKind::Claude => native_profile(
                spec.provider.native().expect("native provider"),
                spec.model,
                spec.code_mode,
                extras,
                spec.composition,
                surface,
                home,
                root,
                cwd,
            ),
        }
    }

    pub(crate) fn assemble_profile(
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        route_profile(spec, extras, &route_surface(spec, extras), home, root, cwd)
    }

    pub(crate) fn compile_projection(
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<ToolProjectionMap, String> {
        let extra = extra_names(extras);
        let kind = spec.provider.presentation();
        let registry = registry_for(spec, extras);
        match spec.composition {
            ProductCompositionSelection::NativePtc => {
                let surface = route_surface(spec, extras);
                // The API route compiles its nested catalog as production
                // does: Codex OAuth order, every surface tool but `inspect_image`.
                let nested = match spec.provider {
                    ProviderKind::Platform(_) => {
                        crate::api_runtime::code_mode_nested_names(&surface)
                    }
                    _ => surface.exposed_names().to_vec(),
                };
                CodexMinimalLocalCodeModeV1::compile_for_catalog(
                    plakous::tools::JobConfig::default(),
                    registry,
                    target,
                    &nested,
                )
                .map(|descriptor| descriptor.outer_projection().clone())
                .map_err(|error| error.to_string())
            }
            ProductCompositionSelection::JsPtc => {
                let names = route_surface(spec, extras).exposed_names().to_vec();
                ProductJavaScriptCompositionV1::compile_for_kind(
                    plakous::tools::JobConfig::default(),
                    registry,
                    target,
                    kind,
                    names,
                )
                .map(|descriptor| descriptor.outer_projection().clone())
                .map_err(|error| error.to_string())
            }
            ProductCompositionSelection::PyPtc => {
                let names = route_surface(spec, extras).exposed_names().to_vec();
                PythonPtcV1::compile_for_kind(
                    plakous::tools::JobConfig::default(),
                    registry,
                    target,
                    PythonPtcPresentationV1::StructuredFunctions,
                    kind,
                    names,
                )
                .map(|descriptor| descriptor.outer_projection().clone())
                .map_err(|error| error.to_string())
            }
            ProductCompositionSelection::Direct | ProductCompositionSelection::FollowCatalog => {
                let omit_inspect = matches!(
                    spec.provider,
                    ProviderKind::OpenRouter {
                        wire: ProductResolvedProtocol::Chat,
                    }
                );
                let full = kind
                    .projection_map_with_extensions(
                        plakous::tools::JobConfig::default(),
                        registry.clone(),
                        target.clone(),
                        &extra,
                    )
                    .map_err(|error| error.to_string())?;
                let surface = route_surface(spec, extras);
                let mut omit = full
                    .entries()
                    .iter()
                    .filter(|entry| !surface.exposed_names().contains(entry.exposed_name()))
                    .map(|entry| entry.exposed_name().as_str())
                    .collect::<Vec<_>>();
                if omit_inspect {
                    omit.push("inspect_image");
                }
                if omit.is_empty() {
                    kind.projection_map_with_extensions(
                        plakous::tools::JobConfig::default(),
                        registry,
                        target,
                        &extra,
                    )
                } else {
                    kind.projection_map_with_extensions_omitting(
                        plakous::tools::JobConfig::default(),
                        registry,
                        target,
                        &extra,
                        &omit,
                    )
                }
                .map_err(|error| error.to_string())
            }
        }
    }

    fn render_harness_footprint(footprint: &wire::HarnessFootprint) -> String {
        let total = footprint.total_bytes();
        let instruction_tokens = wire::HarnessFootprint::approx_tokens(footprint.instruction_bytes);
        let tool_tokens = wire::HarnessFootprint::approx_tokens(footprint.tool_bytes);
        let total_tokens = wire::HarnessFootprint::approx_tokens(total);
        format!(
            "Harness environment: **{total}** bytes · **{total_tokens}** tokens\n\
             Method: {}.\n\
             This total is dialect costume plus family/product/PTC instructions plus the compact `tools[]` JSON. It excludes enrolled `AGENTS.md` ({} bytes) and this scenario's user/assignment message ({} bytes).\n\n\
             | part | bytes | approx tokens |\n\
             | --- | ---: | ---: |\n\
             | instructions | {} | {instruction_tokens} |\n\
             | tools | {} | {tool_tokens} |\n\
             | **total** | **{total}** | **{total_tokens}** |\n",
            wire::HarnessFootprint::token_count_method(),
            footprint.enrolled_source_bytes,
            footprint.assignment_bytes,
            footprint.instruction_bytes,
            footprint.tool_bytes,
        )
    }

    fn render_route(
        spec: &RouteSpec,
        home: &Path,
        root: &Path,
        cwd: &Path,
        tools: &OccupancyTools,
    ) -> String {
        let artifacts = wire::render_wire(spec, home, root, cwd, tools);
        let mut markdown = String::new();
        markdown.push_str(&render_harness_footprint(&artifacts.footprint));
        markdown.push_str(&format!("\n# `{}`\n\n", spec.file_stem));
        markdown.push_str(&scenario_description(spec));
        markdown.push_str("\n\n## Headers\n\n");
        markdown.push_str(&render_headers_table(spec));
        markdown.push_str("\n## Request\n\n```jsonc\n");
        markdown.push_str(&artifacts.request_jsonc);
        markdown.push_str("```\n");
        markdown
    }

    /// Collects every `name` / `description` string carried by the lowered
    /// `tools[]` array, across responses, chat-completions and messages shapes.
    fn lowered_tool_strings(body: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        fn walk(
            value: &serde_json::Value,
            names: &mut Vec<String>,
            descriptions: &mut Vec<String>,
        ) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, child) in map {
                        if let Some(text) = child.as_str() {
                            match key.as_str() {
                                "name" => names.push(text.to_owned()),
                                "description" => descriptions.push(text.to_owned()),
                                _ => {}
                            }
                        }
                        walk(child, names, descriptions);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item, names, descriptions);
                    }
                }
                _ => {}
            }
        }
        let mut names = Vec::new();
        let mut descriptions = Vec::new();
        if let Some(tools) = body.get("tools") {
            walk(tools, &mut names, &mut descriptions);
        }
        (names, descriptions)
    }

    /// Every product extension tool must be model-visible verbatim: either as a
    /// lowered `tools[]` entry, or spelled out in the harness instruction text
    /// (the code-mode presentations describe the same tools in prose).
    fn extension_visibility_failures(
        spec: &RouteSpec,
        extras: &ProductToolExtensions,
        instruction_text: &str,
        body: &serde_json::Value,
    ) -> Vec<String> {
        if extras.is_empty() {
            return Vec::new();
        }
        let registry = registry_for(spec, extras);
        let (names, descriptions) = lowered_tool_strings(body);
        let mut failures = Vec::new();
        for name in extra_names(extras) {
            let definition = registry.definition(&name).unwrap_or_else(|| {
                panic!("{}: extension {name} is not registered", spec.file_stem)
            });
            let description = definition.description();
            if !names
                .iter()
                .any(|candidate| candidate.contains(name.as_str()))
                && !instruction_text.contains(name.as_str())
            {
                failures.push(format!(
                    "{}: extension tool `{name}` is not model-visible in tools[] or instructions",
                    spec.file_stem
                ));
            }
            if !descriptions
                .iter()
                .any(|candidate| candidate.contains(description))
                && !instruction_text.contains(description)
            {
                failures.push(format!(
                    "{}: description of extension tool `{name}` does not appear verbatim in tools[] or instructions",
                    spec.file_stem
                ));
            }
        }
        failures
    }

    /// sha256 of the rendered Markdown for every included route. This is the
    /// correctness pin that replaced the committed `docs/prompts/rendered`
    /// goldens: nothing is read from disk, and a prompt change must be
    /// re-pinned here deliberately.
    ///
    /// Re-pin with:
    /// `PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_ -- --nocapture`
    // Claude pins include explicit account/device/session fixture identities and
    // body attestation, alongside instruction text and complete tool payloads.
    const PROMPT_RENDER_PINS: &[(&str, &str)] = &[
        (
            "codex.direct.root.single-agent",
            "a570b0325954439a7fbcffacfc2022019183ff51d3987ace59c9b11bc0f29539",
        ),
        (
            "codex.direct.root",
            "94394d9a89813d88d318b39a2b1a066ad9f00b6689de49cb6581e1e6ef94a730",
        ),
        (
            "codex.direct.nested-parent",
            "3808bf8313d1ad169c7fa7b9f275e6f44cea1fcd0634e39ba69eb4cf377fec5e",
        ),
        (
            "codex.direct.child",
            "8e7178615177193425c72dd8781df901084b85d3c310b4c9122cd49eb9de2b26",
        ),
        (
            "codex.native-ptc.root.single-agent",
            "05b87044b87c36c607cd226849603a46b43f07974ce04cedc2e25813cfa8cbc5",
        ),
        (
            "codex.native-ptc.root",
            "b2aec9334ee6f70670bea17585c5861fba6fb9f94e38771a413ec37eab8bfc91",
        ),
        (
            "codex.native-ptc.nested-parent",
            "172c1d95360b8ff17a946f4b0e46b6142513bfcb4c2b7ae42d8bff056ca175d3",
        ),
        (
            "codex.native-ptc.child",
            "83c1e77468a78e2e061f6ea4958c9312056cd9e5fff808f641298151cab311e6",
        ),
        (
            "codex.js-ptc.root.single-agent",
            "6f8f07d822ac310169cfbeffd56101fd8536b4933eb2f208ad79e46ffe31981d",
        ),
        (
            "codex.js-ptc.root",
            "c9edc98002ffe79f7d3de12e264c463263ab4e7a4c49995b7d83dd458b288059",
        ),
        (
            "codex.js-ptc.nested-parent",
            "89bb8015de0fcceac3aa5882b78571e6849130470e52817cb2044d646ece4503",
        ),
        (
            "codex.js-ptc.child",
            "ec5781e098de1aa05827244378ad1a7d34a8e2e6be76a4f4e4aea0ef9df85fb9",
        ),
        (
            "codex.py-ptc.root.single-agent",
            "0bf48c563d780e2679cef64b20b004a0531c4580544113fad3f3363c89e1fdb5",
        ),
        (
            "codex.py-ptc.root",
            "be4800e224d0745f134d21af31486602456fd07e3c7c296ba7d04eddbb6faca6",
        ),
        (
            "codex.py-ptc.nested-parent",
            "9fd7f01468f518cee9a8297c4722a1036617ba334c89af2ae7c77ca9159fc5ec",
        ),
        (
            "codex.py-ptc.child",
            "38f06fbccf1826bfc77f7a1c8cb2719125da4790d1b44f7d592ee7425895c885",
        ),
        (
            "grok.direct.root.single-agent",
            "80dfdc68247bc08a9801d760149ed23d1123c371152889079be991ea1f3132c1",
        ),
        (
            "grok.direct.root",
            "652029df083b7d3583efb574b5f0c8dc3a16a6ee561339e99d880976c1795150",
        ),
        (
            "grok.direct.nested-parent",
            "74206893431a92d2da3e90c58a2115430fb428b24c7ad040c36fdba04daae7fe",
        ),
        (
            "grok.direct.child",
            "5be6bc8202643872a060ecf9ca488a3462921b254136bcf3e9bb7e3c60fde3b5",
        ),
        (
            "grok.js-ptc.root.single-agent",
            "25eefd4a5302009b4b7dd8b2a656a5dda4ae7b8995c89ad400a090e4e8ad5f1f",
        ),
        (
            "grok.js-ptc.root",
            "6cfe340b2149a7fa8c218bc647457aca0e1497052162b0ec49aaca009d13643a",
        ),
        (
            "grok.js-ptc.nested-parent",
            "59d3517d717751b219c36f6176f6c880d8288e4d57b2dc1ceb2288d375954708",
        ),
        (
            "grok.js-ptc.child",
            "873b046f675bfd9be20a856efa7d5820ea15ae4cea96b2b72fd38e540230e082",
        ),
        (
            "grok.py-ptc.root.single-agent",
            "414827c25f90031063fd62611b82e531e75f56bf8729ae9506586b4706f02588",
        ),
        (
            "grok.py-ptc.root",
            "10f65065505e3fb86f8ffc85c6ef35abbfabf4bb93fe00ab72af59ce8bfb6e38",
        ),
        (
            "grok.py-ptc.nested-parent",
            "c280daebc519683730d96b6440145bc732455b68f71457124d1a302b43359af8",
        ),
        (
            "grok.py-ptc.child",
            "bd4978ee5db6189616720d7fca6fcdfef0d277a63378902ceaf02cf47a9dc005",
        ),
        (
            "claude.direct.root.single-agent",
            "4b81471eed6f5c9771bf003450214cae0022abade03d2fc2d81077b7eb1c3472",
        ),
        (
            "claude.direct.root",
            "c1b0857c39431cdfe30b7111b030b26ce9a079ee439ea4ba59e6135501427e3f",
        ),
        (
            "claude.direct.nested-parent",
            "0f425aeec557b74b01eeebdc3924690c670780b9d86cabb04c0a1e68c8386fb3",
        ),
        (
            "claude.direct.child",
            "1375dd73f4eaa276b03616397a03427557df3e8e5585749e907682ef167fa6c6",
        ),
        (
            "claude.js-ptc.root.single-agent",
            "d23f53366a79ee7b0315284c2657b84b521d0a3e7a4802a5831f053956a87633",
        ),
        (
            "claude.js-ptc.root",
            "2258aee128a6805a61f9828a7eb93f94f1efe58e44f4e11160ac38393a19ce34",
        ),
        (
            "claude.js-ptc.nested-parent",
            "c714faefa1ac873bb2bd79be87e05b88ccb6ef8489f12bee147f8fddd14fe9e1",
        ),
        (
            "claude.js-ptc.child",
            "82e904ef78068769ed388333604fe4ba4a21e3886f99c1dbdce43d37b5d45e70",
        ),
        (
            "claude.py-ptc.root.single-agent",
            "9c58c522a0dbe4fbf24d6bdb42b0249759c199529e91a7bde205f02d5b1d4b6b",
        ),
        (
            "claude.py-ptc.root",
            "bbb879ae3da3bdc054c896d02a376ecef1965f9202f63ac4e15e25ae61ca1431",
        ),
        (
            "claude.py-ptc.nested-parent",
            "decc01941ae56d533c242245f15181a6bba61cc71acd3b893e734aef256d30c0",
        ),
        (
            "claude.py-ptc.child",
            "00e947a319dc150fd9113cf59ebd7d3709b6f774d2f383135a67bf4e14bfd49d",
        ),
        (
            "openrouter-chat.direct.root.single-agent",
            "476b68ec33eec52f609c95eb2312f500e587ddff93a8ecbda0a27d5b3dd3058c",
        ),
        (
            "openrouter-chat.direct.root",
            "fd9642fbc577ca933cf7ab180e54d4943774ac794fab67620e6676404f8f9b8d",
        ),
        (
            "openrouter-chat.direct.nested-parent",
            "30ddf959839274b31076a09e2def93465610f9b151a3f3a6efd1051918a541c7",
        ),
        (
            "openrouter-chat.direct.child",
            "7cf2b35fd5f719cf5b7053c37e4f2e839abb2bfaf28137131b0c41a9f1932935",
        ),
        (
            "openrouter-chat.js-ptc.root.single-agent",
            "e60b61db93aa50adc21334af206dfc502e2ccbf1edfe4e6c8427da3d079fc005",
        ),
        (
            "openrouter-chat.js-ptc.root",
            "9096328161c01e6c1dcde69e5ac98ca3f0866fb642d30f5ad660cdf70a85d31a",
        ),
        (
            "openrouter-chat.js-ptc.nested-parent",
            "3cd52525ca0e42060a3a83d689a208d5f8a41ff76a3221a40684251c5d7eaaf0",
        ),
        (
            "openrouter-chat.js-ptc.child",
            "9a03ff205256d358c48eec03475d09f6d3a5d9509cb62fad0ba16a85b0d4fc31",
        ),
        (
            "openrouter-chat.py-ptc.root.single-agent",
            "6ec692e4dcd85536887011606f5b4d5abb0a3d145eeadd481fa4d2059a996caa",
        ),
        (
            "openrouter-chat.py-ptc.root",
            "9b38a4255bac122d36e68d2541dd59927ae72f914c8756e2c6ffb86a65eb8119",
        ),
        (
            "openrouter-chat.py-ptc.nested-parent",
            "9740da55ecbe71dd741e0b4550867aa97eb35ba8ac268b09e52d3d72930261ed",
        ),
        (
            "openrouter-chat.py-ptc.child",
            "16b01b1864fe8f329fc51c53df484d15e4cee7bb06b75550980b3506b05bc822",
        ),
        (
            "openrouter-responses.direct.root.single-agent",
            "4907c89248f92f7fed4c95690df7f29011e494090d4460bcb7a7def18b17aaac",
        ),
        (
            "openrouter-responses.direct.root",
            "6fb15c66bfe5113491c4528a9b8f63ca46f6f130d4666301fda083a43df67fdf",
        ),
        (
            "openrouter-responses.direct.nested-parent",
            "4be901848a3b276bb634f69be88776eb5568a6feb155c548472428f11a842592",
        ),
        (
            "openrouter-responses.direct.child",
            "bd3cac368e01d593febb573027f3cc3ea10f3c52628aed599ba4ebca809bdabf",
        ),
        (
            "openrouter-responses.js-ptc.root.single-agent",
            "7055691f66339a812cfb948e257d41e12c4739d0e59aced133db5c5efb4bf137",
        ),
        (
            "openrouter-responses.js-ptc.root",
            "95160e96afa51a7076a9fb2cd73c0f41b18f7aade98c059c85f54584b6bcac44",
        ),
        (
            "openrouter-responses.js-ptc.nested-parent",
            "d1ec49250256e3f3ff5ea5d2e11b34f00174f19748366ba2b9520c66d9f25dbb",
        ),
        (
            "openrouter-responses.js-ptc.child",
            "4d243bff378f87d980daed00e9e0d3eaa3eb1fec35b6b7bd00ababdd45028886",
        ),
        (
            "openrouter-responses.py-ptc.root.single-agent",
            "5ccb765c03021b0f55f94873658f52d774120e244e09aa82080f688f6fac5a02",
        ),
        (
            "openrouter-responses.py-ptc.root",
            "25536106b4354e9b94aee79151a0ec79e356a31f6c590265ef6de1351e2436a2",
        ),
        (
            "openrouter-responses.py-ptc.nested-parent",
            "6254034a92c96031b8a0d0b1ef1a87fb76b18499c367337ec28faaac9e6ccead",
        ),
        (
            "openrouter-responses.py-ptc.child",
            "840206f809febeac5aef77997906e199edc23407541b4870f448b2c10864cbb1",
        ),
        (
            "openrouter-messages.direct.root.single-agent",
            "a071c0097764fcb477f925cafafaa3f0f3c683e34ba2727a7e34543d407f7d65",
        ),
        (
            "openrouter-messages.direct.root",
            "ea2fa51ee36899f4f135d0bfa0193fa005f11b7cb2a5af4aed8986b2fa1255e2",
        ),
        (
            "openrouter-messages.direct.nested-parent",
            "0751e94b9ad4795900859a46100c404fe73dc50f1c51fce222e5b95b300af693",
        ),
        (
            "openrouter-messages.direct.child",
            "e3bcb6a088495f9edb389f58de4294c4f550f163f4c0121f45b0a02727944659",
        ),
        (
            "openrouter-messages.js-ptc.root.single-agent",
            "9f34c4de4032b129e4ce8699a80b33bea7f517f915eaabd95b910d2947bbe7a7",
        ),
        (
            "openrouter-messages.js-ptc.root",
            "3e1ecc9f0ea75933daf19d407898ee7b4985805d2c737a40f7ce09ede4a115c1",
        ),
        (
            "openrouter-messages.js-ptc.nested-parent",
            "3ffdddd696b9aebfd6b59de09395a8d7b670bf141b7f164133d90a5e3f7d2d3c",
        ),
        (
            "openrouter-messages.js-ptc.child",
            "71b10b2979f9ab99da123ff9bf120f762920e54029634e9e591412f07e16e060",
        ),
        (
            "openrouter-messages.py-ptc.root.single-agent",
            "e3120be61728a45e615dbbb6d62b6bedd382fb1ac31c09d02703da24bef00bd5",
        ),
        (
            "openrouter-messages.py-ptc.root",
            "b3e78c91bb9e82839d25bfd5e65e1d4cf415825d640ffb833d72e44c0dc61fa1",
        ),
        (
            "openrouter-messages.py-ptc.nested-parent",
            "c982d949347138db7fef2f6b8cf8673ed645b4234f73c0c7c7e1c1a0277ddef6",
        ),
        (
            "openrouter-messages.py-ptc.child",
            "e58b6bd57c8862723a9f620bf706c20b1c214d2f00c3c8d70949678057b71534",
        ),
        (
            "openai-api.direct.root.single-agent",
            "a659e58ae0918dd4b25a0ac04b7754041d21f468120753e5dd6f67669dd2233f",
        ),
        (
            "openai-api.native-ptc.root.single-agent",
            "428734292f4f5b777238ea302e6c754c4a0555f3c5529b7ecea5465a582272d0",
        ),
        (
            "openai-api.js-ptc.root.single-agent",
            "180e6fa49ddb6f8c1d0a0403ff79dfd37784a16ee3ebf6d585d81d758270bbac",
        ),
        (
            "openai-api.py-ptc.root.single-agent",
            "3fc0998bd99e03bf9d68392a05ae93abfda2f473fe4c189c5d190993666ce359",
        ),
        (
            "xai-api.direct.root.single-agent",
            "a86c2a247108900e9e9cf31f5fa035b92354dfdd41761e28632fbc08e9432282",
        ),
        (
            "xai-api.js-ptc.root.single-agent",
            "12bdc1b63a0fa43c5f349719b11fda31a3a65ebd5a8238a7942971565034a937",
        ),
        (
            "xai-api.py-ptc.root.single-agent",
            "e6128a456cc47e85ac17ddc4e5112bf512190cca2d104c6b47646794cebcf11e",
        ),
        (
            "anthropic-api.direct.root.single-agent",
            "c2f86c9bebe2a7e8716899bf5cfc57880e0140c2cd698613b485d1498926bc67",
        ),
        (
            "anthropic-api.js-ptc.root.single-agent",
            "ed9a662c915bbd3a14b6bd0cfcd3e9adcc7421e218049adb84f024ac4f1dd586",
        ),
        (
            "anthropic-api.py-ptc.root.single-agent",
            "201611324d7eea5bd2f8a19ad65e37871074d04c485261eee176bc2bd14553eb",
        ),
    ];

    /// On a js-ptc, py-ptc, or OpenRouter hub, `spawn_agent` is
    /// callable only inside a cell (or is a Direct extra on OpenRouter). The
    /// one evaluator — `compile` over the callable surface — earns the
    /// topology paragraph for every one of them, and the route rendered from
    /// those contributions is byte-identical to the pinned render.
    #[test]
    fn ptc_and_openrouter_hubs_earn_the_topology_paragraph_through_compile() {
        use plakous::prompts::identity::{
            ASSIGNMENT_CONTRIBUTION_ID, PLAK_PRODUCT_TOPOLOGY_AGREEMENTS,
            PLAK_PRODUCT_TOPOLOGY_GROK_AGREEMENTS, PLAK_PRODUCT_TOPOLOGY_PI_AGREEMENTS,
            TOPOLOGY_CONTRIBUTION_ID, TOPOLOGY_GROK_CONTRIBUTION_ID, TOPOLOGY_PI_CONTRIBUTION_ID,
        };
        let (_tmp, home, root, cwd) = instruction_fixture();
        let tools = occupancy_tools(&root);
        let pins = PROMPT_RENDER_PINS
            .iter()
            .copied()
            .collect::<BTreeMap<_, _>>();
        let hubs = [
            "codex.js-ptc.root",
            "grok.py-ptc.root",
            "claude.js-ptc.nested-parent",
            "openrouter-chat.js-ptc.root",
            "openrouter-responses.py-ptc.nested-parent",
            "openrouter-messages.direct.root",
        ];
        let mut seen = 0;
        for spec in routes() {
            if !hubs.contains(&spec.file_stem.as_str()) {
                continue;
            }
            seen += 1;
            let extras = extras_for(&spec, &tools);
            let surface = route_surface(&spec, &extras);
            assert!(
                surface
                    .exposed_names()
                    .iter()
                    .any(|name| name.as_str() == "spawn_agent"),
                "{}: spawn_agent is callable on a hub",
                spec.file_stem
            );
            let ids = surface
                .contributions()
                .iter()
                .map(plakous::tools::PromptContribution::id)
                .collect::<Vec<_>>();
            // A Grok hub's agent await is its output tool and a pi
            // (OpenRouter) hub's is `wait`, so every spawner text it earns is
            // that pack's.
            let (topology_id, topology_text, child_wake, hub_wake) = match spec.provider {
                ProviderKind::Grok => (
                    TOPOLOGY_GROK_CONTRIBUTION_ID,
                    PLAK_PRODUCT_TOPOLOGY_GROK_AGREEMENTS,
                    "plak.agreements.child-wake-grok",
                    "plak.agreements.hub-wake-grok",
                ),
                ProviderKind::OpenRouter { .. } => (
                    TOPOLOGY_PI_CONTRIBUTION_ID,
                    PLAK_PRODUCT_TOPOLOGY_PI_AGREEMENTS,
                    "plak.agreements.child-wake-pi",
                    "plak.agreements.hub-wake-pi",
                ),
                _ => (
                    TOPOLOGY_CONTRIBUTION_ID,
                    PLAK_PRODUCT_TOPOLOGY_AGREEMENTS,
                    "plak.agreements.child-wake",
                    "plak.agreements.hub-wake",
                ),
            };
            let expected: &[&str] = match spec.occupancy {
                Occupancy::Root => &[topology_id, child_wake],
                Occupancy::NestedParent => &[
                    topology_id,
                    hub_wake,
                    ASSIGNMENT_CONTRIBUTION_ID,
                    "plak.agreements.assignment-wake",
                ],
                Occupancy::SingleAgent | Occupancy::Child => unreachable!("hub routes only"),
            };
            let mut expected = expected.to_vec();
            let process_guidance = match spec.provider {
                ProviderKind::Codex => "plak.agreements.process-codex",
                ProviderKind::OpenRouter { .. } => "plak.agreements.process-pi",
                ProviderKind::Grok => "plak.agreements.process-grok",
                ProviderKind::Claude => "plak.agreements.process-claude",
                ProviderKind::Platform(_) => unreachable!("API rows are single-agent roots"),
            };
            let process_guidance = format!("{process_guidance}-wake");
            expected.push(&process_guidance);
            assert_eq!(ids, expected, "{}", spec.file_stem);
            let (profile, _) = assemble_profile(&spec, &extras, &home, &root, &cwd);
            assert!(
                profile.base_instructions().contains(topology_text),
                "{}: the hub profile carries the topology paragraph",
                spec.file_stem
            );
            let digest = render_digest(&render_route(&spec, &home, &root, &cwd, &tools));
            assert_eq!(
                Some(&digest.as_str()),
                pins.get(spec.file_stem.as_str()),
                "{}: render through compile() must match the pinned route",
                spec.file_stem
            );
        }
        assert_eq!(seen, hubs.len());
    }

    /// The harness footprint a basic single node may spend, in approximate
    /// tokens: instructions plus tool schemas, exclusive of enrolled
    /// `AGENTS.md` and the turn's user or assignment message. Spec 05 states
    /// it as normative; this is the build gate.
    pub(crate) const SINGLE_NODE_TOKEN_BUDGET: usize = 4_000;

    #[test]
    fn filtered_route_instructions_name_only_presented_tools() {
        let (_tmp, home, root, cwd) = instruction_fixture();
        let tools = occupancy_tools(&root);
        let mut failures = Vec::new();
        for spec in routes() {
            let extras = extras_for(&spec, &tools);
            let registry = registry_for(&spec, &extras);
            let target = synthetic_target(spec.file_stem.as_bytes());
            let full = product_surface_with_fence(
                plakous::tools::JobConfig::default(),
                spec.provider.presentation(),
                registry.clone(),
                target.clone(),
                &extras,
                super::super::SessionToolFence::Full,
                route_runtime(&spec),
            )
            .expect("full surface");
            for fence in [
                super::super::SessionToolFence::OccupancyReadOnly,
                super::super::SessionToolFence::OccupancyExecute,
            ] {
                let surface = product_surface_with_fence(
                    plakous::tools::JobConfig::default(),
                    spec.provider.presentation(),
                    registry.clone(),
                    target.clone(),
                    &extras,
                    fence,
                    route_runtime(&spec),
                )
                .expect("filtered surface");
                let (profile, _) = route_profile(&spec, &extras, &surface, &home, &root, &cwd);
                let instructions = profile.base_instructions();
                for name in full
                    .exposed_names()
                    .iter()
                    .filter(|name| !surface.exposed_names().contains(name))
                {
                    let name = name.as_str();
                    let named = if name.contains('_') {
                        instructions
                            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                            .any(|word| word == name)
                    } else {
                        // Bare English verbs such as "Write for an engineer" are not tool references.
                        ["use ", "Use ", "Prefer ", "prefer ", "and ", "`"]
                            .iter()
                            .any(|prefix| {
                                instructions.contains(&format!("{prefix}{name} "))
                                    || instructions.contains(&format!("`{name}`"))
                            })
                    };
                    if named {
                        failures.push(format!(
                            "{} {fence:?}: instructions name absent {name}",
                            spec.file_stem
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// The harness footprint the fullest surface may spend: a root or a nested
    /// parent, whose instructions carry the earned orchestration paragraphs
    /// and whose tools carry the orchestration schemas.
    pub(crate) const ORCHESTRATION_TOKEN_BUDGET: usize = 8_000;

    #[test]
    fn rendered_codex_base_selection_tracks_the_model_family() {
        let (_tmp, home, root, cwd) = instruction_fixture();
        let tools = occupancy_tools(&root);
        for mut spec in routes().into_iter().filter(|spec| {
            spec.provider == ProviderKind::Codex
                && matches!(
                    spec.composition,
                    ProductCompositionSelection::Direct | ProductCompositionSelection::NativePtc
                )
        }) {
            for model in ["gpt-6.1-sol", "gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"] {
                spec.model = model;
                let artifacts = wire::render_wire(&spec, &home, &root, &cwd, &tools);
                let text = &artifacts.footprint.instruction_text;
                assert!(
                    text.contains(if model.starts_with("gpt-6") {
                        "You are Codex, an agent based on GPT-6."
                    } else {
                        "You are Codex, a coding agent based on GPT-5."
                    }),
                    "{}: {model}",
                    spec.file_stem
                );
                assert_eq!(
                    text.contains("## Intermediate commentary"),
                    model.starts_with("gpt-6")
                );
                assert!(!text.contains("JSON.parse(await tools.exec_command"));
                assert!(!text.contains("Do not narrate tool calls"));
                let total =
                    wire::HarnessFootprint::approx_tokens(artifacts.footprint.total_bytes());
                let budget = if spec.occupancy == Occupancy::SingleAgent {
                    if model.starts_with("gpt-6") {
                        5_500
                    } else {
                        SINGLE_NODE_TOKEN_BUDGET
                    }
                } else {
                    ORCHESTRATION_TOKEN_BUDGET
                };
                assert!(
                    total < budget,
                    "{}: {model} uses {total} tokens",
                    spec.file_stem
                );
                if spec.occupancy == Occupancy::SingleAgent
                    && spec.composition == ProductCompositionSelection::NativePtc
                {
                    assert!(
                        total <= budget - 150,
                        "{}: {model} lacks headroom",
                        spec.file_stem
                    );
                }
            }
        }
    }

    /// Every route's harness footprint, against its normative family budget.
    ///
    /// This is the gate the prompt-parity work needed: the Codex base grew
    /// from ~400 characters to the kept sections of Codex's own base
    /// instructions, and nothing else stops that growth from continuing until
    /// the harness crowds out the task. The table is printed on every run, so
    /// a reviewer reads the cost of a wording change without re-deriving it.
    #[test]
    fn rendered_prompt_footprints_stay_within_budget() {
        let (_tmp, home, root, cwd) = instruction_fixture();
        let tools = occupancy_tools(&root);
        let mut rows = vec![
            "| route | occupancy | instructions | tools | total | budget |".to_owned(),
            "| --- | --- | --- | --- | --- | --- |".to_owned(),
        ];
        let mut failures = Vec::new();
        for spec in routes() {
            let artifacts = wire::render_wire(&spec, &home, &root, &cwd, &tools);
            let footprint = &artifacts.footprint;
            let instructions = wire::HarnessFootprint::approx_tokens(footprint.instruction_bytes);
            let tool_tokens = wire::HarnessFootprint::approx_tokens(footprint.tool_bytes);
            let total = wire::HarnessFootprint::approx_tokens(footprint.total_bytes());
            let gpt6 = spec.model.starts_with("gpt-6")
                && matches!(
                    spec.composition,
                    ProductCompositionSelection::Direct | ProductCompositionSelection::NativePtc
                )
                && spec.provider.presentation() == CodingPresentation::Codex;
            let single_budget = if gpt6 {
                5_500
            } else {
                SINGLE_NODE_TOKEN_BUDGET
            };
            let full_budget = ORCHESTRATION_TOKEN_BUDGET;
            let (occupancy, budget) = match spec.occupancy {
                Occupancy::SingleAgent => ("single-agent", single_budget),
                Occupancy::Root => ("root", full_budget),
                Occupancy::NestedParent => ("nested-parent", full_budget),
                Occupancy::Child => ("child", full_budget),
            };
            rows.push(format!(
                "| `{}` | {occupancy} | {instructions} | {tool_tokens} | {total} | {budget} |",
                spec.file_stem
            ));
            if spec.file_stem == "codex.native-ptc.root.single-agent" && total > single_budget - 150
            {
                failures.push(format!(
                    "{}: {total} tokens leaves insufficient headroom",
                    spec.file_stem
                ));
            }
            // A basic single node must stay *under* the budget; an
            // orchestration surface may reach it.
            let over = match spec.occupancy {
                Occupancy::SingleAgent => total >= budget,
                Occupancy::Root | Occupancy::NestedParent | Occupancy::Child => total > budget,
            };
            if over {
                failures.push(format!(
                    "{}: {total} approximate tokens ({instructions} instructions + {tool_tokens} tools) against a {budget}-token budget",
                    spec.file_stem
                ));
            }
        }
        println!(
            "Harness footprint, approximate tokens ({}):\n{}",
            wire::HarnessFootprint::token_count_method(),
            rows.join("\n")
        );
        assert!(
            failures.is_empty(),
            "harness footprint over budget (spec 05):\n\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn rendered_content_filter_guidance_matches_its_pin() {
        let render = plakous::prompts::content_filter::render(None);
        let pin = &plakous::prompts::content_filter::CONTENT_FILTER_PROMPT_PINS[0];
        assert_eq!(render, pin.text());
        assert_eq!(
            plakous::tools::drifted_pins(std::slice::from_ref(pin)),
            [] as [(&str, String); 0]
        );
        if print_pins_requested() {
            println!(
                "CONTENT_FILTER_PROMPT_PINS: {:?}",
                plakous::tools::PromptDigest::of(&render)
            );
        }
    }

    #[test]
    fn rendered_product_prompts_match_review_goldens() {
        assert_eq!(
            routes().len(),
            86,
            "included route matrix is providers×compositions×topology minus documented exclusions"
        );
        assert_eq!(excluded_routes().len(), 2);
        assert_eq!(
            PROMPT_RENDER_PINS.len(),
            86,
            "the prompt render pin table covers every included route"
        );
        let (_tmp, home, root, cwd) = instruction_fixture();
        let tools = occupancy_tools(&root);
        let pins = PROMPT_RENDER_PINS
            .iter()
            .copied()
            .collect::<BTreeMap<_, _>>();
        let update = std::env::var("PLAK_UPDATE_GOLDENS").is_ok_and(|value| value == "1");
        let golden_dir = golden_dir();
        let wire_dir = wire_dir();
        if update {
            std::fs::create_dir_all(&golden_dir).expect("create prompt artifact dir");
            std::fs::create_dir_all(&wire_dir).expect("create prompt wire artifact dir");
        }

        let mut failures = Vec::new();
        let mut rows = Vec::new();
        for spec in routes() {
            let markdown = render_route(&spec, &home, &root, &cwd, &tools);
            let artifacts = wire::render_wire(&spec, &home, &root, &cwd, &tools);
            let wire_value: serde_json::Value =
                serde_json::from_str(&artifacts.request_pretty).expect("wire request json");
            let kernel_value = {
                let extras = extras_for(&spec, &tools);
                let (profile, facts) = assemble_profile(&spec, &extras, &home, &root, &cwd);
                wire::lowered_request_value(&spec, &extras, &profile, &facts)
            };
            if wire_value != kernel_value {
                failures.push(format!(
                    "{}: rendered wire JSON does not match kernel-lowered request",
                    spec.file_stem
                ));
            }
            let fence = extract_request_fence(&markdown).unwrap_or_else(|| {
                panic!(
                    "{}: Markdown missing ## Request jsonc fence",
                    spec.file_stem
                )
            });
            if fence != artifacts.request_jsonc.trim_end() {
                failures.push(format!(
                    "{}: Markdown Request fence does not match jsonc formatter output",
                    spec.file_stem
                ));
            }
            if !markdown.starts_with("Harness environment: **") {
                failures.push(format!(
                    "{}: Markdown missing harness footprint header",
                    spec.file_stem
                ));
            }
            if !markdown.contains(&format!("\n# `{}`\n\n", spec.file_stem)) {
                failures.push(format!(
                    "{}: Markdown missing scenario header `# `{}`",
                    spec.file_stem, spec.file_stem
                ));
            }
            if artifacts
                .footprint
                .instruction_text
                .contains(WORKSPACE_AGENTS)
            {
                failures.push(format!(
                    "{}: harness footprint still includes enrolled AGENTS.md",
                    spec.file_stem
                ));
            }
            let assignment = first_turn_user_text(&spec);
            if artifacts.footprint.assignment_bytes != assignment.len() {
                failures.push(format!(
                    "{}: harness assignment exclusion is {} bytes, expected {}",
                    spec.file_stem,
                    artifacts.footprint.assignment_bytes,
                    assignment.len()
                ));
            }
            if matches!(spec.occupancy, Occupancy::Child | Occupancy::NestedParent)
                && artifacts
                    .footprint
                    .instruction_text
                    .contains(assignment.as_str())
            {
                failures.push(format!(
                    "{}: harness footprint still includes the assignment Start prompt",
                    spec.file_stem
                ));
            }
            if artifacts.footprint.enrolled_source_bytes == 0 {
                failures.push(format!(
                    "{}: harness footprint did not subtract enrolled AGENTS.md",
                    spec.file_stem
                ));
            }
            if matches!(spec.provider, ProviderKind::Claude)
                && !artifacts
                    .footprint
                    .instruction_text
                    .contains("You are Claude Code")
            {
                failures.push(format!(
                    "{}: harness footprint dropped Claude costume identity",
                    spec.file_stem
                ));
            }
            failures.extend(extension_visibility_failures(
                &spec,
                &extras_for(&spec, &tools),
                &artifacts.footprint.instruction_text,
                &kernel_value,
            ));
            let digest = render_digest(&markdown);
            match pins.get(spec.file_stem.as_str()) {
                Some(pinned) if *pinned == digest => {}
                Some(pinned) => failures.push(format!(
                    "{}: rendered prompt changed (pinned {pinned}, rendered {digest})",
                    spec.file_stem
                )),
                None => failures.push(format!("{}: route has no pinned digest", spec.file_stem)),
            }
            rows.push(format!("        (\"{}\", \"{digest}\"),", spec.file_stem));
            if update {
                let path = golden_dir.join(format!("{}.md", spec.file_stem));
                std::fs::write(&path, &markdown).expect("write rendered prompt artifact");
            }
        }
        if print_pins_requested() {
            println!("PROMPT_RENDER_PINS:\n{}", rows.join("\n"));
        }
        assert!(
            failures.is_empty(),
            "rendered prompt drift (inspect with `just prompts-diff`; re-pin with PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_):\n\n{}",
            failures.join("\n\n")
        );
    }
}
