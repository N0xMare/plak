use super::tool_error;

#[test]
fn tool_error_truncates_long_host_text_on_a_character_boundary() {
    let prefix = "x".repeat(16 * 1024 - 1);
    let error = tool_error(format!("{prefix}éafter the cap"));
    assert_eq!(error.class.as_deref(), Some("ToolError"));
    assert_eq!(error.message, format!("{prefix}\n[diagnostic truncated]"));
}
