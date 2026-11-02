//! Assembled model-visible prompt goldens for product routes.
//!
//! Included from `agent.rs` under `#[cfg(test)]`. The inner `#[cfg(test)]`
//! module keeps `tools/gate/check_workspace.py` from treating golden helpers as
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
            ))
            .expect("child composition has a result channel"),
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
            ))
            .expect("child composition has a result channel"),
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
            ProductCompositionSelection::PySdk => "Python SDK",
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
            (_, ProductCompositionSelection::PySdk) => " (run_py)",
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
            ProductCompositionSelection::PySdk,
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
                    if composition == ProductCompositionSelection::PySdk
                        && occupancy != Occupancy::SingleAgent
                    {
                        continue;
                    }
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
        use ProductCompositionSelection::{Direct, JsPtc, NativePtc, PyPtc, PySdk};
        [
            (PlatformProvider::OpenAi, "gpt-5.6-luna", Direct),
            (PlatformProvider::OpenAi, "gpt-5.6-sol", NativePtc),
            (PlatformProvider::OpenAi, "gpt-5.6-luna", JsPtc),
            (PlatformProvider::OpenAi, "gpt-5.6-luna", PyPtc),
            (PlatformProvider::OpenAi, "gpt-5.6-luna", PySdk),
            (PlatformProvider::Xai, "grok-4.6", Direct),
            (PlatformProvider::Xai, "grok-4.6", JsPtc),
            (PlatformProvider::Xai, "grok-4.6", PyPtc),
            (PlatformProvider::Xai, "grok-4.6", PySdk),
            (PlatformProvider::Anthropic, "claude-sonnet-5", Direct),
            (PlatformProvider::Anthropic, "claude-sonnet-5", JsPtc),
            (PlatformProvider::Anthropic, "claude-sonnet-5", PyPtc),
            (PlatformProvider::Anthropic, "claude-sonnet-5", PySdk),
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

    #[allow(
        clippy::too_many_arguments,
        reason = "Native prompt goldens compile the selected provider profile with the exact nested tools, product home, workspace, and working directory."
    )]
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
            (_, ProductCompositionSelection::PySdk) => "golden-python-sdk",
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
        let profile = crate::agent::build::api::api_product_profile(
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
                        crate::agent::build::api::code_mode_nested_names(&surface)
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
            ProductCompositionSelection::PySdk => plakous::tools::PythonHarnessSdkV1::compile(
                plakous::tools::JobConfig::default(),
                registry,
                target,
            )
            .map(|descriptor| descriptor.outer_projection().clone())
            .map_err(|error| error.to_string()),
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
            "codex.py-sdk.root.single-agent",
            "66b370afa7fb7afc88656f2d598d4664f42959388088beebfcd649c7f5078a62",
        ),
        (
            "grok.py-sdk.root.single-agent",
            "4ab09d4118980697dd332dbb1c3fd7c08d72b82e48f563150bc30eaa5db66b23",
        ),
        (
            "claude.py-sdk.root.single-agent",
            "a6e17f01f00d28e429d738fc5f040e4ab1a64b2169fb035dcb2b8dba266f9366",
        ),
        (
            "openrouter-chat.py-sdk.root.single-agent",
            "dc5cd16bf2af670b19275bc205c8d45fe6438a702d81ad95accd273990ee6d77",
        ),
        (
            "openrouter-responses.py-sdk.root.single-agent",
            "27c6b3d227409bf10fab7f40496f608e9dc870f304f472c2afee09857a4603c1",
        ),
        (
            "openrouter-messages.py-sdk.root.single-agent",
            "414a03915f052193bb82eb5cec6633aaeea12b8408ab66ec588d5eaa90e91451",
        ),
        (
            "openai-api.py-sdk.root.single-agent",
            "f2a64c71746817e2d1fdc3717215c591bc14aac6bf7fefbb0505140db0d610b9",
        ),
        (
            "xai-api.py-sdk.root.single-agent",
            "121a79fe36ec55e4e103454d6f8aa6865ad05f3856689db1dfc9871451ec50dc",
        ),
        (
            "anthropic-api.py-sdk.root.single-agent",
            "333e5d46d10a84c27e57d39c955cae8112213e448e293c0470222f884b0c8f69",
        ),
        (
            "codex.direct.root.single-agent",
            "172a30f3c14dc4b1f1bda28e098d1e929bef4b180b446bc690364e1dc39335b6",
        ),
        (
            "codex.direct.root",
            "d78c8ef7d9d79b48e7e9431076188c237cdcaa20c5a30f0d2a2b1c042b3285ad",
        ),
        (
            "codex.direct.nested-parent",
            "700f023719d77d2454d8cdd0d701db8efd0cf3e2ce8ea074db1c67aed3a25d6b",
        ),
        (
            "codex.direct.child",
            "b80a3bc7889fdbd9c7522f80d2620b3b0187a32213780036f4cd4238fb40fcc5",
        ),
        (
            "codex.native-ptc.root.single-agent",
            "b128a7c58a506b0e44cdf60cc1d6edc2ca440aca296460a52423de0ba62f6a38",
        ),
        (
            "codex.native-ptc.root",
            "1a3bf7cbea01f12d2d962659ac9d17e8d12a390252101114d1d130323ab18412",
        ),
        (
            "codex.native-ptc.nested-parent",
            "d69b3e100a84a4d18a7f4a1dd3c77b4be8aef5aeaa7da04a2850c9685a0369fd",
        ),
        (
            "codex.native-ptc.child",
            "712b8d6207ead9b8a4dc46829945523ba80a28ca8e7c9eb1c9d2cbe3f654921b",
        ),
        (
            "codex.js-ptc.root.single-agent",
            "a25498032ab4469900d6936d17940d46a83dfbfb4a7b0d24c5e10eabc05e5ffc",
        ),
        (
            "codex.js-ptc.root",
            "7977d12bd61afcc02fa66b50c28d93ac6d4d816050872937b7a11d49fba85758",
        ),
        (
            "codex.js-ptc.nested-parent",
            "befb570532a2bd28eada93c9f5cbe6cdd94ad9d791e606f7b979e5b74dae8188",
        ),
        (
            "codex.js-ptc.child",
            "4814f27a8ed2364fd33e32b738d88352134d06fb8e22aadf6e1733c0d4804324",
        ),
        (
            "codex.py-ptc.root.single-agent",
            "de9fa3f17a52efc3991edb724f190b00988f67a89c1d42222cf9e2d5e0478658",
        ),
        (
            "codex.py-ptc.root",
            "68962862dab349e764f51694d92b9da259bd0f57735163de5fbb3a6e62bb236f",
        ),
        (
            "codex.py-ptc.nested-parent",
            "3fc9d0a8db26dcc0922f20a11f7a9743c11989a3c13fdefd747f37416d2795a3",
        ),
        (
            "codex.py-ptc.child",
            "b1b6076fd0df2c434a9047b631c05485cf42104343a08c431eeba4897b1bbf0a",
        ),
        (
            "grok.direct.root.single-agent",
            "d96834138b0f06328c5aa2067306096efaf3801609d2b393f054f780023a9037",
        ),
        (
            "grok.direct.root",
            "5bfc2e235f67d73124d4673e9e465b990007456aab53680edc677350a58d345b",
        ),
        (
            "grok.direct.nested-parent",
            "9d2955090fccb36d7e7133c9df07881a83cbb978c4b0f1ff5a750e3b22199eca",
        ),
        (
            "grok.direct.child",
            "77d4523d6b4422457c2d843e344543026b8f3ad8dcf06e30b458c7769f04e50b",
        ),
        (
            "grok.js-ptc.root.single-agent",
            "2b2e37571213405d3364e673cc89956275a5da12d7933d73d8c34361d3ae02da",
        ),
        (
            "grok.js-ptc.root",
            "ed882cd25d21896f4dd2332c7c00b05a238c51bcf6fb9eba67ad25879bb8855e",
        ),
        (
            "grok.js-ptc.nested-parent",
            "f469964f43337c53a5ae3e3596d0ff3111f8e11bf2cb86f7b9209024b937cda9",
        ),
        (
            "grok.js-ptc.child",
            "43bdf82ac3bdda686c5d97549f6ea8efdbe5a213546ac6d759a24094016308e2",
        ),
        (
            "grok.py-ptc.root.single-agent",
            "905f7934eca3fbc0e4318cd6271dd23c601137defc971badd82b55022c7f58f5",
        ),
        (
            "grok.py-ptc.root",
            "c73edf5f8c5e5124c14afafbaaf31cfb062852511bd74af60f01d4b7c4805876",
        ),
        (
            "grok.py-ptc.nested-parent",
            "dbfb49c0a65ce35d3f87a1d66d209ff2ea9d86fce26cf7d0167294fa9e961d33",
        ),
        (
            "grok.py-ptc.child",
            "28d483624eaba5f4e1d3e8a939368841e4aef0afc5814af50c5a1e66aecdd3fa",
        ),
        (
            "claude.direct.root.single-agent",
            "0ca45804a7ca3f41e72c798199814c208cba7a0a5fae7e57fa8201d86b317c16",
        ),
        (
            "claude.direct.root",
            "b58c0f632c996c2b91db976eedb1ca8791808c541c2e51e2170a5c7e10b5aa7f",
        ),
        (
            "claude.direct.nested-parent",
            "1ddcaee5236c849263e349935c17f6a7cc90718dd664cb2d9a6c69254b6e8efa",
        ),
        (
            "claude.direct.child",
            "6e5f7f74ba31bd701c3b8271ec327f64014cb9599c36248af028cb171ec1e840",
        ),
        (
            "claude.js-ptc.root.single-agent",
            "a065c2c7b1ba1c00f03d89afa3101a3a2e4af228bc63eda22bacf985c5b33521",
        ),
        (
            "claude.js-ptc.root",
            "bace11487d4740dcc70179a2be011fd4d0c9afdefcdd8c0dbc98eb393ded45f5",
        ),
        (
            "claude.js-ptc.nested-parent",
            "b04595ca12babebf17d6bf236b537c47b063f27bb330bce49613db7393f63f8e",
        ),
        (
            "claude.js-ptc.child",
            "7443f10d1e2beed8e5704b3311ccf341147996b0a563221a70f34c0e8341fa38",
        ),
        (
            "claude.py-ptc.root.single-agent",
            "fec3525484998b82e13954edba732c56d65b1dba49af3a1fbf7a1e55281b8be4",
        ),
        (
            "claude.py-ptc.root",
            "adc4d25e87ed4568b69c3df530b1d56923f2ec0cf1cf2864e2fd1a106bd7e26e",
        ),
        (
            "claude.py-ptc.nested-parent",
            "e67e698188d2f12a65f918096ea0c48dc41c14b16bff7a626d0da1193034923c",
        ),
        (
            "claude.py-ptc.child",
            "d9efab0b79e5114c77d4367bfbd16d0d24a7216b6d83f12857e9522e7f6c6110",
        ),
        (
            "openrouter-chat.direct.root.single-agent",
            "d38c29c24d45f4125be784e99e5655eeebdb5c20f981e334914d681cab5f7fe7",
        ),
        (
            "openrouter-chat.direct.root",
            "d3a553131eb9b333882cf34b8b26290e08f3f0537c786d4d94dccc6a8d78ecf0",
        ),
        (
            "openrouter-chat.direct.nested-parent",
            "c2d94f92c7726829907d5c18febfca274fcf49aa595b57f2b873b1908845a365",
        ),
        (
            "openrouter-chat.direct.child",
            "25d40e8aa76458668f86ac398840d83cd4f8fcf28e89c471e99fb82606cd3356",
        ),
        (
            "openrouter-chat.js-ptc.root.single-agent",
            "dca873eecd9cde82a43096078e1596aabc38a67964392d92860ec45b8163500a",
        ),
        (
            "openrouter-chat.js-ptc.root",
            "833aad009409c170dc3f9ba74b36c0ee12e1a6c532a735c1503ba2720ae7f85b",
        ),
        (
            "openrouter-chat.js-ptc.nested-parent",
            "81220fa36273e9cc6046b763e75ece29998f76b5b0d0d4bb93b4fa91a7bcfca0",
        ),
        (
            "openrouter-chat.js-ptc.child",
            "56f262c85a0baba8392a3f1f204a0869fe2463c6120b2bee969d7a8151adc27b",
        ),
        (
            "openrouter-chat.py-ptc.root.single-agent",
            "2dba6306f01b67f4b9a372775387c7aa0c5093736b086851880f7263aef6876a",
        ),
        (
            "openrouter-chat.py-ptc.root",
            "4977783612de47327867d13bd93e99e19acb9b536ec81a3cc935c3a800b071e7",
        ),
        (
            "openrouter-chat.py-ptc.nested-parent",
            "599cc94dd6426a0a87d94b17a9eef467e54593ae9d82dff0f6577e3c5a9b92d8",
        ),
        (
            "openrouter-chat.py-ptc.child",
            "efa2740d7ac083f00410a5253b6b94ea9dd14546761b7a2d6d6bfa2c397fb118",
        ),
        (
            "openrouter-responses.direct.root.single-agent",
            "ffdd5b0f51e4e5e9af2f21991d51b94ce7876fda826c381232840c68632710e7",
        ),
        (
            "openrouter-responses.direct.root",
            "666937f4c237f88eaf8d5af448d43c69aa9a809097486123000862136a974434",
        ),
        (
            "openrouter-responses.direct.nested-parent",
            "09d53744ab3217c5098a2dc4bb6b4700ac5d0800a3aa86895349eb78898ebdff",
        ),
        (
            "openrouter-responses.direct.child",
            "2fd8b00db3a02f35deeac960099abed636ae3d619b101abbc5b2cf93a79528c7",
        ),
        (
            "openrouter-responses.js-ptc.root.single-agent",
            "f6b484873060cd7e3716689c8def60a1b726e97ef11c90ba7c62100fe69a579d",
        ),
        (
            "openrouter-responses.js-ptc.root",
            "af93e243990078d3c7db270238a58ade9b8132c825e3c8618e99b2c6e406f1bc",
        ),
        (
            "openrouter-responses.js-ptc.nested-parent",
            "43f3c0f8376ca0c019a197c93825aef36b7b4e4d16ff34fef42b066632e11282",
        ),
        (
            "openrouter-responses.js-ptc.child",
            "64f583c4e076c0cfb160ee31e76bbdc40701502079b77aade56b0aabf2896932",
        ),
        (
            "openrouter-responses.py-ptc.root.single-agent",
            "61149fa5c3f4cb160ecc7e83b91271c3ff93d31363a3c9b822c2026002b38204",
        ),
        (
            "openrouter-responses.py-ptc.root",
            "dabfae00a9366e33672e41f93a4efdc972ef2fa7210ad23324e4a67bdeb78770",
        ),
        (
            "openrouter-responses.py-ptc.nested-parent",
            "1d67e81a5dba5f7c57edb29440b645865795833e0557f2848ba4ced7983ed395",
        ),
        (
            "openrouter-responses.py-ptc.child",
            "4bbab393a9a0dfd889ba14a4584b688db44bbb76f068e2966e3b6af217aa6bf3",
        ),
        (
            "openrouter-messages.direct.root.single-agent",
            "61348a32a0f1cb04ced54988aa1aef6b6984c925793c8458f79d65b42a303cc5",
        ),
        (
            "openrouter-messages.direct.root",
            "f1df33765c933e352bc7bb56aecb46db43e51d8ab146f6771f61530de54d603b",
        ),
        (
            "openrouter-messages.direct.nested-parent",
            "ae0260b5876c9cd084c5f84eb9fc3f894156501d3bc8f6718a29fad12ea06ec0",
        ),
        (
            "openrouter-messages.direct.child",
            "e4ff74315a22941e20d947cb1cca7e78563f9305e6c6b8e11539d2370c5f85a1",
        ),
        (
            "openrouter-messages.js-ptc.root.single-agent",
            "6c3b73d50ff13954294a0fa02562ca761444d31132f6891894e47dbbe35f0ccb",
        ),
        (
            "openrouter-messages.js-ptc.root",
            "117c6acad48c33ae8949662df1f3dc805cbb5308dc970c8c297c3231c7e55426",
        ),
        (
            "openrouter-messages.js-ptc.nested-parent",
            "b036ed5793a3c20f682d2278408ab2d6bf54dd4df8fa26f092ece54717d95309",
        ),
        (
            "openrouter-messages.js-ptc.child",
            "cb74354181eaa7be86dfe4822e55e054abd2cc2d3200650c2c744516f3408bb7",
        ),
        (
            "openrouter-messages.py-ptc.root.single-agent",
            "44e1ee298ea1a73a84f59a5b36becbf396cd35661f69bc8a38cfa3c71019bd7f",
        ),
        (
            "openrouter-messages.py-ptc.root",
            "2fab930cada530763789b1999fa0eb3268f386d7aa6a459ca765aa98fc540344",
        ),
        (
            "openrouter-messages.py-ptc.nested-parent",
            "41bd2363e827cb3c848fb04b707b14aae9a22e2eac892b83cd919e6dba78f6cc",
        ),
        (
            "openrouter-messages.py-ptc.child",
            "068e0b274fb6cbbc0b411b5dfc3e2d3dec144dba52417ac24f7b45d2e0ea4e9c",
        ),
        (
            "openai-api.direct.root.single-agent",
            "2201a34eddeb24bb950fb909247cb013f4e9a022b652e96139daafcd5126f42a",
        ),
        (
            "openai-api.native-ptc.root.single-agent",
            "6106c5de4c7daf6a87269fbdc597c8b0cbc5e666d79b30a9810a8b3f80c74c15",
        ),
        (
            "openai-api.js-ptc.root.single-agent",
            "63af8b845b123e0c159b9f9b89096811be37f08759b603ab2d78f0ab93b14548",
        ),
        (
            "openai-api.py-ptc.root.single-agent",
            "2b16b693197ed1e5f6854904e6fc69bd69161d55acb7be8387c0a56b123ad2e2",
        ),
        (
            "xai-api.direct.root.single-agent",
            "f8243bf04663a2ef591879d94ef9c8207e8fdc5a7693eea757ce75e4129c505c",
        ),
        (
            "xai-api.js-ptc.root.single-agent",
            "7e4e7f2ab0b52a7b8c02b1d1e9f8b981e09df591d1abbe7a2936d298ebedb6b3",
        ),
        (
            "xai-api.py-ptc.root.single-agent",
            "555e01d62f9f4dadf6b0aed0cc219428df12db158cdf3163ffaf138a3ac73a86",
        ),
        (
            "anthropic-api.direct.root.single-agent",
            "618ce29b5a3619842e7da7fbd3e59ee78299d4cb2ff9ed16508e23dae81e2710",
        ),
        (
            "anthropic-api.js-ptc.root.single-agent",
            "b67a9eeb0e4b56c8a27497c8f04d2c49e789a91c76e04854d854c14e9f0e2792",
        ),
        (
            "anthropic-api.py-ptc.root.single-agent",
            "d182fa7cba5533da9ea8d1f425ab7a74d7b442764ed77b6b83f78d8d74bbd63d",
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
                );
                if spec.composition == ProductCompositionSelection::PySdk {
                    assert!(surface.is_err(), "SDK child occupancy must refuse");
                    continue;
                }
                let surface = surface.expect("filtered surface");
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
            95,
            "included route matrix is providers×compositions×topology minus documented exclusions"
        );
        assert_eq!(excluded_routes().len(), 2);
        assert_eq!(
            PROMPT_RENDER_PINS.len(),
            95,
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
            "rendered prompt drift (inspect with `just prompts diff`; re-pin with PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_):\n\n{}",
            failures.join("\n\n")
        );
    }
}
