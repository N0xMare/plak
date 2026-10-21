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
            "e9ee3583cf3a10fe1ab6f653705035dbf26fe50ea72ee9b07950d445c41e292e",
        ),
        (
            "codex.direct.root",
            "1a1340555a837c4427c72dbf90017f5c722c4062d26bb1d27c6adf8865a242cd",
        ),
        (
            "codex.direct.nested-parent",
            "f3df8a75a923f1ddf5f3a4046aee31bf874c6f5626be202383453d17c93ea9ee",
        ),
        (
            "codex.direct.child",
            "e7a32a761473128383c00bb5d31ad8dcb66174118b1a0d294160eb0175b4f911",
        ),
        (
            "codex.native-ptc.root.single-agent",
            "928ed644019ca84f2bb380562886ef4e8f48f5f075361c2d7c97f4ce5df3da4a",
        ),
        (
            "codex.native-ptc.root",
            "5dcfe1259ed29878f10c31968b9db229913848a55c3e53606e124095c8135fc5",
        ),
        (
            "codex.native-ptc.nested-parent",
            "3a7bdb74f1653865b649c875a5381b83427709572944b57d13c89e93d81c1c53",
        ),
        (
            "codex.native-ptc.child",
            "4115c0bdf1ba92949c3f5bca72267fd23993c5ae67b778e85e1b775e31de11e6",
        ),
        (
            "codex.js-ptc.root.single-agent",
            "ccff5bfa7bbbf13846316db81c7403277e7e4f98757868c79e04c2d49c1b4001",
        ),
        (
            "codex.js-ptc.root",
            "d59db1b5de3e1125ef9d1f4add84ae50b64f7d82cc4a63604a6ac3f2abd07a05",
        ),
        (
            "codex.js-ptc.nested-parent",
            "01c5f11419a9776a5ee7f2b0e851474cf5c567923050179b594d96550c375953",
        ),
        (
            "codex.js-ptc.child",
            "006121956af2a2b2f66545f129c30f8ae66a3cb81fe78e00348a961264ab22a8",
        ),
        (
            "codex.py-ptc.root.single-agent",
            "2dcdc5c5c0c0d18c744bd012d6e801ce92fdd3d5d40f91378292e4258e518d74",
        ),
        (
            "codex.py-ptc.root",
            "85c577df5343effcbd236267ee744a003f9c7ecf0776eb817bad08acf2258801",
        ),
        (
            "codex.py-ptc.nested-parent",
            "8c37a533ab2402d01a6fe6f0b526424b2b6ad861c6456bae63c03d1aeaf0aecc",
        ),
        (
            "codex.py-ptc.child",
            "aac0a0680caedd09cb580a97164808ef45e5611c844f5dac0626f411e9553aa2",
        ),
        (
            "grok.direct.root.single-agent",
            "3c7c7252f9a14f9fb9fc5fb52d358f3da9dfbc1b8c699bd0c0774798b21b3626",
        ),
        (
            "grok.direct.root",
            "06101ca172ed980d99b1c013d1e5bbc2567a19268741dbbc84c05fd0df6a68d6",
        ),
        (
            "grok.direct.nested-parent",
            "838c51b98cb5d7bf7522309bbae8f35ed32c0e94af2b2de067e568cded968936",
        ),
        (
            "grok.direct.child",
            "e7f9de5454cb4cdefd3e633e1b97e631187ecb12d3c5095b7ad75d7c9375b572",
        ),
        (
            "grok.js-ptc.root.single-agent",
            "6e96b45db99a1d40b108200ce55369758603162d9e61df08b374d4c26c058d2c",
        ),
        (
            "grok.js-ptc.root",
            "18da4e0d15c19ff997802a67eca10270913e5e1689fedec8fc96564ffe2fc543",
        ),
        (
            "grok.js-ptc.nested-parent",
            "20c307326c787ed934fb3968094abe2e91f97066e9fe21cbebb9b2bf256bacd4",
        ),
        (
            "grok.js-ptc.child",
            "4462f6f110d0380b6aa900ebbd2ccb76a0d74692fb9dce1d6b2b4ba2bdfd56e7",
        ),
        (
            "grok.py-ptc.root.single-agent",
            "51647380c3d2199174775dc3ccd128a6c25bf08f6875fc92e239e31917a7a108",
        ),
        (
            "grok.py-ptc.root",
            "d0302cddb44653c168f1da8b2b43e47654be984910b85caf112d03c650e05de7",
        ),
        (
            "grok.py-ptc.nested-parent",
            "3c6875ce6dd05ca11ff64b239157578812aa97ce495004fa755e47e69e214105",
        ),
        (
            "grok.py-ptc.child",
            "698084f6c5ac159c3fbf46970126fabc29e52081d2842c9eb62d8efa05cd9fac",
        ),
        (
            "claude.direct.root.single-agent",
            "9d19cadc7921e92d95bad88b7935cc6442dd6779364607de0773f51eefc230b0",
        ),
        (
            "claude.direct.root",
            "2abd9d1f6300fcf5e6df09457cc3e6b6242f5dd00091706b2a3832e231d94ebb",
        ),
        (
            "claude.direct.nested-parent",
            "915cb58b98c6ecd3fd5cfb6509833550a4f1674e553c957e7d53952c8ad9f5ea",
        ),
        (
            "claude.direct.child",
            "b1dd7659203ce7351c06eee073641d8468d9bf4052b2ce738647156d7e62ef5b",
        ),
        (
            "claude.js-ptc.root.single-agent",
            "45a796934a6cfa453d5c2a84a875dd4875855a4f1e81f9ac68e34195aa48fb43",
        ),
        (
            "claude.js-ptc.root",
            "91ba6aba961472fef89c0ea6824f92ff0772e87aeb95a57c7dac5ac15d2ea333",
        ),
        (
            "claude.js-ptc.nested-parent",
            "615619b2442f4ba5ac63449ec3479e20877dfc7884b5882a0e4203938e38167f",
        ),
        (
            "claude.js-ptc.child",
            "d34ca5d51d5112eaace6273a49e0c8fdfc38251bcc4e6e17fdaff6ecfa1eaf45",
        ),
        (
            "claude.py-ptc.root.single-agent",
            "a756b4f5fa2f3a14e66688ded33aa7ec90986f265e1412550d7d52f438a28330",
        ),
        (
            "claude.py-ptc.root",
            "1769327b4c99f80f5f62d5f32d82b3aa515f69c13ac07d5275fb30a1b2720597",
        ),
        (
            "claude.py-ptc.nested-parent",
            "f3ea2bc810f11f0d3fe928bbfa9644c5c40ff236c0d6a7062f42ce886030b90d",
        ),
        (
            "claude.py-ptc.child",
            "419fa078c4e640e2081c1e11ad044d6a6dfe8af8f30a3fc6ace8f28275c0e0f7",
        ),
        (
            "openrouter-chat.direct.root.single-agent",
            "e71c565c053337ea4f56c6700db5a49f5b4cd006ea3737815d28fee24a33fab1",
        ),
        (
            "openrouter-chat.direct.root",
            "387e93fc8391f54689a357d9bf510ee2988a63701843c7ba9091bde2e470c102",
        ),
        (
            "openrouter-chat.direct.nested-parent",
            "369d3ae973f1eec645f54de89af143620bc8d0d0d70383e94ccd68780b63623f",
        ),
        (
            "openrouter-chat.direct.child",
            "25b04b5a0a9f18200bbc28aade2b71082a96f70a0f2d177673fc3d4ebc2b4d3b",
        ),
        (
            "openrouter-chat.js-ptc.root.single-agent",
            "67771c79e13e338b4afd0b01d0e2776b14c7eb884c9f77cad3434ab6679034d2",
        ),
        (
            "openrouter-chat.js-ptc.root",
            "ad68adaeda86dde3b2a9da67813fda3d254416cc8c50e7b68f1a28b8dd794a89",
        ),
        (
            "openrouter-chat.js-ptc.nested-parent",
            "49d6298b11f94675046430452250f1b39406046bcce0c1d534123057c9d8c28d",
        ),
        (
            "openrouter-chat.js-ptc.child",
            "62f07005650434061d2f4bbcfce80b9ab2232383e888ed21606665fc54327129",
        ),
        (
            "openrouter-chat.py-ptc.root.single-agent",
            "1f780d7167634120853a6c6bcf1ef99978214e97fa9a759eb50dc3f2a111ed1a",
        ),
        (
            "openrouter-chat.py-ptc.root",
            "0d687dde0a0bbcd25ea86291f67ddd8d761eb7688fdbfd0077cda626ee697b1c",
        ),
        (
            "openrouter-chat.py-ptc.nested-parent",
            "1397ca4cf97b90d0448e72a7c2ad46d32cd74bf8ec14b5f481e73cbe0d3a7bc3",
        ),
        (
            "openrouter-chat.py-ptc.child",
            "f8fc6ccf95743d09f95b140d4657a6eb9b18c1ef1bb96665eb678c71989eab97",
        ),
        (
            "openrouter-responses.direct.root.single-agent",
            "9fe475e9021185b33c7436fd5d5c4a8835ad39563b06f9f662813feca5409d14",
        ),
        (
            "openrouter-responses.direct.root",
            "7959d917d52050f412e034f957f6154f73a1c67666b18b80dfd79ac35ca19155",
        ),
        (
            "openrouter-responses.direct.nested-parent",
            "5264a493761562616b3ade9007c74b644ffc1179885a972a3c0f3d2122cbacd5",
        ),
        (
            "openrouter-responses.direct.child",
            "788411e81f0c69bdbe6f5c521720703671523e43b1657af0382ac2ca903d6e89",
        ),
        (
            "openrouter-responses.js-ptc.root.single-agent",
            "afbf0f7150744354e336edeef2b7de5253820b53fe2bdc56adf893ea5a72255a",
        ),
        (
            "openrouter-responses.js-ptc.root",
            "ff4973ae1df2d5db6194f30d8ed3ed17e100183d373918d32df7e4f09f9a7489",
        ),
        (
            "openrouter-responses.js-ptc.nested-parent",
            "742b14767d3544c0c6fb57afb8e5e7e76d3b129c89781daaf942b08b98ca42e0",
        ),
        (
            "openrouter-responses.js-ptc.child",
            "6e1dc563d6cee7c626d3528cec49b1ec519409750a27ec04946eaee0941142ff",
        ),
        (
            "openrouter-responses.py-ptc.root.single-agent",
            "eda7a997ad229151bba2577927f354afc75ada47613069df3356aa19d7f773f2",
        ),
        (
            "openrouter-responses.py-ptc.root",
            "a84e28cf28eedcc6999a3dcad239d74938b14ea580fd030fe279e32f44033e0e",
        ),
        (
            "openrouter-responses.py-ptc.nested-parent",
            "a00c6247540673aefc08e33c9ab5175d24f772e8739d55661b8f137ca6d30d81",
        ),
        (
            "openrouter-responses.py-ptc.child",
            "d44f25287113cc371498fbba6480d2fbdff273b6e1da75754bac3ec61c239aa7",
        ),
        (
            "openrouter-messages.direct.root.single-agent",
            "a3ab1009f89d74339a5a2482d79ec65b1538cf894ca1a28e988dc5b4eb282b1f",
        ),
        (
            "openrouter-messages.direct.root",
            "a8b73f5d22c7219d7be5c1238d82e1ba2c0aa06cd66be6a4f2da0159a101aeb9",
        ),
        (
            "openrouter-messages.direct.nested-parent",
            "8b13fd8d5ea23ca75fd26329f055257aba2b3b1d6b4503c9f652c73919aa1ebc",
        ),
        (
            "openrouter-messages.direct.child",
            "ec2b921fd7629b7b1964965f832a40f477c7c953affc1f4f3392008e106e59f0",
        ),
        (
            "openrouter-messages.js-ptc.root.single-agent",
            "f024567b1fba94064d1980eda0ff85f00fddf9f4611cbb1d6d056d13c3bd3c13",
        ),
        (
            "openrouter-messages.js-ptc.root",
            "aba7c11f8eee26912d1aa45ab27cdf3ee7a567c4dd570f3e3ba222fc1d2c0677",
        ),
        (
            "openrouter-messages.js-ptc.nested-parent",
            "f3e3613be338ca3d605ed72f86c3b076b4d527107501201347885e9058391dd9",
        ),
        (
            "openrouter-messages.js-ptc.child",
            "855e837394cd87d236085ca80f10e4ad5ba047a1ccf1b6d73f735aeb61b2a4bb",
        ),
        (
            "openrouter-messages.py-ptc.root.single-agent",
            "7886a8ae796a798bee3db2cdc9a4d692a1368546ec13d6b74c2ba75ec1f03c16",
        ),
        (
            "openrouter-messages.py-ptc.root",
            "2c327022a944fa244dc9c931369f1e055cfe0cc2553bc94f7782159889421727",
        ),
        (
            "openrouter-messages.py-ptc.nested-parent",
            "0be41dcf74557bbd096b827c377316a29e2f35ae888d72913f12a6a45098d977",
        ),
        (
            "openrouter-messages.py-ptc.child",
            "34c476d4b8a5e29949254093856077a7d9a1336caad5d71fca807939272a97f5",
        ),
        (
            "openai-api.direct.root.single-agent",
            "47651f8c8380b003a64c45f540386a7f681890b9ead9965ea237b7ef215e350e",
        ),
        (
            "openai-api.native-ptc.root.single-agent",
            "99180b96f53c29ae484b90321e378d1a30ef9de26e2b4d18bc4a262dd5d01f4f",
        ),
        (
            "openai-api.js-ptc.root.single-agent",
            "686c3a5bb0383fa3876c8c1f1a7fad6d2471fb527abe0abd9c78ca235ec7de98",
        ),
        (
            "openai-api.py-ptc.root.single-agent",
            "7d21c45161f8969e552cb5bb14d93ad4c304ba09d88b86c8cc4a36d2d4dbf800",
        ),
        (
            "xai-api.direct.root.single-agent",
            "b03e48b766fcc930dcc3afd029a311ae7f6862aae598b3847438eb4c7a3ba089",
        ),
        (
            "xai-api.js-ptc.root.single-agent",
            "d1129b61773acd1a93ee890033789dbe5008eab352cc73ecb73998700d638263",
        ),
        (
            "xai-api.py-ptc.root.single-agent",
            "6ad75b3fda616b6fc56a41883a09f27afeb863941c211d074df4e29915cbbfaa",
        ),
        (
            "anthropic-api.direct.root.single-agent",
            "5679f8d002621cfc3b12f857ee934fd6b4b3a9b2a3d3067ced10eae2486dee55",
        ),
        (
            "anthropic-api.js-ptc.root.single-agent",
            "2e63b050143026d29fd10d8bf7ddd1dbd52bb9493777ceb4c36ee0d3dbb0e628",
        ),
        (
            "anthropic-api.py-ptc.root.single-agent",
            "144f263ff7f5b4a5fea254e178cbc8dd55cde8894c71a40f4c1fc04870097208",
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
