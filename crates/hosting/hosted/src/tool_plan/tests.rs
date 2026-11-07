use super::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn server(id: &str) -> TestResult<McpServerName> {
    Ok(McpServerName::new(id)?)
}

#[test]
fn selection_parse_table() -> TestResult {
    let accepted: [(&str, SelectedTool); 8] = [
        (
            "web_search",
            SelectedTool::Optional(OptionalTool::WebSearch),
        ),
        ("web_fetch", SelectedTool::Optional(OptionalTool::WebFetch)),
        ("x_search", SelectedTool::Optional(OptionalTool::XSearch)),
        ("fetch_url", SelectedTool::Optional(OptionalTool::FetchUrl)),
        ("image_gen", SelectedTool::Optional(OptionalTool::ImageGen)),
        ("mcp:github", SelectedTool::McpServer(server("github")?)),
        (
            "mcp:linear/list_issues",
            SelectedTool::McpTool {
                server: server("linear")?,
                tool: "list_issues".to_owned(),
            },
        ),
        (
            "mcp:my_srv/a/b.c-d",
            SelectedTool::McpTool {
                server: server("my_srv")?,
                tool: "a/b.c-d".to_owned(),
            },
        ),
    ];
    for (name, expected) in accepted {
        let parsed = SelectedTool::parse(name)?;
        assert_eq!(parsed, expected, "{name}");
        assert_eq!(parsed.to_string(), name, "round trip {name}");
    }

    let unknown = |name: &str| ToolSelectionError::Unknown {
        name: name.to_owned(),
    };
    let bad_server = |name: &str| ToolSelectionError::InvalidMcpServer {
        name: name.to_owned(),
    };
    let bad_tool = |name: &str| ToolSelectionError::InvalidMcpTool {
        name: name.to_owned(),
    };
    let long_tool = format!("mcp:github/{}", "t".repeat(MAX_MCP_TOOL_NAME_BYTES + 1));
    let refused = [
        ("", unknown("")),
        ("WEB_SEARCH", unknown("WEB_SEARCH")),
        ("websearch", unknown("websearch")),
        ("code_execution", unknown("code_execution")),
        ("mcp", unknown("mcp")),
        ("mcp:", bad_server("mcp:")),
        ("mcp:GitHub", bad_server("mcp:GitHub")),
        ("mcp:git__hub", bad_server("mcp:git__hub")),
        ("mcp:git-hub", bad_server("mcp:git-hub")),
        ("mcp:/tool", bad_server("mcp:/tool")),
        ("mcp:github/", bad_tool("mcp:github/")),
        ("mcp:github/a b", bad_tool("mcp:github/a b")),
        ("mcp:github/a\nb", bad_tool("mcp:github/a\nb")),
        (long_tool.as_str(), bad_tool(&long_tool)),
    ];
    for (name, expected) in refused {
        assert_eq!(SelectedTool::parse(name), Err(expected), "{name:?}");
    }
    let at_limit = format!("mcp:github/{}", "t".repeat(MAX_MCP_TOOL_NAME_BYTES));
    assert!(SelectedTool::parse(&at_limit).is_ok());
    Ok(())
}

#[test]
fn selection_refuses_duplicates_and_covered_tools() -> TestResult {
    let parsed = TemplateToolSelection::parse([
        "mcp:linear/list_issues",
        "web_search",
        "mcp:github",
        "mcp:linear/create_issue",
    ])?;
    assert_eq!(parsed.entries().len(), 4);
    assert_eq!(
        parsed
            .mcp_servers()
            .map(McpServerName::as_str)
            .collect::<Vec<_>>(),
        ["github", "linear"]
    );
    assert!(TemplateToolSelection::parse(Vec::<String>::new())?.is_empty());

    assert_eq!(
        TemplateToolSelection::parse(["web_search", "fetch_url", "web_search"]),
        Err(ToolSelectionError::Duplicate {
            name: "web_search".to_owned()
        })
    );
    assert_eq!(
        TemplateToolSelection::parse(["mcp:github/a", "mcp:github/a"]),
        Err(ToolSelectionError::Duplicate {
            name: "mcp:github/a".to_owned()
        })
    );
    for names in [
        ["mcp:github/create_issue", "mcp:github"],
        ["mcp:github", "mcp:github/create_issue"],
    ] {
        assert_eq!(
            TemplateToolSelection::parse(names),
            Err(ToolSelectionError::CoveredByServer {
                name: "mcp:github/create_issue".to_owned(),
                server: server("github")?,
            })
        );
    }
    Ok(())
}

#[test]
fn selection_error_text_pins() -> TestResult {
    let text = |names: &[&str]| {
        TemplateToolSelection::parse(names)
            .err()
            .map(|error| error.to_string())
    };
    assert_eq!(
        text(&["web-search"]).as_deref(),
        Some(
            "unknown tool \"web-search\"; expected one of web_search, web_fetch, x_search, \
                 fetch_url, image_gen, mcp:<id> or mcp:<id>/<tool>"
        )
    );
    assert_eq!(
        text(&["x_search", "x_search"]).as_deref(),
        Some("tool \"x_search\" is listed twice")
    );
    assert_eq!(
        text(&["mcp:github", "mcp:github/create_issue"]).as_deref(),
        Some(
            "tool \"mcp:github/create_issue\" is already included by mcp:github; \
                 list one or the other"
        )
    );
    assert_eq!(
        text(&["mcp:Git"]).as_deref(),
        Some(
            "tool \"mcp:Git\" names an invalid MCP server id; an id matches \
                 [a-z][a-z0-9]{0,15}(_[a-z0-9]+)* within 32 bytes"
        )
    );
    assert_eq!(
        text(&["mcp:git/"]).as_deref(),
        Some(
            "tool \"mcp:git/\" names an invalid MCP tool; the tool part is 1 to 128 bytes \
                 without whitespace"
        )
    );
    Ok(())
}

#[test]
fn plan_order_is_fixed_and_independent_of_list_and_listing_order() -> TestResult {
    let exposed = |name: &str| ToolName::new(name);
    let listing = vec![
        (server("linear")?, exposed("mcp__linear__list_issues")?),
        (
            server("github")?,
            exposed("mcp__github__get_file_contents")?,
        ),
        (server("github")?, exposed("mcp__github__create_issue")?),
    ];
    let forward = ToolPlan::new(
        &TemplateToolSelection::parse([
            "image_gen",
            "mcp:linear",
            "x_search",
            "fetch_url",
            "mcp:github",
            "web_fetch",
            "web_search",
        ])?,
        listing.clone(),
    );
    let reverse = ToolPlan::new(
        &TemplateToolSelection::parse([
            "web_search",
            "mcp:github",
            "web_fetch",
            "fetch_url",
            "mcp:linear",
            "x_search",
            "image_gen",
        ])?,
        listing.into_iter().rev(),
    );
    assert_eq!(forward.tools(), reverse.tools());
    assert_eq!(
        forward.tools(),
        [
            PlannedTool::Optional(OptionalTool::WebSearch),
            PlannedTool::Optional(OptionalTool::WebFetch),
            PlannedTool::Optional(OptionalTool::XSearch),
            PlannedTool::Optional(OptionalTool::FetchUrl),
            PlannedTool::Optional(OptionalTool::ImageGen),
            PlannedTool::Mcp {
                server: server("github")?,
                exposed: exposed("mcp__github__create_issue")?,
            },
            PlannedTool::Mcp {
                server: server("github")?,
                exposed: exposed("mcp__github__get_file_contents")?,
            },
            PlannedTool::Mcp {
                server: server("linear")?,
                exposed: exposed("mcp__linear__list_issues")?,
            },
        ]
    );
    Ok(())
}

fn route_label(route: ToolRoute) -> &'static str {
    match route {
        ToolRoute::Codex => "codex",
        ToolRoute::Grok => "grok",
        ToolRoute::Claude => "claude",
        ToolRoute::OpenAi => "openai",
        ToolRoute::Anthropic => "anthropic",
        ToolRoute::XaiApi => "xai-api",
        ToolRoute::OpenRouter => "openrouter",
    }
}

fn refusal_text(selection: &[&str], route: ToolRoute) -> TestResult<String> {
    let selection = TemplateToolSelection::parse(selection)?;
    match bind_tool_plan(&selection, route, route_label(route)) {
        Ok(plan) => Err(format!("{route:?} bound {plan:?}").into()),
        Err(refusal) => Ok(refusal.to_string()),
    }
}

/// An option the route's provider has no parameter for refuses the
/// bind with its remedy; every other route takes it.
#[test]
fn retrieval_options_without_a_provider_parameter_refuse_per_route() -> TestResult {
    let selection = TemplateToolSelection::parse(["web_search"])?;
    let blocked = RetrievalOptions {
        blocked_domains: vec!["example.com".to_owned()],
        ..RetrievalOptions::default()
    };
    let counted = RetrievalOptions {
        web_search_max_uses: 2,
        ..RetrievalOptions::default()
    };
    for route in ToolRoute::ALL {
        let label = route_label(route);
        for (options, option, refused) in [
            (
                &blocked,
                "blocked_domains",
                matches!(route, ToolRoute::Codex | ToolRoute::OpenAi),
            ),
            (
                &counted,
                "max_uses",
                matches!(
                    route,
                    ToolRoute::OpenAi | ToolRoute::Grok | ToolRoute::XaiApi
                ),
            ),
        ] {
            let checked = check_retrieval_options(&selection, route, label, options);
            assert_eq!(checked.is_err(), refused, "{route:?} {option}");
            if let Err(refusal) = checked {
                assert!(
                    refusal.to_string().starts_with(&format!(
                        "[tools.web_search] {option} is not available on {label}; remedy: "
                    )),
                    "{refusal}"
                );
            }
        }
        let other = TemplateToolSelection::parse(["fetch_url"])?;
        assert!(check_retrieval_options(&other, route, label, &blocked).is_ok());
    }
    Ok(())
}

#[test]
fn hosted_retrieval_refusal_text_per_route() -> TestResult {
    let web_fetch_elsewhere = |route: &str| {
        format!(
            "web_fetch is not available on {route}; \
                 remedy: use fetch_url (Plak-run) or a claude/anthropic template"
        )
    };
    let x_search_elsewhere = |route: &str| {
        format!(
            "x_search is not available on {route}; \
                 remedy: put x_search on a grok or xai-api template and spawn it"
        )
    };
    let expected = [
        (ToolRoute::Codex, "web_fetch", web_fetch_elsewhere("codex")),
        (ToolRoute::Codex, "x_search", x_search_elsewhere("codex")),
        (ToolRoute::Grok, "web_fetch", web_fetch_elsewhere("grok")),
        (ToolRoute::Claude, "x_search", x_search_elsewhere("claude")),
        (
            ToolRoute::OpenAi,
            "web_fetch",
            web_fetch_elsewhere("openai"),
        ),
        (ToolRoute::OpenAi, "x_search", x_search_elsewhere("openai")),
        (
            ToolRoute::Anthropic,
            "x_search",
            x_search_elsewhere("anthropic"),
        ),
        (
            ToolRoute::XaiApi,
            "web_fetch",
            web_fetch_elsewhere("xai-api"),
        ),
        (
            ToolRoute::OpenRouter,
            "web_fetch",
            web_fetch_elsewhere("openrouter"),
        ),
        (
            ToolRoute::OpenRouter,
            "x_search",
            x_search_elsewhere("openrouter"),
        ),
    ];
    for (route, tool, text) in expected {
        assert_eq!(refusal_text(&[tool], route)?, text, "{route:?} {tool}");
    }
    Ok(())
}

#[test]
fn image_gen_binds_where_qualified_and_names_the_route_elsewhere() -> TestResult {
    for route in ToolRoute::ALL {
        let label = route_label(route);
        let expected = match route {
            ToolRoute::Codex | ToolRoute::Grok | ToolRoute::OpenAi | ToolRoute::XaiApi => {
                let selection = TemplateToolSelection::parse(["image_gen"])?;
                let plan = bind_tool_plan(&selection, route, label)
                    .map_err(|refusal| refusal.to_string())?;
                assert!(plan.image_gen().is_some(), "{route:?}");
                continue;
            }
            ToolRoute::Claude | ToolRoute::Anthropic | ToolRoute::OpenRouter => format!(
                "image_gen is not available on {label}; remedy: put image_gen on a codex, \
                     grok, openai or xai-api template and spawn it"
            ),
        };
        assert_eq!(refusal_text(&["image_gen"], route)?, expected, "{route:?}");
    }
    let qualified = HostedRetrievalTable::new(
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
    );
    assert_eq!(
        refusal(&SelectedTool::parse("web_search")?, qualified, "grok").to_string(),
        "web_search is not available in this build; remedy: remove web_search from \
             the template's tools"
    );
    Ok(())
}

/// A qualified row binds `image_gen` on a deferred slot where the route
/// has an image port; a route without one refuses it as not built.
#[test]
fn a_qualified_image_gen_row_binds_on_the_routes_with_an_image_port() -> TestResult {
    let selection = TemplateToolSelection::parse(["image_gen", "fetch_url"])?;
    let unsupported = HostedRetrievalTable::new(
        HostedRetrievalSupport::Unsupported {
            reason: HostedRetrievalUnsupported::NotQualified,
        },
        HostedRetrievalSupport::Unsupported {
            reason: HostedRetrievalUnsupported::NotQualified,
        },
        HostedRetrievalSupport::Unsupported {
            reason: HostedRetrievalUnsupported::NotQualified,
        },
    );
    for route in ToolRoute::ALL {
        let label = route_label(route);
        let bound = bind_tool_plan_on(
            &selection,
            route,
            unsupported,
            ImageGenRow::Supported,
            label,
        );
        if !route.has_image_port() {
            assert_eq!(
                bound.err().map(|refusal| refusal.to_string()),
                Some(
                    "image_gen is not available in this build; remedy: remove image_gen \
                         from the template's tools"
                        .to_owned()
                ),
                "{route:?}"
            );
            continue;
        }
        let plan = bound.map_err(|refusal| refusal.to_string())?;
        assert_eq!(
            plan.tools(),
            [
                PlannedTool::Optional(OptionalTool::FetchUrl),
                PlannedTool::Optional(OptionalTool::ImageGen)
            ]
        );
        let binding = plan.image_gen().ok_or("no image binding")?;
        let slot = binding.slot().ok_or("no deferred slot")?;
        let extensions = crate::tools::ProductToolExtensions::empty()
            .with_plan(&plan)
            .map_err(|error| error.to_string())?;
        assert_eq!(
            extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
            ["fetch_url", "image_gen"],
            "{route:?}"
        );
        assert_eq!(slot.max_edit_images(), 0);
        extensions
            .connect_image_gen(Some(Arc::new(FixedImagePort)))
            .map_err(|error| error.to_string())?;
        assert_eq!(slot.max_edit_images(), 3);
        assert!(
            extensions
                .connect_image_gen(Some(Arc::new(FixedImagePort)))
                .is_err()
        );
    }
    assert!(
        crate::tools::ProductToolExtensions::empty()
            .connect_image_gen(None)
            .is_ok()
    );
    Ok(())
}

struct FixedImagePort;

impl ImageGenPort for FixedImagePort {
    fn max_edit_images(&self) -> usize {
        3
    }

    fn takes_size(&self) -> bool {
        true
    }

    fn generate(&self, _request: ImageGenRequest, _control: OperationContext) -> ImageGenFuture {
        Box::pin(async { Ok(Vec::new()) })
    }
}

#[test]
fn fetch_url_binds_on_every_route_and_registers_after_the_wrappers() -> TestResult {
    let selection = TemplateToolSelection::parse(["fetch_url"])?;
    for route in ToolRoute::ALL {
        let plan = bind_tool_plan(&selection, route, route_label(route))
            .map_err(|refusal| refusal.to_string())?;
        assert_eq!(
            plan.tools(),
            [PlannedTool::Optional(OptionalTool::FetchUrl)],
            "{route:?}"
        );
        assert!(plan.retrieval().is_none(), "{route:?}");
        assert_eq!(plan.fetch_url_limits(), FetchUrlLimits::default());
        assert_eq!(
            plan.tool_result_images(),
            !matches!(route, ToolRoute::Grok)
                || crate::capabilities::supergrok_tool_result_images()
        );
        let extensions = crate::tools::ProductToolExtensions::empty()
            .with_plan(&plan)
            .map_err(|error| error.to_string())?;
        assert_eq!(
            extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
            ["fetch_url"],
            "{route:?}"
        );
    }
    let limits = FetchUrlLimits::new(1024, 5).ok_or("limits")?;
    let plan = bind_tool_plan_with(
        &TemplateToolSelection::parse(["fetch_url", "web_search"])?,
        HostedRetrievalTable::new(
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
            HostedRetrievalSupport::Supported,
        ),
        CodingPresentation::Claude,
        "claude",
        Some(search_and_fetch_port()),
        NO_IMAGE_GEN,
    )?
    .with_fetch_url_limits(limits);
    assert_eq!(plan.fetch_url_limits(), limits);
    let extensions = crate::tools::ProductToolExtensions::empty()
        .with_plan(&plan)
        .map_err(|error| error.to_string())?;
    assert_eq!(
        extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
        ["WebSearch", "fetch_url"]
    );
    Ok(())
}

#[test]
#[cfg(feature = "web-fetch")]
fn tool_image_switch_narrows_only_the_subscription_route_and_rotates_binding() -> TestResult {
    use plakous::tools::ToolRegistryBuilder;

    for route in ToolRoute::ALL {
        let decisions = [
            tool_result_images(route, false),
            tool_result_images(route, true),
        ];
        assert_eq!(
            decisions,
            if route == ToolRoute::Grok {
                [false, true]
            } else {
                [true, true]
            }
        );
        let mut identities = Vec::new();
        for (switch, allowed) in [false, true].into_iter().zip(decisions) {
            let selection = TemplateToolSelection::parse(["fetch_url"])?;
            let plan = ToolPlan::new(&selection, []).with_tool_image_policy(route, switch);
            assert_eq!(plan.tool_result_images(), allowed);
            let extensions = crate::tools::ProductToolExtensions::empty()
                .with_plan(&plan)
                .map_err(|error| error.to_string())?;
            assert_eq!(extensions.tool_result_images(), allowed);
            let mut builder = ToolRegistryBuilder::default();
            for registration in extensions.registrations() {
                builder.register(registration.clone())?;
            }
            identities.push(
                builder
                    .freeze()
                    .identity(&ToolName::new("fetch_url")?)
                    .ok_or("missing fetch")?
                    .executor_binding_identity(),
            );
            let empty = ToolPlan::new(&TemplateToolSelection::default(), [])
                .with_tool_image_policy(route, switch);
            assert_eq!(
                crate::tools::ProductToolExtensions::empty()
                    .with_plan(&empty)
                    .map_err(|error| error.to_string())?
                    .tool_result_images(),
                allowed
            );
        }
        assert_eq!(identities[0] != identities[1], route == ToolRoute::Grok);
    }
    #[cfg(any(feature = "grok", feature = "xai-api"))]
    assert_ne!(
        plakous::xai::supergrok_tool_image_adapter_revision(false),
        plakous::xai::supergrok_tool_image_adapter_revision(true)
    );
    Ok(())
}

struct FakePort(Vec<RetrievalKind>);

impl RetrievalPort for FakePort {
    fn kinds(&self) -> &[RetrievalKind] {
        &self.0
    }

    fn retrieve(&self, _: RetrievalRequest, _: OperationContext) -> RetrievalFuture {
        Box::pin(async { Ok(plakous::tools::web::RetrievalOutcome::default()) })
    }
}

fn search_and_fetch_port() -> Arc<dyn RetrievalPort> {
    Arc::new(FakePort(vec![
        RetrievalKind::WebSearch,
        RetrievalKind::PageFetch,
    ]))
}

#[test]
fn a_qualified_row_with_a_port_binds_the_wrapper_pack() -> TestResult {
    let qualified = HostedRetrievalTable::new(
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Unsupported {
            reason: HostedRetrievalUnsupported::NotOffered,
        },
    );
    let selection = TemplateToolSelection::parse(["web_fetch", "web_search"])?;
    let port = search_and_fetch_port();
    let plan = bind_tool_plan_with(
        &selection,
        qualified,
        CodingPresentation::Claude,
        "claude",
        Some(Arc::clone(&port)),
        NO_IMAGE_GEN,
    )?;
    assert_eq!(
        plan.tools(),
        [
            PlannedTool::Optional(OptionalTool::WebSearch),
            PlannedTool::Optional(OptionalTool::WebFetch)
        ]
    );
    let extensions = crate::tools::ProductToolExtensions::empty()
        .with_plan(&plan)
        .map_err(|error| error.to_string())?;
    assert_eq!(
        extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
        ["WebSearch", "WebFetch"]
    );

    // The same port on an unqualified row, or a qualified row without a
    // port, refuses with the route text.
    let unqualified = bind_tool_plan_with(
        &TemplateToolSelection::parse(["x_search"])?,
        qualified,
        CodingPresentation::Grok,
        "grok",
        Some(port),
        NO_IMAGE_GEN,
    )
    .err()
    .ok_or("an unoffered tool bound")?;
    assert_eq!(
        unqualified.to_string(),
        "x_search is not available on grok; remedy: put x_search on a grok or xai-api \
             template and spawn it"
    );
    let portless = bind_tool_plan_with(
        &selection,
        qualified,
        CodingPresentation::Claude,
        "claude",
        None,
        NO_IMAGE_GEN,
    )
    .err()
    .ok_or("a portless tool bound")?;
    assert_eq!(
        portless.to_string(),
        "web_fetch is not available in this build; remedy: remove web_fetch from the \
             template's tools"
    );
    Ok(())
}

/// Records every request and answers each with its own query or URL.
#[derive(Default)]
struct RecordingPort(std::sync::Mutex<Vec<RetrievalRequest>>);

impl RetrievalPort for RecordingPort {
    fn kinds(&self) -> &[RetrievalKind] {
        &[RetrievalKind::WebSearch, RetrievalKind::PageFetch]
    }

    fn retrieve(&self, request: RetrievalRequest, _: OperationContext) -> RetrievalFuture {
        let text = match &request {
            RetrievalRequest::WebSearch { query, .. } => query.clone(),
            RetrievalRequest::PageFetch { url, .. } => url.clone(),
            RetrievalRequest::PostSearch { .. } => String::new(),
        };
        if let Ok(mut requests) = self.0.lock() {
            requests.push(request);
        }
        Box::pin(async move {
            Ok(plakous::tools::web::RetrievalOutcome {
                text,
                ..plakous::tools::web::RetrievalOutcome::default()
            })
        })
    }
}

fn control() -> OperationContext {
    OperationContext::new(plakous::model::CancellationToken::new())
}

#[tokio::test]
async fn a_qualified_anthropic_or_claude_row_binds_a_deferred_port_the_executor_connects()
-> TestResult {
    let qualified = HostedRetrievalTable::new(
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Unsupported {
            reason: HostedRetrievalUnsupported::NotOffered,
        },
    );
    let selection = TemplateToolSelection::parse(["web_search", "web_fetch"])?;
    for (route, label) in [
        (ToolRoute::Anthropic, "anthropic"),
        (ToolRoute::Claude, "claude"),
    ] {
        let plan = bind_tool_plan_on(&selection, route, qualified, ImageGenRow::NotOffered, label)?;
        let extensions = crate::tools::ProductToolExtensions::empty()
            .with_plan(&plan)
            .map_err(|error| error.to_string())?;
        assert_eq!(
            extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
            ["WebSearch", "WebFetch"],
            "{label}"
        );
        let port = plan.retrieval().ok_or("no binding")?.port();
        let early = port
            .retrieve(
                RetrievalRequest::PageFetch {
                    url: "https://example.com/".to_owned(),
                    prompt: None,
                },
                control(),
            )
            .await
            .err()
            .ok_or("an unconnected port answered")?;
        assert_eq!(early.kind(), RetrievalErrorKind::Unavailable);

        let executor = Arc::new(RecordingPort::default());
        extensions
            .connect_retrieval(Some(Arc::clone(&executor) as Arc<dyn RetrievalPort>))
            .map_err(|error| error.to_string())?;
        let answered = port
            .retrieve(
                RetrievalRequest::WebSearch {
                    query: "rust".to_owned(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    max_uses: 5,
                },
                control(),
            )
            .await?;
        assert_eq!(answered.text, "rust");
        assert_eq!(executor.0.lock().map_err(|_| "poisoned")?.len(), 1);
        assert!(
            extensions
                .connect_retrieval(Some(executor as Arc<dyn RetrievalPort>))
                .is_err(),
            "a slot connects once"
        );
    }
    // A route whose port does not serve a kind refuses a qualified row
    // as not built in.
    let refused = bind_tool_plan_on(
        &selection,
        ToolRoute::Codex,
        qualified,
        ImageGenRow::NotOffered,
        "codex",
    )
    .err()
    .ok_or("an unserved kind bound")?;
    assert!(refused.to_string().contains("not available in this build"));
    Ok(())
}

#[tokio::test]
async fn a_qualified_openrouter_row_binds_web_search_only() -> TestResult {
    let qualified = HostedRetrievalTable::new(
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
    );
    let selection = TemplateToolSelection::parse(["web_search"])?;
    let plan = bind_tool_plan_on(
        &selection,
        ToolRoute::OpenRouter,
        qualified,
        ImageGenRow::NotOffered,
        "openrouter",
    )?;
    let extensions = crate::tools::ProductToolExtensions::empty()
        .with_plan(&plan)
        .map_err(|error| error.to_string())?;
    assert_eq!(
        extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
        ["web_search"]
    );
    let executor = Arc::new(RecordingPort::default());
    extensions
        .connect_retrieval(Some(Arc::clone(&executor) as Arc<dyn RetrievalPort>))
        .map_err(|error| error.to_string())?;
    let answered = plan
        .retrieval()
        .ok_or("no binding")?
        .port()
        .retrieve(
            RetrievalRequest::WebSearch {
                query: "rust".to_owned(),
                allowed_domains: Vec::new(),
                blocked_domains: Vec::new(),
                max_uses: 5,
            },
            control(),
        )
        .await?;
    assert_eq!(answered.text, "rust");
    for unserved in [["web_fetch"], ["x_search"]] {
        let refused = bind_tool_plan_on(
            &TemplateToolSelection::parse(unserved)?,
            ToolRoute::OpenRouter,
            qualified,
            ImageGenRow::NotOffered,
            "openrouter",
        );
        assert!(refused.is_err(), "{unserved:?} bound on openrouter");
    }
    Ok(())
}

#[tokio::test]
async fn qualified_responses_rows_bind_deferred_ports_the_executors_connect() -> TestResult {
    let supported = HostedRetrievalSupport::Supported;
    let qualified = HostedRetrievalTable::new(supported, supported, supported);
    for (route, label, tools, names, kinds) in [
        (
            ToolRoute::OpenAi,
            "openai",
            &["web_search"][..],
            &["web_search"][..],
            vec![RetrievalKind::WebSearch],
        ),
        (
            ToolRoute::XaiApi,
            "xai-api",
            &["web_search", "x_search"][..],
            &["web_search", "x_search"][..],
            vec![RetrievalKind::WebSearch, RetrievalKind::PostSearch],
        ),
        (
            ToolRoute::Codex,
            "codex",
            &["web_search"][..],
            &["web_search"][..],
            vec![RetrievalKind::WebSearch],
        ),
        (
            ToolRoute::Grok,
            "grok",
            &["web_search", "x_search"][..],
            &["web_search", "x_search"][..],
            vec![RetrievalKind::WebSearch, RetrievalKind::PostSearch],
        ),
    ] {
        let selection = TemplateToolSelection::parse(tools.iter().copied())?;
        let plan = bind_tool_plan_on(&selection, route, qualified, ImageGenRow::NotOffered, label)?;
        let extensions = crate::tools::ProductToolExtensions::empty()
            .with_plan(&plan)
            .map_err(|error| error.to_string())?;
        assert_eq!(
            extensions.names().map(ToolName::as_str).collect::<Vec<_>>(),
            names,
            "{label}"
        );
        extensions
            .connect_retrieval(Some(Arc::new(FakePort(kinds)) as Arc<dyn RetrievalPort>))
            .map_err(|error| error.to_string())?;
        let answered = plan
            .retrieval()
            .ok_or("no binding")?
            .port()
            .retrieve(
                RetrievalRequest::WebSearch {
                    query: "rust".to_owned(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    max_uses: 5,
                },
                control(),
            )
            .await;
        assert!(answered.is_ok(), "{label}");
        // The shipped tables qualify web search on every route.
        assert!(bind_tool_plan(&selection, route, label).is_ok(), "{label}");
    }
    // The OpenAI API and Codex ports serve no X search.
    let x_search = TemplateToolSelection::parse(["x_search"])?;
    assert!(
        bind_tool_plan_on(
            &x_search,
            ToolRoute::OpenAi,
            qualified,
            ImageGenRow::NotOffered,
            "openai"
        )
        .is_err()
    );
    assert!(
        bind_tool_plan_on(
            &x_search,
            ToolRoute::Codex,
            qualified,
            ImageGenRow::NotOffered,
            "codex"
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn a_connected_executor_must_serve_every_bound_kind() {
    let slot = DeferredRetrievalPort::new(&[RetrievalKind::WebSearch, RetrievalKind::PageFetch]);
    assert!(
        slot.connect(Arc::new(FakePort(vec![RetrievalKind::WebSearch])))
            .is_err()
    );
}

#[test]
fn a_port_that_does_not_serve_the_kind_refuses() -> TestResult {
    let qualified = HostedRetrievalTable::new(
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
    );
    let refused = bind_tool_plan_with(
        &TemplateToolSelection::parse(["x_search"])?,
        qualified,
        CodingPresentation::Grok,
        "xai-api",
        Some(search_and_fetch_port()),
        NO_IMAGE_GEN,
    );
    assert!(refused.is_err());
    Ok(())
}

#[test]
fn binding_refuses_the_first_optional_entry_and_passes_mcp_entries() -> TestResult {
    let refused = bind_tool_plan(
        &TemplateToolSelection::parse(["mcp:github", "fetch_url", "image_gen", "web_search"])?,
        ToolRoute::Claude,
        "claude",
    )
    .err()
    .ok_or("an optional tool bound")?;
    assert_eq!(
        refused.tool(),
        &SelectedTool::Optional(OptionalTool::ImageGen)
    );
    let mcp_only = TemplateToolSelection::parse(["mcp:github", "mcp:linear/list_issues"])?;
    for route in ToolRoute::ALL {
        for selection in [&TemplateToolSelection::default(), &mcp_only] {
            let plan = bind_tool_plan(selection, route, "any")?;
            assert!(plan.tools().is_empty(), "{route:?}");
            assert!(plan.retrieval().is_none());
        }
    }
    Ok(())
}

/// Hosted-retrieval wrappers and MCP tools share one plan: the wrappers
/// keep their port and come first, the MCP tools follow by server and
/// exposed name.
#[test]
fn wrappers_and_mcp_tools_share_one_plan() -> TestResult {
    let qualified = HostedRetrievalTable::new(
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
        HostedRetrievalSupport::Supported,
    );
    let selection = TemplateToolSelection::parse(["mcp:fake", "web_search"])?;
    let plan = bind_tool_plan_with(
        &selection,
        qualified,
        CodingPresentation::Claude,
        "claude",
        Some(search_and_fetch_port()),
        NO_IMAGE_GEN,
    )?
    .with_mcp(
        &selection,
        crate::mcp_binding::BoundMcpTools {
            names: vec![(server("fake")?, ToolName::new("mcp__fake__echo")?)],
            registrations: Vec::new(),
            notices: vec!["omitted".to_owned()],
            records: Vec::new(),
            placements: BTreeMap::new(),
        },
    );
    assert!(plan.retrieval().is_some());
    assert_eq!(
        plan.tools(),
        [
            PlannedTool::Optional(OptionalTool::WebSearch),
            PlannedTool::Mcp {
                server: server("fake")?,
                exposed: ToolName::new("mcp__fake__echo")?,
            },
        ]
    );
    // A planned MCP tool without its registration refuses the append.
    assert!(
        crate::tools::ProductToolExtensions::empty()
            .with_plan(&plan)
            .is_err()
    );
    Ok(())
}

#[test]
fn routes_map_from_every_native_provider_and_api_target() {
    use crate::binding::ResolvedProtocol;
    assert_eq!(
        ToolRoute::native(NativeProvider::Codex),
        Some(ToolRoute::Codex)
    );
    assert_eq!(
        ToolRoute::native(NativeProvider::Xai),
        Some(ToolRoute::Grok)
    );
    assert_eq!(
        ToolRoute::native(NativeProvider::Claude),
        Some(ToolRoute::Claude)
    );
    assert_eq!(
        ToolRoute::api(&ApiTarget::OpenRouter {
            wire: ResolvedProtocol::Chat,
            account: "openrouter".parse().unwrap(),
        }),
        ToolRoute::OpenRouter
    );
    for (provider, route) in [
        (PlatformProvider::OpenAi, ToolRoute::OpenAi),
        (PlatformProvider::Anthropic, ToolRoute::Anthropic),
        (PlatformProvider::Xai, ToolRoute::XaiApi),
    ] {
        assert_eq!(
            ToolRoute::api(&ApiTarget::Platform {
                provider,
                account: (provider).account().parse().unwrap(),
            }),
            route
        );
    }
}
