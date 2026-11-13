//! Release-owned executable metadata, independent of route ports and credentials.

use crate::{CodingPresentation, ISSUER, ToolExecutorBindingIdentity, ToolRuntimeError, WebTool};

pub(crate) fn fetch(images: bool) -> Result<ToolExecutorBindingIdentity, ToolRuntimeError> {
    ToolExecutorBindingIdentity::new(2, &[ISSUER, b"fetch_url", &[u8::from(images)]])
}

pub(crate) fn image() -> Result<ToolExecutorBindingIdentity, ToolRuntimeError> {
    ToolExecutorBindingIdentity::new(1, &[ISSUER, b"image_gen"])
}

pub(crate) fn retrieval(
    tool: WebTool,
    presented: &str,
) -> Result<ToolExecutorBindingIdentity, ToolRuntimeError> {
    ToolExecutorBindingIdentity::new(
        1,
        &[
            ISSUER,
            tool.template_name().as_bytes(),
            presented.as_bytes(),
        ],
    )
}

/// Computes current optional-tool executable contracts without building a port.
/// Image-result variants are distinct supported fetch contracts.
///
/// # Errors
/// Returns an invalid binding identity.
#[doc(hidden)]
pub fn current_executor_bindings(
    presented: &str,
    executor: &str,
) -> Result<Vec<ToolExecutorBindingIdentity>, ToolRuntimeError> {
    if presented == "fetch_url" && executor == "plakous.web.fetch_url.executor.v1" {
        return [false, true].into_iter().map(fetch).collect();
    }
    if presented == "image_gen" && executor == "plakous.web.image_gen.executor.v1" {
        return Ok(vec![image()?]);
    }
    for tool in [WebTool::WebSearch, WebTool::WebFetch, WebTool::XSearch] {
        if executor != format!("plakous.web.{}.executor.v1", tool.template_name()) {
            continue;
        }
        if [
            CodingPresentation::Codex,
            CodingPresentation::Claude,
            CodingPresentation::Grok,
            CodingPresentation::Pi,
        ]
        .into_iter()
        .any(|vocabulary| tool.presented_name(vocabulary) == Some(presented))
        {
            return Ok(vec![retrieval(tool, presented)?]);
        }
    }
    Ok(Vec::new())
}
