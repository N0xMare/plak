use super::*;
use plakous_compose::{CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn profiles() -> TestResult<Vec<LocalCompositionProfile>> {
    let mut registry = plakous_tools::ToolRegistryBuilder::default();
    plakous_tools::register_packs(
        &plakous_compose::standard_coding_packs_in_memory()?,
        &mut registry,
    )?;
    CodexNativeLocalCodeModeV1::register_markers_into(&mut registry)?;
    ProductJavaScriptCompositionV1::register_markers_into(&mut registry)?;
    PythonPtcV1::register_markers_into(&mut registry)?;
    let registry = registry.freeze();
    let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"cell-attachment-matrix"],
    )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "openai.codex.responses",
        "openai.codex_subscription",
        "openai.codex.compat.0.153.3",
        "responses-family",
        "gpt-codex-code-mode-test",
        "gpt-codex-code-mode-test",
        configuration,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let job = plakous_tools::JobConfig::default();
    Ok(vec![
        LocalCompositionProfile::cell(
            &(&CodexNativeLocalCodeModeV1::compile(job, registry.clone(), target.clone())?).into(),
        )?,
        LocalCompositionProfile::cell(
            &(&CodexMinimalLocalCodeModeV1::compile(job, registry.clone(), target.clone())?).into(),
        )?,
        LocalCompositionProfile::cell(
            &(&ProductJavaScriptCompositionV1::compile_for_kind(
                job,
                registry.clone(),
                target.clone(),
                plakous_tools::CodingPresentation::Codex,
                [ToolName::new("read_file")?],
            )?)
                .into(),
        )?,
        LocalCompositionProfile::cell(
            &(&PythonPtcV1::compile_for_kind(
                job,
                registry,
                target,
                plakous_compose::PythonPtcPresentationV1::StructuredFunctions,
                plakous_tools::CodingPresentation::Codex,
                [ToolName::new("read_file")?],
            )?)
                .into(),
        )?,
    ])
}

fn evidence(
    profile: &LocalCompositionProfile,
    artifact: &LocalCompositionEngineArtifact,
) -> TestResult<ProductExtensionEvidence> {
    let facts = extension_facts(profile)?;
    let identity = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"receipt"])?;
    Ok(ProductExtensionEvidence {
        target: facts.target,
        profile: profile.identity(),
        artifact: artifact.identity(),
        catalog: facts.catalog,
        presentation: facts.presentation,
        limits: facts.limits,
        host: artifact.host_identity(),
        engine_contract: artifact.engine_contract_identity(),
        containment: artifact.containment().identity(),
        product_issuer: identity,
        qualification_receipt: identity,
        receipt: identity,
        selected_configuration: serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::SelectedProviderConfiguration,
            5,
            &[b"matrix"],
        )?)?)?,
        effective_configuration: serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"matrix"],
        )?)?)?,
    })
}

#[test]
fn cell_profiles_preserve_the_complete_descriptor_and_projection_evidence() -> TestResult {
    for (index, profile) in profiles()?.into_iter().enumerate() {
        assert_eq!(profile.identity(), profile.descriptor.identity());
        assert_eq!(profile.identity().version(), 3);
        assert_eq!(
            profile.language(),
            if index == 3 {
                LocalCompositionLanguage::Python
            } else {
                LocalCompositionLanguage::JavaScript
            }
        );
        if index >= 2 {
            let facts = extension_facts(&profile)?;
            assert_eq!(
                facts.catalog,
                profile.descriptor.nested_projection().digest()
            );
            assert_eq!(
                facts.presentation,
                profile.descriptor.outer_projection().digest()
            );
        } else {
            assert!(extension_facts(&profile).is_err());
        }
    }
    Ok(())
}

#[test]
fn attachment_accepts_only_the_exact_profile_engine_and_evidence_lane() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("worker");
    std::fs::write(&path, b"offline matrix worker")?;
    let path = path.canonicalize()?;
    let profiles = profiles()?;
    for (index, profile) in profiles.iter().enumerate() {
        for python in [false, true] {
            for with_receipt in [false, true] {
                let artifact = if python {
                    LocalCompositionEngineArtifact::monty_worker_v1(&path)?
                } else {
                    LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?
                };
                let receipt = if with_receipt {
                    Some(evidence(
                        if index >= 2 { profile } else { &profiles[2] },
                        &artifact,
                    )?)
                } else {
                    None
                };
                let mut builder = LocalCompositionAttachment::builder(profile.clone())
                    .engine_artifact(artifact)
                    .coding_presentation_kind(plakous_tools::CodingPresentation::Codex);
                if let Some(receipt) = receipt {
                    builder = builder.product_extension_evidence(receipt);
                }
                assert_eq!(
                    builder.build().is_ok(),
                    python == (index == 3) && with_receipt == (index >= 2),
                    "profile {index}, python {python}, receipt {with_receipt}"
                );
            }
        }
        assert!(
            LocalCompositionAttachment::builder(profile.clone())
                .build()
                .is_err()
        );
        if index >= 2 {
            let artifact = if index == 3 {
                LocalCompositionEngineArtifact::monty_worker_v1(&path)?
            } else {
                LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?
            };
            let receipt = evidence(profile, &artifact)?;
            assert!(
                LocalCompositionAttachment::builder(profile.clone())
                    .engine_artifact(artifact)
                    .product_extension_evidence(receipt)
                    .build()
                    .is_err(),
                "missing presentation kind"
            );
        }
    }
    Ok(())
}

#[test]
fn every_attachment_evidence_fact_is_checked_for_both_languages() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("worker");
    std::fs::write(&path, b"offline evidence worker")?;
    let path = path.canonicalize()?;
    for (index, profile) in profiles()?.into_iter().enumerate().skip(2) {
        let artifact = if index == 3 {
            LocalCompositionEngineArtifact::monty_worker_v1(&path)?
        } else {
            LocalCompositionEngineArtifact::quickjs_worker_v1(&path)?
        };
        let receipt = evidence(&profile, &artifact)?;
        assert!(validate_product_evidence(&profile, &artifact, &receipt).is_ok());
        let neighbor = Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &[b"neighbor"])?;
        for axis in 0..11 {
            let mut drifted = receipt.clone();
            match axis {
                0 => drifted.target = IntegrationClass::Custom,
                1 => drifted.profile = neighbor,
                2 => drifted.artifact = neighbor,
                3 => drifted.catalog = neighbor,
                4 => drifted.presentation = neighbor,
                5 => drifted.host = neighbor,
                6 => drifted.engine_contract = neighbor,
                7 => drifted.containment = neighbor,
                8 => drifted.limits.max_source_bytes += 1,
                9 => drifted.limits.max_yield_time_ms += 1,
                _ => drifted.limits.max_output_tokens += 1,
            }
            assert!(
                validate_product_evidence(&profile, &artifact, &drifted).is_err(),
                "profile {index}, axis {axis}"
            );
        }
    }
    Ok(())
}
