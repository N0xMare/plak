use super::test_support::{TestResult, extensions, registration, surface};
use super::*;

fn cli<T>(result: CliResult<T>) -> TestResult<T> {
    result.map_err(|error| std::io::Error::other(error.message()).into())
}

#[test]
fn catalog_preserves_core_then_extension_declaration_order() -> TestResult {
    // One membership: the compiled surface. Core in the presentation's own
    // order, then every extension in declaration order.
    assert_eq!(
        surface(
            CodingPresentation::Codex,
            &["custom_second", "custom_first"]
        )
        .exposed_names()
        .iter()
        .map(ToolName::as_str)
        .collect::<Vec<_>>(),
        [
            "apply_patch",
            "exec_command",
            "update_plan",
            "write_stdin",
            "custom_second",
            "custom_first",
        ]
    );
    Ok(())
}

#[test]
fn duplicate_extensions_core_collisions_and_direct_only_fail_closed() -> TestResult {
    assert!(
        ProductToolExtensions::new([
            registration("duplicate", true)?,
            registration("duplicate", true)?,
        ])
        .is_err()
    );
    assert!(ProductToolExtensions::new([registration("direct_only", false)?]).is_err());

    // An extension that collides with a canonical coding name fails closed
    // at the registry, before any menu is compiled.
    let collision = cli(ProductToolExtensions::new([registration(
        "apply_patch",
        true,
    )?]))?;
    assert!(
        crate::agent::coding_tool_registry_with_extensions(
            "collision",
            crate::binding::CompositionSelection::Direct,
            false,
            &collision,
        )
        .is_err()
    );

    // And a menu-level collision — an extension name a pack already
    // presents — fails closed in `compile_over`.
    let extensions = extensions(&["custom_only"]);
    let registry = crate::agent::coding_tool_registry_with_extensions(
        "menu-collision",
        crate::binding::CompositionSelection::Direct,
        false,
        &extensions,
    )
    .expect("registry");
    let doubled = cli(ProductToolExtensions::new([
        registration("custom_only", true)?,
        registration("custom_only", true)?,
    ]));
    assert!(doubled.is_err());
    assert!(
        product_surface(
            plakous::tools::JobConfig::default(),
            CodingPresentation::Codex,
            registry,
            crate::agent::rendered_prompts::suite::synthetic_target(b"menu-collision"),
            &extensions,
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn an_empty_plan_leaves_extensions_registry_and_surface_unchanged() -> TestResult {
    use crate::tool_plan::{TemplateToolSelection, ToolPlan};

    let compile = |extensions: &ProductToolExtensions, presentation| {
        let registry = cli(crate::agent::coding_tool_registry_with_extensions(
            "empty-plan",
            crate::binding::CompositionSelection::Direct,
            false,
            extensions,
        ))?;
        cli(product_surface(
            plakous::tools::JobConfig::default(),
            presentation,
            registry,
            crate::agent::rendered_prompts::suite::synthetic_target(b"empty-plan"),
            extensions,
        ))
    };
    for presentation in [
        CodingPresentation::Codex,
        CodingPresentation::Claude,
        CodingPresentation::Grok,
        CodingPresentation::Pi,
    ] {
        for names in [&[][..], &["submit_result", "custom_tool"][..]] {
            let base = extensions(names);
            let planned = cli(base.clone().with_plan(&ToolPlan::default()))?;
            assert_eq!(
                planned.names().collect::<Vec<_>>(),
                base.names().collect::<Vec<_>>()
            );
            let (before, after) = (
                compile(&base, presentation)?,
                compile(&planned, presentation)?,
            );
            assert_eq!(after.exposed_names(), before.exposed_names());
            assert_eq!(
                after.projection().registry_digest(),
                before.projection().registry_digest()
            );
            assert_eq!(
                after.registry().definitions(),
                before.registry().definitions()
            );
        }
    }
    let selection = TemplateToolSelection::parse(["web_search"])?;
    assert!(
        extensions(&[])
            .with_plan(&ToolPlan::new(&selection, []))
            .is_err()
    );
    let unbound = ToolPlan::new(
        &TemplateToolSelection::parse(["mcp:fake"])?,
        [(
            plakous::tools::McpServerName::new("fake")?,
            ToolName::new("mcp__fake__echo")?,
        )],
    );
    assert!(extensions(&[]).with_plan(&unbound).is_err());
    Ok(())
}

#[test]
fn one_declaration_registers_the_exact_selected_names() -> TestResult {
    let extensions = cli(ProductToolExtensions::new([registration(
        "custom_tool",
        true,
    )?]))?;
    let mut builder = ToolRegistryBuilder::default();
    cli(extensions.register_into(&mut builder))?;
    let registry = builder.freeze();
    let names = surface(CodingPresentation::Grok, &["custom_tool"])
        .exposed_names()
        .to_vec();
    let selected = names.last().ok_or("extension name is missing")?;
    assert_eq!(selected.as_str(), "custom_tool");
    assert!(registry.definition(selected).is_some());
    assert!(
        registry
            .identity(selected)
            .is_some_and(|identity| identity.nested_eligibility()
                == extensions.registrations[0].nested_eligibility())
    );
    Ok(())
}

#[test]
fn occupancy_catalog_omits_file_writes_but_keeps_process_plan_and_tree() -> TestResult {
    for presentation in [
        CodingPresentation::Codex,
        CodingPresentation::Claude,
        CodingPresentation::Grok,
        CodingPresentation::Pi,
    ] {
        let extensions = extensions(&["submit_result", "adopt_candidate"]);
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "occupancy-catalog",
            crate::binding::CompositionSelection::Direct,
            false,
            &extensions,
        )
        .expect("registry");
        for fence in [
            crate::agent::SessionToolFence::OccupancyReadOnly,
            crate::agent::SessionToolFence::OccupancyExecute,
        ] {
            let surface = cli(product_surface_with_fence(
                plakous::tools::JobConfig::default(),
                presentation,
                registry.clone(),
                crate::agent::rendered_prompts::suite::synthetic_target(b"occupancy-catalog"),
                &extensions,
                fence,
                None,
            ))?;
            let names = surface
                .exposed_names()
                .iter()
                .map(ToolName::as_str)
                .collect::<Vec<_>>();
            for name in [
                "apply_patch",
                "Edit",
                "Write",
                "write_file",
                "write",
                "edit",
                "search_replace",
            ] {
                assert!(!names.contains(&name), "{presentation:?}: {name}");
            }
            for name in ["submit_result", "adopt_candidate"] {
                assert!(names.contains(&name));
            }
            // Claude (five-tool pack) and pi present no plan tool.
            assert_eq!(
                names
                    .iter()
                    .any(|name| matches!(*name, "update_plan" | "todo_write" | "merge_plan")),
                !matches!(
                    presentation,
                    CodingPresentation::Claude | CodingPresentation::Pi
                ),
                "{presentation:?}: {names:?}"
            );
            for name in ["update_plan", "merge_plan"] {
                let name = ToolName::new(name)?;
                assert_eq!(
                    registry
                        .definition(&name)
                        .ok_or("plan definition")?
                        .effect(),
                    plakous::model::ToolEffect::Mutating
                );
            }
            assert!(names.iter().any(|name| matches!(
                *name,
                "exec_command" | "Bash" | "run_terminal_command" | "bash"
            )));
            for entry in surface.projection().entries() {
                for identity in entry.canonical_operations().values() {
                    let definition = registry.definition(identity.name()).ok_or("definition")?;
                    assert!(crate::agent::occupancy_read_only_allows(
                        definition.name().as_str(),
                        definition.effect()
                    ));
                }
            }
        }
    }
    Ok(())
}
