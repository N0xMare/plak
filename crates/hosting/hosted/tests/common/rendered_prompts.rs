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
            "dac8f041e0c33618e0f1a5399c37129ec0e6082f37370a72e58f33635e3a51ba",
        ),
        (
            "grok.py-sdk.root.single-agent",
            "e037a8fd94d7b60a8cebee6466bb0516337da5de1ddff794fa2200f207336bbd",
        ),
        (
            "claude.py-sdk.root.single-agent",
            "3b4dabe27aa98c34716818ea972cfd87402fdad35146a333212dcb3d81427242",
        ),
        (
            "openrouter-chat.py-sdk.root.single-agent",
            "ca199d67a1472df33961614e59f652fb69359e18c750599b72a2851dc7121918",
        ),
        (
            "openrouter-responses.py-sdk.root.single-agent",
            "56356f2cbd29c98a4a63f5d1ecc9e096bf6b6962df79ffca54d3a544a017020d",
        ),
        (
            "openrouter-messages.py-sdk.root.single-agent",
            "d024e4af691121975859072d868f0f7c75d10348a8cc068aa65ee947b167ad46",
        ),
        (
            "openai-api.py-sdk.root.single-agent",
            "622549ed68b1f6e91d2ba866242c8ed5bb638804a85ea4e57665104574671066",
        ),
        (
            "xai-api.py-sdk.root.single-agent",
            "c0067b5bbe21db83a69ff631ce305a55e728208031955fc8eeaf4973392b3f49",
        ),
        (
            "anthropic-api.py-sdk.root.single-agent",
            "8a8d123d4be62770854f725c3376128e09b721110ee32e425997ce66738d0bc6",
        ),
        (
            "codex.direct.root.single-agent",
            "9b0afadf5dcb8e78a6295ff3a4c8a31a70f58aef284571b2c1bec428a2aabbe1",
        ),
        (
            "codex.direct.root",
            "d8c65d5f9931ae0c477d2e166b77c9a97e795b113a79cd77836c940a2258be55",
        ),
        (
            "codex.direct.nested-parent",
            "34694520fce6bc528c76ffef28a93730b29a6c82ca199d45a53582db85ec894a",
        ),
        (
            "codex.direct.child",
            "11e63b7e9c96315ab51abf0bad5b5ce74d1d2a0bc52c25aa8c1c9e8117454410",
        ),
        (
            "codex.native-ptc.root.single-agent",
            "895af17431c30cfd9b6476af3276187fcd6d754c49ab9da79c5aad68b80036d4",
        ),
        (
            "codex.native-ptc.root",
            "c5c5935871adddbe523d359d9ca7074823bf46dadca8920f10d8ef1b0755e0b2",
        ),
        (
            "codex.native-ptc.nested-parent",
            "39d139fb8190c82f27b0e1117770f801990385439b01b87f89a88c429a5f0e28",
        ),
        (
            "codex.native-ptc.child",
            "97862fc5469367fdc6ce4106c30bd1018a3eccbd393ccaff0d97b2a54818b23e",
        ),
        (
            "codex.js-ptc.root.single-agent",
            "f04fe9e70a7eb6e317be7a52e212f131e42f7353e571057feb43c50a5d73df32",
        ),
        (
            "codex.js-ptc.root",
            "23e1324f9515fcda8bbdfdf4020479044bce228d5f9f2ea7013239ba114e6aae",
        ),
        (
            "codex.js-ptc.nested-parent",
            "036c7cd9610ac0d585908586b4de41f9fb92a7001869809c0aba628452cb091a",
        ),
        (
            "codex.js-ptc.child",
            "bca6f3979d84a93503a9020944812543e4a7e746cdab9e43d4c52b2a15b5c083",
        ),
        (
            "codex.py-ptc.root.single-agent",
            "4c4bf9c1cb8a34455068071115556c442fe12b5291732c7e8c8bcca5fc50791a",
        ),
        (
            "codex.py-ptc.root",
            "05f185dfe57c2a5635aab8fc8872a833f5855f0b40ca9a80076f49320e5b5c78",
        ),
        (
            "codex.py-ptc.nested-parent",
            "ad89d6e4411848d66d64e50e708416f80974d762ddfcaabbf53e5068a7105d6f",
        ),
        (
            "codex.py-ptc.child",
            "42cbc9e19ece977abc2d0ecac7322357f6e46273c25e55b90eeb6ea07f3028a4",
        ),
        (
            "grok.direct.root.single-agent",
            "916432ac7e3e580caa50eabe989f75ff525793769b82572d81352401976b6934",
        ),
        (
            "grok.direct.root",
            "5029268368616dd3e0ef6386d6d078c68b4aa937207178db82cc0225e0ca410f",
        ),
        (
            "grok.direct.nested-parent",
            "1ac5ff4b3afed9e9892b2757e28c1fee5fb815fbaaba2ace26e45d8bc300c382",
        ),
        (
            "grok.direct.child",
            "c0ded61e4507fa622059a8d9dcc5971148a503d17fb3a4f85f4c86d0f5a01cd5",
        ),
        (
            "grok.js-ptc.root.single-agent",
            "37086dfb58d730df067d0ff1e4cc4d6b5cf85e0f891c3904763a3c3b24f433e2",
        ),
        (
            "grok.js-ptc.root",
            "74bbe1fc0a4d1a31b8d89d89be1b489b4c853f11dc7da38a58bccade2bdd8e4e",
        ),
        (
            "grok.js-ptc.nested-parent",
            "e90c60e0ca71dbd5b1dd3d91b261de1df055002a7dcc0fda3109761831e3fcee",
        ),
        (
            "grok.js-ptc.child",
            "466381b26f12dc3eceaf38dff578586b336b30728c7bf3887320178fb60a8ef3",
        ),
        (
            "grok.py-ptc.root.single-agent",
            "b080a8f99ca90c3489d4b0b4c80290bdeead922e7f2eadf1e1a82e3af57df523",
        ),
        (
            "grok.py-ptc.root",
            "b744c8e55c00953854a1187c433a21f4c648ff6ae00970cc85eebb633eba0ba8",
        ),
        (
            "grok.py-ptc.nested-parent",
            "0ed468d698c8c02e0e0768df063d657bc6ce437729d26fa3d9416d16291e1669",
        ),
        (
            "grok.py-ptc.child",
            "56fbcf38febf993d3c00fd19989d0c737b9755f87c86e8355882724eb17b1b12",
        ),
        (
            "claude.direct.root.single-agent",
            "03148e084bf0e8768824e9e0597c9a124d023c9d6fc9465808e770b8f9bdb4fa",
        ),
        (
            "claude.direct.root",
            "b156d8c6de2c715e2a823077baf98fbcacfdaff98b1ca278bb115448b13b7292",
        ),
        (
            "claude.direct.nested-parent",
            "092d00edd444cfab0fd687e31eaa19454c7d32c79f9f616501a5ee5a109d89ec",
        ),
        (
            "claude.direct.child",
            "a1067f44a230bdfb96bdf46a085e3e75e8c4834921fa5429c62d50f8bcdd21e4",
        ),
        (
            "claude.js-ptc.root.single-agent",
            "c3ff4590f0dee6d50cbeffa9dae0ae25a660474610946e3f22c0d833300167bb",
        ),
        (
            "claude.js-ptc.root",
            "8e50f7ff507c841f8392f333f93e141e97ed247dbf54f10ac2efa7f739dd0d86",
        ),
        (
            "claude.js-ptc.nested-parent",
            "05ae2b9191fb594a652cd691617019c27e7ac8493324cd73fbe8f5b5af44482b",
        ),
        (
            "claude.js-ptc.child",
            "8ec6aaa7803e15e657cde48a7a5b93afd9020b25e447e056879d52eca035505a",
        ),
        (
            "claude.py-ptc.root.single-agent",
            "a98f2d436fc7d3d24c8100fe72f1e503855e849db330c603ba3c76709a172196",
        ),
        (
            "claude.py-ptc.root",
            "fa262f957984e921876165625cb78236b3b2cf73a0b3a2408957f3696fe5f0d5",
        ),
        (
            "claude.py-ptc.nested-parent",
            "9291908af78dc6f3e4591a5796808e2b712c1e311411880196971525922b56d7",
        ),
        (
            "claude.py-ptc.child",
            "242cba30d7e710697345c403026977bdc81f010ba0cbbf885ef58c00be6756be",
        ),
        (
            "openrouter-chat.direct.root.single-agent",
            "82d92935087e580b2b80d594329580e5491995f0faeec87134a4d1909a5e68cb",
        ),
        (
            "openrouter-chat.direct.root",
            "c54e12121c947150bc904b8ec692b6fbbc84855e33747ab97ce5f81f75d26452",
        ),
        (
            "openrouter-chat.direct.nested-parent",
            "86cafaaa79c506a9077e30ca82c08d679a76151048d3ee86ee4cd4853071134e",
        ),
        (
            "openrouter-chat.direct.child",
            "a5db73a9ba18cf5b520097f17ce1fa5ac564490c4c4854e1c8f4a5cea1ebabbf",
        ),
        (
            "openrouter-chat.js-ptc.root.single-agent",
            "7d28e058571783237a6056deac9068ec293df70397122eea9b4c75598f70d81c",
        ),
        (
            "openrouter-chat.js-ptc.root",
            "b8a4437589a59e5d9d2c1d7c27ba7c2c5f3320b3326910fbd7acb51f8e5fbfb9",
        ),
        (
            "openrouter-chat.js-ptc.nested-parent",
            "22beebc141c6d74dd8571d1252b8bae353335752a178ce5cb587b99811686d56",
        ),
        (
            "openrouter-chat.js-ptc.child",
            "847bcbe06166f71acfcb5cc64d0e223aed01baa72d3dbdd8dac68192da8ea92f",
        ),
        (
            "openrouter-chat.py-ptc.root.single-agent",
            "29d7c88a473125abeb4a2234dd24bbb07f9e3d78b89bb421172df608f4001da4",
        ),
        (
            "openrouter-chat.py-ptc.root",
            "40a267d3a5ecd79d22eac5a2aa338a1fb357148da3cf04f825181f4584d7d1ad",
        ),
        (
            "openrouter-chat.py-ptc.nested-parent",
            "0a85749cde82f6f48fea160c7747b48a28d53e406e6b898a670da371140ac6a2",
        ),
        (
            "openrouter-chat.py-ptc.child",
            "d2e28ebd55a48128cdae0b87f582f73a8af63cd0679929dd2f7ba2665a33a578",
        ),
        (
            "openrouter-responses.direct.root.single-agent",
            "f219820be137d9c0c466fa6a029f64e9846a9476723882d268c0f76207a9cc82",
        ),
        (
            "openrouter-responses.direct.root",
            "22ad190bdc7c4bdb0dfe042666635224e170cb916463564ce7caa1d867a66632",
        ),
        (
            "openrouter-responses.direct.nested-parent",
            "d98b8af0507024ea6495fe5cc75ad21e9b96fa2f6f6f3b05613c2c9603d08ae5",
        ),
        (
            "openrouter-responses.direct.child",
            "d31224d0c7e32accf3ec8a8ebae48d672812f094b817f34415a06077052ee0ef",
        ),
        (
            "openrouter-responses.js-ptc.root.single-agent",
            "63c16f334906ad84047460749bc124f1a6b3aa07a3fe02cee3d787c75222c357",
        ),
        (
            "openrouter-responses.js-ptc.root",
            "55dbc06e9c0a02e8b756188d975e6435c290d9ad6b0496122fd23b61558440fc",
        ),
        (
            "openrouter-responses.js-ptc.nested-parent",
            "97c96631efcbb8bd0226ac26f09b1a288edc08bae6a954e8ad6c53ba7421f2a7",
        ),
        (
            "openrouter-responses.js-ptc.child",
            "5834df7ab95eb8b4d248a369d6bea4bb38b5094dd79f55c72d418f2504a7d1ed",
        ),
        (
            "openrouter-responses.py-ptc.root.single-agent",
            "b0ab5ec744f18f208f62651e1885f4c70cb56d7a31377c9b800349111ead337d",
        ),
        (
            "openrouter-responses.py-ptc.root",
            "fc953f4e4c061cdce370304ccf1f062d09c48700b2d4ac268c8630bf27e2b5f3",
        ),
        (
            "openrouter-responses.py-ptc.nested-parent",
            "1a754230a994e037269853a6a83a5af20ae54904a2a1a8f50516eb91f2d6e7a1",
        ),
        (
            "openrouter-responses.py-ptc.child",
            "d6b1e92bef22ccb11fb2fe695e777651051471405d5b832523e4f6da22e211a5",
        ),
        (
            "openrouter-messages.direct.root.single-agent",
            "dd4795979665ba4b7387e961c4f10b2ab0f7225fc52b87a6944f461a73dbf6b8",
        ),
        (
            "openrouter-messages.direct.root",
            "bc80cf93698a2692cb05c33489716024c19bde91e96eff6fd5b3f98943c9afac",
        ),
        (
            "openrouter-messages.direct.nested-parent",
            "50390500c0a63f7c41d904b84215ce560d9624681babf60803173efa94020867",
        ),
        (
            "openrouter-messages.direct.child",
            "6ce84365ed674dc37cbeebbb416ed63834240d53a19704c35e354904b205b313",
        ),
        (
            "openrouter-messages.js-ptc.root.single-agent",
            "5ce5c5b5003cf380006a9d87befb098a0c19fd447e14771803faf51c4fae7a11",
        ),
        (
            "openrouter-messages.js-ptc.root",
            "a0ebee53eccb66a1e99a0756605d37abb895530ca835c8798b317dd5d4db7019",
        ),
        (
            "openrouter-messages.js-ptc.nested-parent",
            "b4460a633847255b80d39e8c662628a00b43709f36452cf434cbadcd8dbf919d",
        ),
        (
            "openrouter-messages.js-ptc.child",
            "fdb9783542f493799f0ae12cef4547117de134f76b6d9662476bee21610b3fe0",
        ),
        (
            "openrouter-messages.py-ptc.root.single-agent",
            "18a36dac93466b12d9ebddccbfdd0959e2c088957162f90ae8354b6a8d973826",
        ),
        (
            "openrouter-messages.py-ptc.root",
            "cf7fc2ab87715db5e6cefc0b06595435479dd89b811f1b64cdea868b7cbe5d7b",
        ),
        (
            "openrouter-messages.py-ptc.nested-parent",
            "c8233930de144b67476a28d352b54f0e8f903eca9f904e5176bcdca266d96bcd",
        ),
        (
            "openrouter-messages.py-ptc.child",
            "9b952c52c096f567ae9fc5e9fcd75908fbb15b54296506ee0301e796a852577e",
        ),
        (
            "openai-api.direct.root.single-agent",
            "6a25a8d28859c1326968723bf1e62b88d4f780fcfd72aa9aec5f6a7451e724ca",
        ),
        (
            "openai-api.native-ptc.root.single-agent",
            "8c413e1d25478a9b5d75ab14515ed3a14f9d87b786bbac6d36d0d239797bae1f",
        ),
        (
            "openai-api.js-ptc.root.single-agent",
            "e227a898b2cb72c87739ac25fb662a15b860d579ac960dccb956e09266213b4c",
        ),
        (
            "openai-api.py-ptc.root.single-agent",
            "0e8477fc8669f09779bfaf74a27df1e08963d8595a15449aaba62b0cbf6de036",
        ),
        (
            "xai-api.direct.root.single-agent",
            "e87561cc3b3a87ad5a7dc0165e79f55cf25b38741e79c297b732918a82cd6e4b",
        ),
        (
            "xai-api.js-ptc.root.single-agent",
            "c3ce0cbf634ac9b7c5598f321831bbf2014f43a693e848363901cef7bbdb2966",
        ),
        (
            "xai-api.py-ptc.root.single-agent",
            "001be0657fd197bb5bec77f580b01f293391d11e9dfc8c39a0157307d30d5374",
        ),
        (
            "anthropic-api.direct.root.single-agent",
            "d225d483bcbe5b156e8c29ff5fd0a3206b77da90f5c5c99801fde7991c0235bf",
        ),
        (
            "anthropic-api.js-ptc.root.single-agent",
            "b495e4260864cbccce416e14f94baf5c40f0785ba367d23a840524edc3b38b5b",
        ),
        (
            "anthropic-api.py-ptc.root.single-agent",
            "2194dc3e32969e21cbcfd0313ce3a1ef95cd19f56401d134ae69e1047c54835a",
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
