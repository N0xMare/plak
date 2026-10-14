//! What the composed pack list registers.
//!
//! One pack knows what it registers; only the composition knows what the four
//! of them register together, so the count lives here.

#![allow(missing_docs, clippy::expect_used, reason = "integration test crate")]

use plakous_compose::standard_coding_packs_in_memory;
use std::error::Error;
use std::str::FromStr;

use plakous_model::{AttemptId, CallId, RunId, ToolCorrelation, ToolEffect, ToolName, TurnId};
use plakous_tools::{
    CODEX_PATCH_ENVELOPE_CANONICAL_NAME, PROCESS_SESSION_CAPABILITY_NAME,
    PROCESS_SESSION_POLL_NAME, PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_WRITE_NAME,
    ToolInvocationRequest, ToolPack, ToolRegistryBuilder, ToolRegistryLimits, ToolRuntimeError,
    register_packs,
};

#[test]
fn projected_process_pack_adds_four_closed_operations() -> Result<(), ToolRuntimeError> {
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let registry = builder.freeze();
    let names = registry
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 11);
    assert!(names.contains(&PROCESS_SESSION_CAPABILITY_NAME));
    assert!(names.contains(&PROCESS_SESSION_WRITE_NAME));
    assert!(names.contains(&PROCESS_SESSION_POLL_NAME));
    assert!(names.contains(&PROCESS_SESSION_TERMINATE_NAME));
    assert!(names.contains(&CODEX_PATCH_ENVELOPE_CANONICAL_NAME));
    Ok(())
}

#[test]
fn standard_manifest_is_stable_and_complete() -> Result<(), ToolRuntimeError> {
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let frozen = builder.freeze();
    let mut names = frozen
        .definitions()
        .iter()
        .map(|definition| definition.name().as_str().to_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        vec![
            "apply_patch",
            "inspect_image",
            "merge_plan",
            "patch_envelope",
            "process_session_poll",
            "process_session_start",
            "process_session_terminate",
            "process_session_write",
            "read_file",
            "search_repository",
            "update_plan",
        ]
    );
    Ok(())
}

#[test]
fn read_only_standard_manifest_is_exact_and_executable() -> Result<(), Box<dyn Error>> {
    // The read-only half of the coding surface is the two stateless packs'
    // read-only registrations; nothing outside a pack selects it.
    let packs = standard_coding_packs_in_memory()?;
    let mut builder = ToolRegistryBuilder::new(ToolRegistryLimits::default());
    for pack in &packs {
        for registration in ToolPack::registrations(pack.as_ref())? {
            if registration.definition().effect() == ToolEffect::ReadOnly {
                builder.register(registration)?;
            }
        }
    }
    let frozen = builder.freeze();
    let definitions = frozen.definitions();

    assert_eq!(frozen.len(), 4);
    let mut names = definitions
        .iter()
        .map(|definition| definition.name().as_str().to_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        vec![
            "inspect_image",
            "process_session_poll",
            "read_file",
            "search_repository",
        ]
    );
    assert!(
        definitions
            .iter()
            .all(|definition| definition.effect() == ToolEffect::ReadOnly)
    );

    for definition in definitions {
        assert_eq!(frozen.definition(definition.name()), Some(definition));
        assert!(
            frozen
                .identity(definition.name())
                .is_some_and(|identity| identity.name() == definition.name())
        );
    }

    for excluded_name in ["apply_patch", "update_plan"] {
        assert!(frozen.definition(&ToolName::new(excluded_name)?).is_none());
        assert!(frozen.identity(&ToolName::new(excluded_name)?).is_none());
    }

    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000071")?;
    let turn_id = TurnId::from_str("00000000-0000-0000-0000-000000000072")?;
    let attempt_id = AttemptId::from_str("00000000-0000-0000-0000-000000000073")?;
    let call_id = CallId::from_str("00000000-0000-0000-0000-000000000074")?;
    for (name, arguments) in [
        ("read_file", r#"{"path":"README.md"}"#),
        ("search_repository", r#"{"query":"needle","path":"."}"#),
        ("inspect_image", r#"{"path":"image.png"}"#),
    ] {
        let name = ToolName::new(name)?;
        let invocation = frozen.prepare(ToolInvocationRequest::new(
            run_id,
            turn_id,
            attempt_id,
            call_id,
            name.clone(),
            ToolCorrelation::default(),
            arguments,
        ))?;
        assert_eq!(invocation.identity().name(), &name);
    }
    assert!(
        frozen
            .prepare(ToolInvocationRequest::new(
                run_id,
                turn_id,
                attempt_id,
                call_id,
                ToolName::new("search_repository")?,
                ToolCorrelation::default(),
                r#"{"path":"."}"#,
            ))
            .is_err(),
        "search_repository requires query"
    );
    Ok(())
}
