//! Model-visible names of MCP tools.

use plakous_model::{ToolName, sha256_hex};
use plakous_tools::McpServerName;

use crate::error::{McpError, McpErrorKind};

/// Longest exposed name: the common provider function-name bound.
pub const MAX_EXPOSED_NAME_BYTES: usize = 64;
/// Bytes kept before the hash suffix when an exposed name is cut.
const CUT_PREFIX_BYTES: usize = 55;
/// Hex digits of `sha256(MCP name)` that disambiguate a cut name.
const CUT_HASH_HEX: usize = 8;

/// The exposed name of one MCP tool: `mcp__<server>__<tool>`.
///
/// Every byte of the MCP name outside `[A-Za-z0-9_-]` becomes `_`. A result
/// longer than [`MAX_EXPOSED_NAME_BYTES`] keeps its first 55 bytes and gains
/// `_` and the first 8 hex digits of the SHA-256 of the original MCP name.
///
/// # Errors
///
/// Refuses an empty MCP tool name.
pub fn exposed_tool_name(server: &McpServerName, mcp_name: &str) -> Result<ToolName, McpError> {
    if mcp_name.is_empty() {
        return Err(McpError::new(
            McpErrorKind::Listing,
            format!("mcp server {server} listed a tool with an empty name"),
        ));
    }
    let mapped = mcp_name
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut exposed = format!("mcp__{server}__{mapped}");
    if exposed.len() > MAX_EXPOSED_NAME_BYTES {
        // Every byte is ASCII here, so any cut is a character boundary.
        exposed.truncate(CUT_PREFIX_BYTES);
        let hash = sha256_hex(&[mcp_name.as_bytes()]);
        exposed.push('_');
        exposed.push_str(&hash[..CUT_HASH_HEX]);
    }
    ToolName::new(exposed).map_err(|error| {
        McpError::with_source(
            McpErrorKind::Listing,
            format!("mcp server {server} tool name could not be exposed"),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn server(name: &str) -> Result<McpServerName, Box<dyn std::error::Error>> {
        Ok(McpServerName::new(name)?)
    }

    #[test]
    fn maps_every_byte_outside_the_alphabet() -> TestResult {
        let github = server("github")?;
        assert_eq!(
            exposed_tool_name(&github, "create_issue")?.as_str(),
            "mcp__github__create_issue"
        );
        assert_eq!(
            exposed_tool_name(&github, "repo.search/v2")?.as_str(),
            "mcp__github__repo_search_v2"
        );
        assert_eq!(
            exposed_tool_name(&github, "get-file")?.as_str(),
            "mcp__github__get-file"
        );
        // A two-byte UTF-8 character maps byte by byte.
        assert_eq!(
            exposed_tool_name(&github, "caf\u{e9}")?.as_str(),
            "mcp__github__caf__"
        );
        Ok(())
    }

    #[test]
    fn long_names_are_cut_and_suffixed_with_the_original_name_hash() -> TestResult {
        let github = server("github")?;
        let long = "a".repeat(80);
        let exposed = exposed_tool_name(&github, &long)?;
        assert_eq!(exposed.as_str().len(), MAX_EXPOSED_NAME_BYTES);
        let hash = sha256_hex(&[long.as_bytes()]);
        assert_eq!(
            exposed.as_str(),
            format!("{}_{}", &format!("mcp__github__{long}")[..55], &hash[..8])
        );
        // Exactly 64 bytes is kept verbatim.
        let fits = "b".repeat(64 - "mcp__github__".len());
        assert_eq!(
            exposed_tool_name(&github, &fits)?.as_str(),
            format!("mcp__github__{fits}")
        );
        // Two long names sharing a prefix stay distinct.
        let other = format!("{}c", "a".repeat(79));
        assert_ne!(exposed, exposed_tool_name(&github, &other)?);
        Ok(())
    }

    #[test]
    fn empty_names_are_refused() -> TestResult {
        assert!(exposed_tool_name(&server("github")?, "").is_err());
        Ok(())
    }
}
