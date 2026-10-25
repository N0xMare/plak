use plakous_model::{Digest, DigestDomain};
use plakous_tools::{ToolProjectionMap, ToolRuntimeError};

use super::compile::definition_error;
use super::host_api::HostApi;
use super::presentation::{CellPresentation, OuterInput};

pub(super) fn descriptor_identity(
    presentation: &CellPresentation,
    outer: &ToolProjectionMap,
    nested: &ToolProjectionMap,
    api: &HostApi,
    execute_text: &str,
) -> Result<Digest, ToolRuntimeError> {
    let grammar = match presentation.execute.input {
        OuterInput::Custom { grammar, .. } => grammar,
        OuterInput::Structured(_) => "",
    };
    let limits = serde_json::to_vec(&presentation.limits).map_err(definition_error)?;
    let semantics = serde_json::to_vec(&(
        presentation.yield_mode,
        presentation.lifecycle,
        presentation.catalog,
        presentation.execute.input_field,
        presentation.result,
    ))
    .map_err(definition_error)?;
    let catalog = serde_json::to_vec(api).map_err(definition_error)?;
    let outer_digest = outer.digest();
    let nested_digest = nested.digest();
    let mut parts = vec![presentation.profile.as_bytes()];
    parts.extend(
        presentation
            .revisions
            .iter()
            .map(|revision| revision.as_bytes()),
    );
    parts.extend([
        execute_text.as_bytes(),
        presentation.messages.wait.as_bytes(),
        grammar.as_bytes(),
        &limits,
        &semantics,
        outer_digest.bytes(),
        nested_digest.bytes(),
        &catalog,
    ]);
    Digest::sha256(DigestDomain::ToolDefinition, 3, &parts).map_err(definition_error)
}
