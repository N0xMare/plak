use crate::{CliFailure, CliResult};
use plakous::agent::{AgentBuilder, AgentError, AgentErrorKind};
use plakous::model::CompiledProviderConfiguration;
use plakous::tools::CodingPresentation;
use std::path::Path;
pub(crate) enum Attachment {}
pub(crate) fn attach_local_composition(
    builder: AgentBuilder,
    attachment: Option<Attachment>,
) -> Result<AgentBuilder, AgentError> {
    match attachment {
        Some(attachment) => match attachment {},
        None => Ok(builder),
    }
}
pub(crate) fn attach_codex_minimal(
    _: AgentBuilder,
    _: &plakous::tools::CodexMinimalLocalCodeModeDescriptorV1,
    _: &Path,
) -> Result<AgentBuilder, AgentError> {
    Err(AgentError::new(
        AgentErrorKind::InvalidInput,
        "JavaScript is not in this build",
    ))
}
pub(crate) fn product_javascript_candidate_attachment(
    _: &plakous::tools::ProductJavaScriptCompositionDescriptorV1,
    _: &Path,
    _: &CompiledProviderConfiguration,
    _: CodingPresentation,
) -> Option<Attachment> {
    None
}
pub(crate) fn product_python_attachment(
    _: &plakous::tools::PythonPtcDescriptorV1,
    _: &Path,
    _: &CompiledProviderConfiguration,
    _: CodingPresentation,
) -> CliResult<Attachment> {
    Err(CliFailure::from(
        crate::capabilities::CapabilityUnavailable {
            id: "python",
            kind: crate::capabilities::CapabilityKind::Evaluator,
            feature: "python",
        },
    ))
}

pub(crate) fn product_sdk_attachment(
    _: &plakous::tools::PythonHarnessSdkDescriptorV1,
    _: &Path,
    _: &CompiledProviderConfiguration,
) -> CliResult<Attachment> {
    Err(CliFailure::from(
        crate::capabilities::CapabilityUnavailable {
            id: "py-sdk",
            kind: crate::capabilities::CapabilityKind::Evaluator,
            feature: "python",
        },
    ))
}
