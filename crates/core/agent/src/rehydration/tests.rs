use super::*;
use crate::{
    CompilerModelFacts, CompilerRuntimeFacts, CompilerToolFacts, FeatureSupportFact,
    HarnessProfileId, HarnessProfileSpecBuilder,
};
use plakous_model::{
    EffectiveProviderConfigurationId, ModelAdapterProfileId, ModelId, ProtocolId, RouteId,
    SelectedProviderConfigurationId,
};

fn provider_configuration_ids(
    tag: &[u8],
) -> Result<
    (
        SelectedProviderConfigurationId,
        EffectiveProviderConfigurationId,
    ),
    Box<dyn std::error::Error>,
> {
    let selected = Digest::sha256(DigestDomain::SelectedProviderConfiguration, 5, &[tag])?;
    let effective = Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[tag])?;
    Ok((
        serde_json::from_value(serde_json::to_value(selected)?)?,
        serde_json::from_value(serde_json::to_value(effective)?)?,
    ))
}

fn fixture() -> Result<
    (
        HarnessProfileSpec,
        HarnessCompilerInputs,
        ResolvedHarnessPlan,
    ),
    Box<dyn std::error::Error>,
> {
    let digest = |domain, tag: &'static [u8]| Digest::sha256(domain, 1, &[tag]);
    let spec = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("codec.fixture")?)
        .base_instructions("persist exact neutral facts")
        .build()?;
    let (selected, effective) = provider_configuration_ids(b"codec-configuration")?;
    let model = CompilerModelFacts::new(
        selected,
        effective,
        ProtocolId::new("test.protocol")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        ModelAdapterProfileId::new("test.adapter")?,
        digest(DigestDomain::ModelBinding, b"model")?,
        FeatureSupportFact::Unsupported,
        FeatureSupportFact::Unsupported,
        FeatureSupportFact::Unsupported,
    );
    let tools = CompilerToolFacts::new(
        digest(DigestDomain::ToolDefinition, b"tools")?,
        vec!["read_file".to_owned()],
    )?;
    let runtime = CompilerRuntimeFacts::new(
        digest(DigestDomain::ExecutionEnvironment, b"environment")?,
        crate::MAX_INSTRUCTION_BYTES,
        false,
        false,
        false,
    );
    let inputs = HarnessCompilerInputs::new(model, tools, runtime, Vec::new())?;
    let plan = HarnessProfileCompiler::new().compile(&spec, &inputs)?;
    Ok((spec, inputs, plan))
}

#[test]
fn canonical_round_trip_recompiles_exact_plan() -> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let bytes = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?.encode()?;
    let HarnessPlanReadiness::Exact(rehydrated) = rehydrate_harness_plan(&bytes, 1)? else {
        return Err("exact plan was not ready".into());
    };
    assert_eq!(rehydrated.plan_digest(), plan.plan_digest());
    assert_eq!(
        canonical_plan_bytes(&rehydrated)?,
        canonical_plan_bytes(&plan)?
    );
    assert_eq!(
        rehydrated.selected_provider_configuration_id(),
        plan.selected_provider_configuration_id()
    );
    assert_eq!(
        rehydrated.effective_provider_configuration_id(),
        plan.effective_provider_configuration_id()
    );
    Ok(())
}

#[test]
fn configuration_ids_cannot_be_missing_swapped_or_tampered()
-> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let baseline = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?;
    let original: serde_json::Value =
        serde_json::from_slice(&baseline.manifest.compiler_input_bytes)?;

    let mut missing = baseline.clone();
    let mut missing_wire = original.clone();
    missing_wire
        .as_object_mut()
        .ok_or("compiler inputs must be an object")?
        .remove("selected_provider_configuration_id");
    missing.manifest.compiler_input_bytes = serde_json::to_vec(&missing_wire)?;
    assert_eq!(
        rehydrate_harness_plan(&missing.encode()?, 1)?,
        mismatch(HarnessPlanMismatch::CompilerInputs)
    );

    let mut swapped = baseline.clone();
    let mut swapped_wire = original.clone();
    let object = swapped_wire
        .as_object_mut()
        .ok_or("compiler inputs must be an object")?;
    let selected = object
        .get("selected_provider_configuration_id")
        .cloned()
        .ok_or("selected identity")?;
    let effective = object
        .get("effective_provider_configuration_id")
        .cloned()
        .ok_or("effective identity")?;
    object.insert("selected_provider_configuration_id".to_owned(), effective);
    object.insert("effective_provider_configuration_id".to_owned(), selected);
    swapped.manifest.compiler_input_bytes = serde_json::to_vec(&swapped_wire)?;
    assert_eq!(
        rehydrate_harness_plan(&swapped.encode()?, 1)?,
        mismatch(HarnessPlanMismatch::CompilerInputs)
    );

    let mut tampered = baseline;
    let mut tampered_wire = original;
    let (other_selected, _) = provider_configuration_ids(b"other-configuration")?;
    tampered_wire["model"]["selected_provider_configuration_id"] =
        serde_json::to_value(other_selected)?;
    tampered.manifest.compiler_input_bytes = serde_json::to_vec(&tampered_wire)?;
    assert_eq!(
        rehydrate_harness_plan(&tampered.encode()?, 1)?,
        mismatch(HarnessPlanMismatch::CompilerInputs)
    );
    Ok(())
}

#[test]
fn pre_configuration_component_encodings_are_not_decoded() -> Result<(), Box<dyn std::error::Error>>
{
    let (spec, inputs, plan) = fixture()?;
    let mut envelope = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?;
    envelope.manifest.compiler_input_encoding_version = 3;
    assert_eq!(
        rehydrate_harness_plan(&envelope.encode()?, 1)?,
        mismatch(HarnessPlanMismatch::ComponentEncoding {
            component: "compiler-input",
            found: 3,
        })
    );
    envelope.manifest.compiler_input_encoding_version = HARNESS_COMPILER_INPUT_ENCODING_VERSION;
    envelope.manifest.resolved_plan_encoding_version = 3;
    assert_eq!(
        rehydrate_harness_plan(&envelope.encode()?, 1)?,
        mismatch(HarnessPlanMismatch::ComponentEncoding {
            component: "resolved-plan",
            found: 3,
        })
    );
    Ok(())
}

#[test]
fn debug_redacts_canonical_plan_payloads() -> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let envelope = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?;
    let debug = format!("{envelope:?}");
    assert!(!debug.contains("persist exact neutral facts"));
    assert!(!debug.contains("read_file"));
    assert!(debug.contains("profile_bytes"));
    Ok(())
}

#[test]
fn decoder_refuses_unknown_versions_tags_corruption_and_trailing_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let bytes = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?.encode()?;
    let mut unknown_version = bytes.clone();
    unknown_version[8..12].copy_from_slice(&2_u32.to_be_bytes());
    assert_eq!(
        rehydrate_harness_plan(&unknown_version, 1),
        Err(HarnessManifestCodecError::UnsupportedEnvelopeVersion { found: 2 })
    );
    let mut unknown_tag = bytes.clone();
    unknown_tag[12] = 7;
    assert_eq!(
        rehydrate_harness_plan(&unknown_tag, 1),
        Err(HarnessManifestCodecError::UnknownTag { tag: 7 })
    );
    let mut corrupt = bytes.clone();
    corrupt[17] ^= 1;
    assert_eq!(
        rehydrate_harness_plan(&corrupt, 1),
        Err(HarnessManifestCodecError::IntegrityMismatch)
    );
    let mut trailing = bytes;
    trailing.push(0);
    assert_eq!(
        rehydrate_harness_plan(&trailing, 1),
        Err(HarnessManifestCodecError::TrailingBytes)
    );
    Ok(())
}

#[test]
fn decoder_rejects_zero_runtime_policy_even_with_valid_integrity()
-> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let mut envelope = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?;
    envelope.manifest.runtime_policy_version = 0;
    assert_eq!(
        envelope.encode(),
        Err(HarnessManifestCodecError::ZeroRuntimePolicyVersion)
    );
    assert_eq!(
        rehydrate_harness_plan(&[0; 0], 0),
        Err(HarnessManifestCodecError::ZeroRuntimePolicyVersion)
    );
    Ok(())
}

#[test]
fn unavailable_revision_is_a_typed_mismatch() -> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let mut envelope = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 1)?;
    envelope.manifest.compiler_revision += 1;
    let ready = rehydrate_harness_plan(&envelope.encode()?, 1)?;
    assert_eq!(
        ready,
        mismatch(HarnessPlanMismatch::CompilerRevision {
            found: HARNESS_PROFILE_COMPILER_REVISION + 1
        })
    );
    Ok(())
}

#[test]
fn oversized_input_is_rejected_before_parsing() {
    let bytes = vec![0; MAX_HARNESS_PLAN_COMPONENT_BYTES + 1];
    assert_eq!(
        rehydrate_harness_plan(&bytes, 1),
        Err(HarnessManifestCodecError::EnvelopeTooLarge)
    );
}

#[test]
fn golden_bytes_and_manifest_digest() -> Result<(), Box<dyn std::error::Error>> {
    let (spec, inputs, plan) = fixture()?;
    let bytes = HarnessPlanComponentEnvelope::capture(&spec, &inputs, &plan, 7)?.encode()?;
    assert_eq!(&bytes[..17], b"PLAKHPC\0\0\0\0\x01\x01\0\0\x15\x9d");
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[&bytes])?;
    assert_eq!(
        hex(digest.bytes()),
        "66864dcff183216ee4fec225699aa087c6dc642289c4d04639d80be3643d8248"
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}
