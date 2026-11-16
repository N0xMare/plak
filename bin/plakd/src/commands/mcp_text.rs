//! Shared presentation of server-chosen MCP text.

/// Text a server chose (tool names, refusal notices), made safe to print.
pub(super) fn shown(text: &str) -> String {
    crate::cli_output::safe_terminal_text(text, 512)
}
