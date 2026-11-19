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
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use plakous::agent::{
        HarnessProfileSpec, ReasoningEffort, RegisteredContextSource, ResolvedContextSourceFact,
    };
    use plakous::model::{
        Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ModelId, ToolName,
    };
    use plakous::prompts::skills::SkillsPresentation;
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
    const SYNTHETIC_HOME: &str = "/home/user";
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
        /// The same hubs on a pi (`OpenRouter`) node: the await and interrupt
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
            let _ = writeln!(body, "| {name} | {value} |");
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
        plakous::model::sha256_hex(&[text.as_bytes()])
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
        for (skills, name, description) in [
            (
                root.join(".agents/skills"),
                "review-pr",
                "Review a pull request against the project checklist. Use when asked to review a PR.",
            ),
            (
                tmp.path().join("user/.agents/skills"),
                "release-notes",
                "Draft release notes from merged changes.",
            ),
        ] {
            std::fs::create_dir_all(skills.join(name)).expect("skill directory");
            std::fs::write(
                skills.join(name).join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n"),
            )
            .expect("skill file");
        }
        (tmp, home, root, cwd)
    }

    /// The skills listing family one route wears, as production selects it.
    pub(crate) fn skills_presentation(spec: &RouteSpec) -> SkillsPresentation {
        match spec.provider {
            _ if spec.composition == ProductCompositionSelection::PySdk => {
                SkillsPresentation::Neutral
            }
            ProviderKind::Codex | ProviderKind::Platform(PlatformProvider::OpenAi) => {
                SkillsPresentation::Codex
            }
            ProviderKind::Grok | ProviderKind::Platform(PlatformProvider::Xai) => {
                SkillsPresentation::Grok
            }
            ProviderKind::Claude | ProviderKind::Platform(PlatformProvider::Anthropic) => {
                SkillsPresentation::Claude
            }
            ProviderKind::OpenRouter { .. } => SkillsPresentation::Pi,
        }
    }

    /// The fixture's project and user skills in one family's listing, with the
    /// temporary roots spelled as the synthetic workspace and a synthetic home
    /// so the render does not depend on where the fixture lives.
    pub(crate) fn fixture_skills_listing(
        presentation: SkillsPresentation,
        root: &Path,
    ) -> RegisteredContextSource {
        let user_home = root.parent().expect("fixture base").join("user");
        let setting = crate::skills::SkillsSetting::new(true, Some(user_home.clone()));
        let skills = crate::skills::NodeSkills::discover(
            &setting,
            crate::skills::SkillsMode::New,
            root,
            presentation,
            None,
        )
        .expect("skills discover");
        let listing = skills
            .listing_source()
            .expect("skills listing")
            .expect("fixture skills are listed");
        let canonical = |path: &Path| {
            std::fs::canonicalize(path)
                .expect("fixture root")
                .display()
                .to_string()
        };
        let content = listing
            .content()
            .replace(&canonical(root), SYNTHETIC_ROOT)
            .replace(&canonical(&user_home), SYNTHETIC_HOME);
        RegisteredContextSource::new(
            listing.source_id(),
            content.clone(),
            format!("synthetic:{}", render_digest(&content)),
            listing.provenance(),
        )
        .expect("synthetic skills listing")
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
                max_children: u16::try_from(MAX_DIRECT_CHILDREN).unwrap(),
                max_nodes: u16::try_from(MAX_TREE_NODES).unwrap(),
                max_attempts: u16::try_from(MAX_TREE_ATTEMPTS).unwrap(),
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
        let mut tree = crate::agent_tree::block_on_for_test(ProductAgentTree::bootstrap_root(
            "golden-tree",
            "root",
            "root",
            workspace.to_path_buf(),
            "gpt-5.6-sol",
            WriterMode::SingleLease,
        ))
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
        profile: &HarnessProfileSpec,
        presentation: SkillsPresentation,
        home: &Path,
        root: &Path,
        cwd: &Path,
    ) -> (HarnessProfileSpec, Vec<ResolvedContextSourceFact>) {
        let listing = fixture_skills_listing(presentation, root);
        enroll_product_instructions(profile, Some(home), root, cwd, Some(&listing), None)
            .expect("enroll AGENTS.md and skills")
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
            extras.extends_beyond_skills(),
            composition,
            surface,
            plakous::prompts::identity::PromptWorkspace::new(
                Path::new(SYNTHETIC_ROOT),
                Path::new(SYNTHETIC_CWD),
                std::env::consts::OS,
            ),
        )
        .expect("harness profile")
        .expect("new session has a harness profile");
        let presentation = match provider {
            _ if composition == ProductCompositionSelection::PySdk => SkillsPresentation::Neutral,
            NativeProvider::Codex => SkillsPresentation::Codex,
            NativeProvider::Claude => SkillsPresentation::Claude,
            _ => SkillsPresentation::Grok,
        };
        enroll(&profile, presentation, home, root, cwd)
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
            plakous::prompts::identity::PromptWorkspace::new(
                Path::new(SYNTHETIC_ROOT),
                Path::new(SYNTHETIC_CWD),
                std::env::consts::OS,
            ),
            surface,
        )
        .expect("openrouter profile");
        let presentation = if composition == ProductCompositionSelection::PySdk {
            SkillsPresentation::Neutral
        } else {
            SkillsPresentation::Pi
        };
        enroll(&profile, presentation, home, root, cwd)
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
        let skills = crate::skills::NodeSkills::discover(
            &crate::skills::SkillsSetting::new(true, None),
            crate::skills::SkillsMode::New,
            Path::new("/nonexistent/workspace"),
            skills_presentation(spec),
            None,
        )
        .expect("skills discover");
        occupancy_extras(spec, tools)
            .with_skills(&skills)
            .expect("skills extension")
    }

    fn occupancy_extras(spec: &RouteSpec, tools: &OccupancyTools) -> ProductToolExtensions {
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
            &provider.account().parse().unwrap(),
            spec.composition,
            Some(API_EFFORT),
            provider.protocol(),
            spec.model,
            extras.extends_beyond_skills(),
            plakous::prompts::identity::PromptWorkspace::new(
                Path::new(SYNTHETIC_ROOT),
                Path::new(SYNTHETIC_CWD),
                std::env::consts::OS,
            ),
            surface,
        )
        .expect("API profile");
        enroll(&profile, skills_presentation(spec), home, root, cwd)
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
                // Production hides `view_image` and `inspect_image` from the
                // cell: image results are not a cell's text catalog, and the
                // native Code Mode outer surface does not list `view_image`.
                let nested = crate::agent::build::api::code_mode_nested_names(&surface);
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
             This total is dialect costume plus family/product/PTC instructions plus the compact `tools[]` JSON. It excludes enrolled `AGENTS.md` and the skills listing ({} bytes) and this scenario's user/assignment message ({} bytes).\n\n\
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
        let _ = write!(markdown, "\n# `{}`\n\n", spec.file_stem);
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
            "25198003d5f74f35f20c0e176ad55c1b49893670558d95a3c0a351a3e21b940f",
        ),
        (
            "grok.py-sdk.root.single-agent",
            "8e64b1dd7a026370b1868aa5eeaee39cf7a6063d8b09a663473fed8ac15389f3",
        ),
        (
            "claude.py-sdk.root.single-agent",
            "a75651eb72c1d76b546d10a14716955688252b96298fdda6c4c12ce286039e90",
        ),
        (
            "openrouter-chat.py-sdk.root.single-agent",
            "7dba6d9eb0cb04c94494d7340260f8c16eea7a57a3f1e7454bbe31ccd55ea043",
        ),
        (
            "openrouter-responses.py-sdk.root.single-agent",
            "2042a9ff9f786728280365100530decaa260ec81002c4d7175753ec3864eadc2",
        ),
        (
            "openrouter-messages.py-sdk.root.single-agent",
            "6197cc2de080c1aaa82162f6975cdc98567aa75200d935087606f7f2a6501e78",
        ),
        (
            "openai-api.py-sdk.root.single-agent",
            "c55d245c40399a567e07ba6e800b1efd15f00760fb1844bc1d3d71b36284ba0d",
        ),
        (
            "xai-api.py-sdk.root.single-agent",
            "dac488feb841fed9f7205b87af6c2ee92085e034a8ebd95031def0a1319efe14",
        ),
        (
            "anthropic-api.py-sdk.root.single-agent",
            "cf218e001ab5c808f1017801f725072c8589d0086e93d102a05a395de0b68973",
        ),
        (
            "codex.direct.root.single-agent",
            "92c7064310b0372c9b60c30744dfc6c68fcb6ad237cab2de3501c5ca7b82d798",
        ),
        (
            "codex.direct.root",
            "995830330fc216053754cc93b65c0e856cd4bbdddc11f6187125855a1b25a038",
        ),
        (
            "codex.direct.nested-parent",
            "e71d8cad1f71e9c70d5bd5c038e695437c04d3cea29d66098b17b1efe8bdc368",
        ),
        (
            "codex.direct.child",
            "ce25c3a470ad816f71cc471a5695ef7fffe7f01971480a54fe9f4d6aabeed424",
        ),
        (
            "codex.native-ptc.root.single-agent",
            "bfb0053e199773b4cf861f145c298018756e0421faa8fb54ff762c45b6cb9a7e",
        ),
        (
            "codex.native-ptc.root",
            "abd859c8b9f522caabfe5be9121afefd398903183d072b0e171c90871a234d61",
        ),
        (
            "codex.native-ptc.nested-parent",
            "5b4c3d2f258b34d6efadfd7f5418dbe3000f301c991f57844a185132b53fcd5e",
        ),
        (
            "codex.native-ptc.child",
            "efacd8694c58805a73b6e404b9997304a71644fbd0c954464f071126d57d1065",
        ),
        (
            "codex.js-ptc.root.single-agent",
            "c3da0b9643f12fdd4adceb9d1df79be32c1d605886aa99c53554a40d0ba3e6de",
        ),
        (
            "codex.js-ptc.root",
            "5ec536ed96a5f1713e33d4603d682174f1517edd5eae5d85f15bae0d20c8f26b",
        ),
        (
            "codex.js-ptc.nested-parent",
            "700126a1bcf6d2d05e77e317e6c3847f4c7dbd5a59c1f78e7086cb369d4de21d",
        ),
        (
            "codex.js-ptc.child",
            "7ade1e5fd267974a0899d2d2deea51ac535a6bc9048e97ca127b1b82f668168e",
        ),
        (
            "codex.py-ptc.root.single-agent",
            "99c5b2beace9d1ce9c24a51f046f3ad7d8ea0d550dc1252da90ff0fc9e994995",
        ),
        (
            "codex.py-ptc.root",
            "45880e1182aacd8599c97958d46d8f878a89a72ccf5346ebd29b742357a751c8",
        ),
        (
            "codex.py-ptc.nested-parent",
            "b330df58796c88e2cd79b231aa42c05c52d64232e20454ddd174f176bbb48d02",
        ),
        (
            "codex.py-ptc.child",
            "37fef414003da9a29f57f9fc2c8d485d9311224192325889122bfb923d188caf",
        ),
        (
            "grok.direct.root.single-agent",
            "ecf167149bf4d7c0c4d1d241e78604bb8c5ee68e814e77d5dc5a40e03669f69b",
        ),
        (
            "grok.direct.root",
            "9b3b864f7f2844cd25b7bf15d7335f1d43552ccb463a26633953cad700398bfc",
        ),
        (
            "grok.direct.nested-parent",
            "3f26859106dd8b71689e9c45e76537d9de0c50de11a86117709e39cbd9bcffd8",
        ),
        (
            "grok.direct.child",
            "ef7d7892b77d94de8203c21e119c5add5d5793820ffbf4a68e526b93cb5735af",
        ),
        (
            "grok.js-ptc.root.single-agent",
            "c64336a94ff191512fbb1e3bfef842520f86d8e9fffd1edf68e34ec6286f8c9b",
        ),
        (
            "grok.js-ptc.root",
            "130cffcf630d1d07b8b36b793a168eae58e5f321b9c5cfeeefbb6a92ff0f4ca7",
        ),
        (
            "grok.js-ptc.nested-parent",
            "4524d63182ae8fcc39aa129d3d5dc202058a595da0e7955afd2da9d3ed0cb4ec",
        ),
        (
            "grok.js-ptc.child",
            "a665ea0d51893230cd17e0e4be20c8a579eaebdeb9f817d64715be2edb82b9b1",
        ),
        (
            "grok.py-ptc.root.single-agent",
            "f4bb87abfab107af7f15798db6f0e4da17f2ef313d3679aa29e582dca3da46d2",
        ),
        (
            "grok.py-ptc.root",
            "d15b289c3b02d01a1dedafdfe81f45afe1f0e52edc9d00a088dc64bb4620d239",
        ),
        (
            "grok.py-ptc.nested-parent",
            "35452641d8c2b8799ae8a90f0abb9a53756e42e3d17adbac120efc9ff6339461",
        ),
        (
            "grok.py-ptc.child",
            "f5ac09529e099a72dbe2f802f817a976d4c9a01efd0d70105ba3124dddc71236",
        ),
        (
            "claude.direct.root.single-agent",
            "9c1f64fef89c4ef1e2c819359169ebe5a0a5a5ecd6dc075d20d2f9e8cb9ae1df",
        ),
        (
            "claude.direct.root",
            "c333efc40682e009967401c41eb8ef933d5f518da167be6bafde6d48ea4a5124",
        ),
        (
            "claude.direct.nested-parent",
            "643a3e0df4a9ebf574bf8f67c27d9846a14fc6d97459656e606c2d26113a461a",
        ),
        (
            "claude.direct.child",
            "a660690522c8f2ca20eb329dceb831393a87fcb91625af1b6dbedf1cf72e66b4",
        ),
        (
            "claude.js-ptc.root.single-agent",
            "e7aa60c6e0c452aba715dac2e8bdb4e811bf1055e5c84a8bac3acb05d38ab49c",
        ),
        (
            "claude.js-ptc.root",
            "dc2c8f3806e2e266dd7a9ac5890a69315562787b714dbd8122d95ea59eecdb7b",
        ),
        (
            "claude.js-ptc.nested-parent",
            "5afd197f5bf904a6173eff5f9224a6fcae8a9707a05ae1029f0842f01710fb24",
        ),
        (
            "claude.js-ptc.child",
            "6674aa9b9a6836c70597b9f2f7aee6d7fbbd91d859de0c64eb2f268f16a851c0",
        ),
        (
            "claude.py-ptc.root.single-agent",
            "f81663299c5ba2c77b63a0891ac38a18ae7f841939caebfeee28c36edbcecfa0",
        ),
        (
            "claude.py-ptc.root",
            "b3871e9d467c54b87e55a828ef40e274fef9a75127662c8954e880abca9e2f02",
        ),
        (
            "claude.py-ptc.nested-parent",
            "4a42b5b0d650f7846202216e52ba95c81e953355a50b8faafda98feb7e3ff70d",
        ),
        (
            "claude.py-ptc.child",
            "ecc86701af091aac57e3a069151042768fc591a3669ca4a1bc86701ace906f1c",
        ),
        (
            "openrouter-chat.direct.root.single-agent",
            "03b2f439393d1a4ada1f2f441f5a645c7b1df93697893aac02d8cf640dfec8c4",
        ),
        (
            "openrouter-chat.direct.root",
            "b90e7830a818ffae42fe1181804be3c3004ff081396818610fc14f6ed81946ff",
        ),
        (
            "openrouter-chat.direct.nested-parent",
            "03c01d29eaad85bb75495f974ce27aa4ffb56fbcdd14f22d6bdeb66940e93dbc",
        ),
        (
            "openrouter-chat.direct.child",
            "141f75c348cf429673ecce63ea6e7594576fb3e9931e082649f5e1c8626929d2",
        ),
        (
            "openrouter-chat.js-ptc.root.single-agent",
            "499d0fa7b9093ba6388d9fd28c398c93d70f2816903890b799df2552782850c3",
        ),
        (
            "openrouter-chat.js-ptc.root",
            "5519ab3284ab0b19db97903980cb7ff62b734aadd86ec968c2cb97c06ca204cb",
        ),
        (
            "openrouter-chat.js-ptc.nested-parent",
            "6c51bb10e0a26f55ad07209d42d4fffc5d6d0bef6d6638af69344d59d1453a96",
        ),
        (
            "openrouter-chat.js-ptc.child",
            "ea11c602ec2dc6b1a58dbb2097668440113f39b51240b2ef3d45638719ff1895",
        ),
        (
            "openrouter-chat.py-ptc.root.single-agent",
            "0d720dbbbffd16ac9d2b3ef90a7e8781c7b062fa17f726dd70b8995db4e1de3f",
        ),
        (
            "openrouter-chat.py-ptc.root",
            "715d4bbb09a08898fd785248d7eeea793df5eb7a8d58909de3b684f40c8532a9",
        ),
        (
            "openrouter-chat.py-ptc.nested-parent",
            "c15cae13e35ba78a1588d8c7258675385740b2e534868f7653fc0cc6e43b21f0",
        ),
        (
            "openrouter-chat.py-ptc.child",
            "71f6cbab78dc341034953d943298bb79953f04873eb76fca2a773eb081cc46ca",
        ),
        (
            "openrouter-responses.direct.root.single-agent",
            "1d7cc1147bf03db358d8c43b65a5cfbe42b14d6710b2a8db77e384b864df8fd5",
        ),
        (
            "openrouter-responses.direct.root",
            "426c8e424f531ba49e76c49294fa60542db0d1a6a00a7851eccf1224ef30f60e",
        ),
        (
            "openrouter-responses.direct.nested-parent",
            "02bf8c9bd58c65c892ad6d020b67ab35a4fbfa815a937e94b917328440b4f65c",
        ),
        (
            "openrouter-responses.direct.child",
            "2efbcb245c740c3346ac143529d1d48e2d4c5a7148abde3c6b741b0717fabcc3",
        ),
        (
            "openrouter-responses.js-ptc.root.single-agent",
            "5ff2fb1cc5e6f5abc2e78f82dca1d9bdabc67a4198498121badeb6c8c94f718e",
        ),
        (
            "openrouter-responses.js-ptc.root",
            "f92e20ff68683a666fc06bb39b72d15a6e3836aa684fa9e9d4da946912d49fc9",
        ),
        (
            "openrouter-responses.js-ptc.nested-parent",
            "10062c18ce3c253df8da0679888ecb7960ce8a5eed06e90d6e3a6a2be4da0b92",
        ),
        (
            "openrouter-responses.js-ptc.child",
            "f7c1636e08918e76e5da0586f7d14c9653ff6d87f281335c5818734255c822be",
        ),
        (
            "openrouter-responses.py-ptc.root.single-agent",
            "7e6a2ca58db48b941302e8f94a45cd402aaf7225b3b6d9b0980a97f1d0903a34",
        ),
        (
            "openrouter-responses.py-ptc.root",
            "4d1561aa6a7f18954f5549ece962f44e249d468bc329fe4635a51777be00fa9a",
        ),
        (
            "openrouter-responses.py-ptc.nested-parent",
            "313cf182b9055614a003270590aa883b6e0c6ae055fe78bc3f78cbbfec0b06be",
        ),
        (
            "openrouter-responses.py-ptc.child",
            "bbeb233f3b0a471935922892f5e4da0e47b50655a2f05c4dcca9c27a1422a69b",
        ),
        (
            "openrouter-messages.direct.root.single-agent",
            "532cd4531636f2e624cfbb61c4ff4fdca3815c88deda2c3032b581b54c1b4110",
        ),
        (
            "openrouter-messages.direct.root",
            "43f3f596e90c04f2eaeab5383066c86890279e55b7b2eeb68b26720dca2fb663",
        ),
        (
            "openrouter-messages.direct.nested-parent",
            "e8ecd7a11046d62c7a64afc6e1da89662485a98ade7b496a649704843c460fe0",
        ),
        (
            "openrouter-messages.direct.child",
            "429adac304d5fa792359902464d5cc91580661009d01de179e98563393e97786",
        ),
        (
            "openrouter-messages.js-ptc.root.single-agent",
            "1ccf0317caa1e4d65623accf84ebf7d85e1b1f79b853090be68d0dcd1e5f5682",
        ),
        (
            "openrouter-messages.js-ptc.root",
            "06f0dfdb426fa7b35f5e259884b89dc73a64be2b86860312a527c0ac26eb59b1",
        ),
        (
            "openrouter-messages.js-ptc.nested-parent",
            "ec39bc37b377e66a2ea1c78dfe555416efc229afb03c936356267c73c678684d",
        ),
        (
            "openrouter-messages.js-ptc.child",
            "aff8032d82cc65c20201558cf3d31aa21af340d9b645d82ec1775acb4bb92116",
        ),
        (
            "openrouter-messages.py-ptc.root.single-agent",
            "cc7da50eb3a985caf2b7ef93a68600d438ba29dca805885535b7ed1fd022a168",
        ),
        (
            "openrouter-messages.py-ptc.root",
            "a2de19676aab0e24c1df2cecbee8d11e3befa7cfc0dcae82b5be509acd31874a",
        ),
        (
            "openrouter-messages.py-ptc.nested-parent",
            "4dc922602b9b684af4f097a13d89833052b23be85dbede5b5d527aa08b672857",
        ),
        (
            "openrouter-messages.py-ptc.child",
            "49a90b03ef55464b1705d3ca00cce7b2c4313f30720e5a7791d49101ce36e323",
        ),
        (
            "openai-api.direct.root.single-agent",
            "90456225e07814b959eb436b53e0f0738f3d2e44963f9f2201b9513b4c3a4525",
        ),
        (
            "openai-api.native-ptc.root.single-agent",
            "efc3335323675df0e68fd82385a1c70485260ebc26e5c4ee2e014bfe44d2ffcb",
        ),
        (
            "openai-api.js-ptc.root.single-agent",
            "74da072a5f3e833f0733ec3b4c7a0b9c11001d6442049ae38addb6860276df6a",
        ),
        (
            "openai-api.py-ptc.root.single-agent",
            "a93f1a2a054430ed56eb0efcfa0a309bb2b5fe1935c93e3b014583a8f8a75ea4",
        ),
        (
            "xai-api.direct.root.single-agent",
            "7c102b0b9b67a89e9634adf9746b872ac5f72a6ddaec947672a510bb662c4c90",
        ),
        (
            "xai-api.js-ptc.root.single-agent",
            "bafcc44e7f2d913b42c948158e2ce7b7495a8a7b93304f76a8e658ddce57cb4e",
        ),
        (
            "xai-api.py-ptc.root.single-agent",
            "31beb75da18c4cd192e5cb6106743dfc2699749823459c93991f5a52c625abe0",
        ),
        (
            "anthropic-api.direct.root.single-agent",
            "91e33da9e9e855b6d21ecd7c6988269d9a4b7a24c712c438156f008ced7d113c",
        ),
        (
            "anthropic-api.js-ptc.root.single-agent",
            "9ca7c2aa7dbfe970e20bd957840549e7c667a76e1111f226023236497762c27e",
        ),
        (
            "anthropic-api.py-ptc.root.single-agent",
            "fd5d6d41f02c80e80d48a4e048bf2f56b337167c98b1a9511af71c5877e0cf97",
        ),
    ];

    /// On a js-ptc, py-ptc, or `OpenRouter` hub, `spawn_agent` is
    /// callable only inside a cell (or is a Direct extra on `OpenRouter`). The
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

    /// GPT-5 Codex native Code Mode uses the 4,000 single-node cap.
    /// `view_image` is not on that outer surface. The headroom check keeps
    /// 150 tokens under the cap.
    const NATIVE_PTC_GPT5_TOKEN_BUDGET: usize = 4_000;

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
            for model in [
                "gpt-6.2-sol",
                "gpt-6.2-astra",
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-5.6-sol",
                "gpt-5.6-luna",
            ] {
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
                    } else if spec.composition == ProductCompositionSelection::NativePtc {
                        NATIVE_PTC_GPT5_TOKEN_BUDGET
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
                        "{}: {model} lacks headroom ({total} tokens, budget {budget})",
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
