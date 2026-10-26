use crate::binding::CompositionSelection;
use crate::{CliFailure, CliResult};
use plakous::agent::{AgentBuilder, AgentError};
pub(crate) use plakous::agent::{
    LocalCompositionAttachment as Attachment, attach_local_composition,
};
use plakous::model::CompiledProviderConfiguration;
use plakous::tools::CodingPresentation;
use std::path::Path;
pub(in crate::agent) const PLAK_PRODUCT_EXTENSION_ISSUER_V1: &[u8] =
    b"plak.product-extension.issuer.v1";
pub(in crate::agent) const PRODUCT_JAVASCRIPT_DIRECT_CANDIDATE_V1: &[u8] =
    b"plak.candidate.product-javascript.direct.v1";
pub(in crate::agent) const PYTHON_DIRECT_VERTICAL_CANDIDATE_V1: &[u8] =
    b"plak.candidate.product-python.wire-compatible-direct.v1";

pub(crate) fn attach_codex_minimal(
    builder: AgentBuilder,
    descriptor: &plakous::tools::CodexMinimalLocalCodeModeDescriptorV1,
    host_path: &Path,
) -> Result<AgentBuilder, AgentError> {
    let profile = plakous::agent::LocalCompositionProfile::cell(&descriptor.into())?;
    let artifact = plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1(host_path)?;
    let attachment = plakous::agent::LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .build()?;
    attach_local_composition(builder, Some(attachment))
}

pub(crate) fn product_javascript_candidate_attachment(
    descriptor: &plakous::tools::ProductJavaScriptCompositionDescriptorV1,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
    presentation_kind: CodingPresentation,
) -> Option<Attachment> {
    let artifact =
        plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1(host_path).ok()?;
    let profile = plakous::agent::LocalCompositionProfile::cell(&descriptor.into()).ok()?;
    let issuer = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[PLAK_PRODUCT_EXTENSION_ISSUER_V1],
    )
    .ok()?;
    let selected = configuration.selected_identity().digest().identity_bytes();
    let effective = configuration.effective_identity().digest().identity_bytes();
    // Plak-owned custom composition candidate: not provider-native, not default.
    let qualification_candidate = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[
            PRODUCT_JAVASCRIPT_DIRECT_CANDIDATE_V1,
            &selected,
            &effective,
        ],
    )
    .ok()?;
    let evidence = plakous::agent::ProductExtensionEvidence::cell(
        &profile,
        &artifact,
        issuer,
        qualification_candidate,
        configuration,
    )
    .ok()?;
    plakous::agent::LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence)
        .coding_presentation_kind(presentation_kind)
        .build()
        .ok()
}

pub(crate) fn product_python_attachment(
    descriptor: &plakous::tools::PythonPtcDescriptorV1,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
    presentation_kind: CodingPresentation,
) -> CliResult<plakous::agent::LocalCompositionAttachment> {
    python_cell_attachment(
        &descriptor.into(),
        host_path,
        configuration,
        Some(presentation_kind),
    )
}

pub(crate) fn product_sdk_attachment(
    descriptor: &plakous::tools::PythonHarnessSdkDescriptorV1,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
) -> CliResult<plakous::agent::LocalCompositionAttachment> {
    python_cell_attachment(&descriptor.into(), host_path, configuration, None)
}

fn python_cell_attachment(
    descriptor: &plakous::tools::CellDescriptor,
    host_path: &Path,
    configuration: &CompiledProviderConfiguration,
    presentation_kind: Option<CodingPresentation>,
) -> CliResult<plakous::agent::LocalCompositionAttachment> {
    crate::capabilities::require_feature("python").map_err(|mut error| {
        error.id = match descriptor {
            plakous::tools::CellDescriptor::PythonHarnessSdkV1(_) => CompositionSelection::PySdk,
            _ => CompositionSelection::PyPtc,
        }
        .as_str();
        error
    })?;
    let artifact = plakous::agent::LocalCompositionEngineArtifact::monty_worker_v1(host_path)
        .map_err(|error| CliFailure::new(format!("Python host identity is invalid: {error}")))?;
    let profile = plakous::agent::LocalCompositionProfile::cell(descriptor)
        .map_err(|error| CliFailure::new(format!("Python profile identity is invalid: {error}")))?;
    let issuer = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[PLAK_PRODUCT_EXTENSION_ISSUER_V1],
    )
    .map_err(|error| CliFailure::new(format!("Python issuer identity is invalid: {error}")))?;
    let selected = configuration.selected_identity().digest().identity_bytes();
    let effective = configuration.effective_identity().digest().identity_bytes();
    let qualification_candidate = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ExecutionEnvironment,
        1,
        &[PYTHON_DIRECT_VERTICAL_CANDIDATE_V1, &selected, &effective],
    )
    .map_err(|error| CliFailure::new(format!("Python candidate identity is invalid: {error}")))?;
    let evidence = plakous::agent::ProductExtensionEvidence::cell(
        &profile,
        &artifact,
        issuer,
        qualification_candidate,
        configuration,
    )
    .map_err(|error| CliFailure::new(format!("Python extension evidence is invalid: {error}")))?;
    let builder = plakous::agent::LocalCompositionAttachment::builder(profile)
        .engine_artifact(artifact)
        .product_extension_evidence(evidence);
    let builder = if let Some(kind) = presentation_kind {
        builder.coding_presentation_kind(kind)
    } else {
        builder
    };
    builder
        .build()
        .map_err(|error| CliFailure::new(format!("Python attachment is invalid: {error}")))
}
