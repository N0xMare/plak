/// A named secret cannot reach an uncompiled MCP client.
pub struct NamedSecretValue;

impl NamedSecretValue {
    /// Takes ownership of a value that this build cannot send to MCP.
    #[must_use]
    pub fn new(value: String) -> Self {
        drop(value);
        Self
    }
}

impl std::fmt::Debug for NamedSecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NamedSecretValue(<redacted>)")
    }
}
