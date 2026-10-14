//! One resolved tool namespace across packs, runtime markers and
//! orchestration: a clash between different slots takes the declared alternate
//! spelling, anything else fails construction naming both providers, and
//! nothing is shadowed.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test crate"
)]

use std::sync::Arc;

use plakous_compose::standard_coding_packs_in_memory;
use plakous_model::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    CodingPresentation, CompiledSurface, CompositionRuntime, MenuBinding, MenuEntry, MenuShape,
    PROCESS_TOOL_PROJECTION_REVISION, PromptPin, ProviderModelProjectionIdentityV2,
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId,
    ToolFuture, ToolInvocation, ToolOutcome, ToolPack, ToolProjectionLoweringV2, ToolRegistration,
    ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind, compile, menu_names,
};
use serde_json::json;

type TestError = Box<dyn std::error::Error>;

#[derive(Debug)]
struct Idle;

impl ToolExecutor for Idle {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("idle") })
    }
}

/// A pack that registers one canonical capability and presents it under one
/// exposed name, optionally declaring an alternate spelling.
struct OneToolPack {
    id: &'static str,
    canonical: &'static str,
    exposed: &'static str,
    alternate: Option<&'static str>,
}

impl ToolPack for OneToolPack {
    fn id(&self) -> &'static str {
        self.id
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        let definition = ToolDefinition::new(
            ToolName::new(self.canonical).expect("canonical name"),
            "test.namespace.v1",
            format!("Test capability {}.", self.canonical),
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))
            .expect("schema"),
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )
        .expect("definition");
        let executor_id = ToolExecutorId::new(format!("test.{}.executor.v1", self.canonical))?;
        Ok(vec![
            ToolRegistration::new(
                RegisteredToolId::new(format!("test.{}.v1", self.canonical))?,
                executor_id.clone(),
                ToolExecutorBindingIdentity::new(1, &[executor_id.as_str().as_bytes()])?,
                definition,
                Arc::new(Idle),
            )
            .nested_eligible(),
        ])
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let entry = MenuEntry::new(
            self.exposed,
            vec![MenuBinding::by_effect(
                ToolName::new(self.canonical).expect("canonical name"),
            )],
            MenuShape::Canonical,
            ToolProjectionLoweringV2::CanonicalPassthrough,
        );
        Ok(vec![match self.alternate {
            Some(alternate) => entry.with_alternate(alternate),
            None => entry,
        }])
    }

    fn pins(&self) -> &'static [PromptPin] {
        &[]
    }
}

/// An orchestration-style tool presented under its canonical name `wait`.
fn orchestration_wait() -> Arc<dyn ToolPack> {
    Arc::new(OneToolPack {
        id: "test.orchestration.v1",
        canonical: "wait",
        exposed: "wait",
        alternate: None,
    })
}

/// A pi-style jobs await: `wait`, or `wait_jobs` when another slot holds `wait`.
fn pi_wait(alternate: Option<&'static str>) -> Arc<dyn ToolPack> {
    Arc::new(OneToolPack {
        id: "test.pi.v1",
        canonical: "test_wait_jobs",
        exposed: "wait",
        alternate,
    })
}

fn target() -> Result<ProviderModelProjectionIdentityV2, TestError> {
    let effective: EffectiveProviderConfigurationId =
        serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"one-resolved-namespace"],
        )?)?)?;
    Ok(ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.namespace.v1",
        "test-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?)
}

fn surface(extra: Vec<Arc<dyn ToolPack>>) -> Result<CompiledSurface, ToolRuntimeError> {
    surface_under(extra, None)
}

fn surface_under(
    extra: Vec<Arc<dyn ToolPack>>,
    runtime: Option<CompositionRuntime>,
) -> Result<CompiledSurface, ToolRuntimeError> {
    let mut packs = standard_coding_packs_in_memory()?;
    packs.extend(extra);
    // Codex presents no `wait`, so the synthetic packs below meet only each
    // other and the runtime markers.
    compile(
        &packs,
        CodingPresentation::Codex,
        ToolRegistryBuilder::default(),
        target().expect("target"),
        runtime,
    )
}

fn names(names: &[ToolName]) -> Vec<&str> {
    names.iter().map(ToolName::as_str).collect()
}

#[test]
fn a_different_slot_clash_takes_the_declared_alternate_and_shadows_nothing() -> Result<(), TestError>
{
    let surface = surface(vec![orchestration_wait(), pi_wait(Some("wait_jobs"))])?;
    let exposed = names(surface.exposed_names());
    assert!(exposed.contains(&"wait") && exposed.contains(&"wait_jobs"));

    // Each name still dispatches to its own capability.
    for (exposed, canonical) in [("wait", "wait"), ("wait_jobs", "test_wait_jobs")] {
        let entry = surface
            .projection()
            .by_exposed(&ToolName::new(exposed)?)
            .expect("presented");
        assert_eq!(
            entry
                .canonical_operations()
                .values()
                .map(|identity| identity.name().as_str())
                .collect::<Vec<_>>(),
            [canonical],
            "{exposed}"
        );
    }

    // The registry-free menu listing resolves the same way.
    let mut packs = standard_coding_packs_in_memory()?;
    packs.extend([orchestration_wait(), pi_wait(Some("wait_jobs"))]);
    assert_eq!(
        names(&menu_names(&packs, CodingPresentation::Codex)?),
        exposed
    );
    Ok(())
}

#[test]
fn an_undeclared_clash_fails_construction_naming_both_providers() {
    let error = surface(vec![orchestration_wait(), pi_wait(None)])
        .expect_err("a clash without an alternate is never shadowed");
    assert_eq!(
        error.to_string(),
        "tool name `wait` is presented by pack `test.orchestration.v1` and pack `test.pi.v1` in \
         one namespace; neither declares an alternate spelling"
    );
}

/// Under a program runtime the surface is the nested namespace and the
/// runtime's markers hold their names there: a pi-style `wait` of another slot
/// beside the native-ptc `wait` marker is compiled as `wait_jobs`, so the
/// projection and every nested catalog read the resolved name. Direct keeps
/// `wait`, and a `wait` with no alternate fails with the typed clash.
#[test]
fn a_runtime_marker_moves_a_nested_wait_to_its_declared_alternate() -> Result<(), TestError> {
    let direct = surface(vec![pi_wait(Some("wait_jobs"))])?;
    assert!(names(direct.exposed_names()).contains(&"wait"));
    assert!(!names(direct.exposed_names()).contains(&"wait_jobs"));

    let code_mode = surface_under(
        vec![pi_wait(Some("wait_jobs"))],
        Some(CompositionRuntime::CodexNative),
    )?;
    let nested = names(code_mode.exposed_names());
    assert!(nested.contains(&"wait_jobs"));
    assert!(!nested.contains(&"wait"));
    let entry = code_mode
        .projection()
        .by_exposed(&ToolName::new("wait_jobs")?)
        .expect("the alternate is the projected name");
    assert!(
        entry
            .canonical_operations()
            .values()
            .all(|identity| identity.name().as_str() == "test_wait_jobs")
    );

    // The js and py markers are `wait_js` and `wait_py`: nothing moves.
    for runtime in [
        CompositionRuntime::ProductJavaScript,
        CompositionRuntime::PythonPtc,
    ] {
        let nested = surface_under(vec![pi_wait(Some("wait_jobs"))], Some(runtime))?;
        assert_eq!(nested.exposed_names(), direct.exposed_names());
    }

    let error = surface_under(vec![pi_wait(None)], Some(CompositionRuntime::CodexNative))
        .expect_err("a marker spelling without an alternate is never shadowed");
    let clash = error.namespace_clash().expect("typed clash");
    assert_eq!(clash.name().as_str(), "wait");
    assert_eq!(
        clash.first(),
        plakous_tools::PresentationProvider::Runtime("native-ptc")
    );
    assert_eq!(
        clash.second(),
        plakous_tools::PresentationProvider::Pack("test.pi.v1")
    );
    Ok(())
}

/// The enrolled pi `wait` declares `wait_jobs`: beside the native-ptc `wait`
/// marker it is compiled as `wait_jobs` through the real pipeline, with no
/// placement forced, and keeps dispatching to the process await.
#[test]
fn the_pi_wait_takes_wait_jobs_beside_the_native_ptc_marker() -> Result<(), TestError> {
    let compile_pi = |runtime| {
        compile(
            &standard_coding_packs_in_memory()?,
            CodingPresentation::Pi,
            ToolRegistryBuilder::default(),
            target().expect("target"),
            runtime,
        )
    };
    let direct = compile_pi(None)?;
    assert_eq!(
        names(direct.exposed_names()),
        ["bash", "edit", "read", "stop", "wait", "write"]
    );
    let code_mode = compile_pi(Some(CompositionRuntime::CodexNative))?;
    assert_eq!(
        names(code_mode.exposed_names()),
        ["bash", "edit", "read", "stop", "wait_jobs", "write"]
    );
    let entry = code_mode
        .projection()
        .by_exposed(&ToolName::new("wait_jobs")?)
        .expect("the alternate is the projected name");
    assert_eq!(entry.lowering(), ToolProjectionLoweringV2::PiWait);
    for runtime in [
        CompositionRuntime::ProductJavaScript,
        CompositionRuntime::PythonPtc,
    ] {
        assert_eq!(
            compile_pi(Some(runtime))?.exposed_names(),
            direct.exposed_names()
        );
    }
    Ok(())
}

#[test]
fn every_runtime_declares_its_outer_markers() -> Result<(), TestError> {
    for (runtime, expected) in [
        (CompositionRuntime::CodexNative, ["exec", "wait"]),
        (
            CompositionRuntime::ProductJavaScript,
            ["exec_js", "wait_js"],
        ),
        (CompositionRuntime::PythonPtc, ["exec_py", "wait_py"]),
    ] {
        let markers = runtime.marker_presentations()?;
        assert!(markers.iter().all(|marker| marker.alternate().is_none()));
        assert_eq!(
            markers
                .iter()
                .map(|marker| marker.name().as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(runtime.marker_names(), expected);
    }
    Ok(())
}

/// A pack that presents a non-marker capability named `wait` whose `cell_id`
/// is a string, as a composition `wait` marker's is.
struct CellIdWaitPack;

impl ToolPack for CellIdWaitPack {
    fn id(&self) -> &'static str {
        "test.cell-id-wait.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        let definition = ToolDefinition::new(
            ToolName::new("wait").expect("canonical name"),
            "test.namespace.v1",
            "A tool that is not a composition marker.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"cell_id": {"type": "string"}},
                "required": ["cell_id"],
                "additionalProperties": false
            }))
            .expect("schema"),
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )
        .expect("definition");
        let executor_id = ToolExecutorId::new("test.cell-id-wait.executor.v1")?;
        Ok(vec![ToolRegistration::new(
            RegisteredToolId::new("test.cell-id-wait.v1")?,
            executor_id.clone(),
            ToolExecutorBindingIdentity::new(1, &[executor_id.as_str().as_bytes()])?,
            definition,
            Arc::new(Idle),
        )])
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        Ok(vec![MenuEntry::new(
            "wait",
            vec![MenuBinding::by_effect(
                ToolName::new("wait").expect("canonical name"),
            )],
            MenuShape::Canonical,
            ToolProjectionLoweringV2::CanonicalPassthrough,
        )])
    }

    fn pins(&self) -> &'static [PromptPin] {
        &[]
    }
}

/// Cell-wait normalisation is keyed on the marker executor identity, not on
/// the spelling: a numeric `cell_id` sent to a non-marker tool exposed as
/// `wait` is validated as sent and refused.
#[test]
fn a_non_marker_wait_is_not_normalised_like_a_cell_wait() -> Result<(), TestError> {
    use std::str::FromStr;

    let surface = surface(vec![Arc::new(CellIdWaitPack)])?;
    let request = |arguments: &str| -> Result<plakous_tools::ToolInvocationRequest, TestError> {
        Ok(plakous_tools::ToolInvocationRequest::new(
            plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000301")?,
            plakous_model::TurnId::from_str("00000000-0000-0000-0000-000000000302")?,
            plakous_model::AttemptId::from_str("00000000-0000-0000-0000-000000000303")?,
            plakous_model::CallId::from_str("00000000-0000-0000-0000-000000000304")?,
            ToolName::new("wait")?,
            plakous_model::ToolCorrelation::default(),
            arguments,
        ))
    };
    let error = surface
        .projection()
        .prepare(request(r#"{"cell_id":3}"#)?)
        .err()
        .ok_or("a numeric cell_id reached a non-marker wait")?;
    assert_eq!(error.kind(), ToolRuntimeErrorKind::InvalidArguments);
    let prepared = surface
        .projection()
        .prepare(request(r#"{"cell_id":"3"}"#)?)?;
    assert_eq!(prepared.invocation().arguments().value()["cell_id"], "3");
    Ok(())
}
