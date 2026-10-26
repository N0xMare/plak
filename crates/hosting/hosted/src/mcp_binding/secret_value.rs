use plakous::mcp::{SensitiveBytes, ServiceSecretValue};

/// One named secret's value on its way to a server: consumed whole into a
/// spawn's environment or a request header, never printed.
pub struct NamedSecretValue(String);

impl NamedSecretValue {
    /// Takes ownership of a value read from the store.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    pub(super) fn into_environment(self) -> Result<ServiceSecretValue, String> {
        ServiceSecretValue::new(self.0)
            .map_err(|_| "it cannot be passed to a server process".to_owned())
    }

    pub(super) fn into_header(self) -> SensitiveBytes {
        SensitiveBytes::new(self.0.into_bytes())
    }
}

impl std::fmt::Debug for NamedSecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NamedSecretValue(<redacted>)")
    }
}
