//! Refusals when served contract-bearing facts disagree with compiled pins.

/// Credential-free subscription compatibility drift; never an adaptive pin.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CompatibilityDrift {
    /// The server requires a newer reference client for this model.
    ClientVersion {
        /// Validated semantic version advertised by the catalog.
        required: String,
        /// Compiled reference version.
        pinned: &'static str,
    },
    /// One enrolled model's required wire/capability field disagrees.
    CatalogField {
        /// Exact validated enrolled model literal.
        model: String,
        /// Name of the contract-bearing field.
        field: &'static str,
        /// Bounded, credential-free diagnostic fact, never a response body.
        served: String,
        /// Compiled interpretation or accepted value.
        pinned: &'static str,
    },
    /// Pinned discovery metadata disagrees; the served endpoint is never used.
    Endpoint {
        /// Name only; discovered URLs are not retained in errors.
        field: &'static str,
    },
    /// A token response narrowed, widened or otherwise changed the request scope.
    GrantedScope,
    /// The signing-key set has no key for the pinned algorithm.
    SigningAlgorithm,
    /// A proven response shape rejected the compiled client version.
    VersionGate,
    /// A proven response shape rejected a beta actually sent by this request.
    UnknownBeta,
}

impl CompatibilityDrift {
    fn reason(&self) -> String {
        match self {
            Self::ClientVersion { required, pinned } => {
                format!("required client version {required} exceeds pinned {pinned}")
            }
            Self::CatalogField { model, field, .. } => {
                format!("model {model} catalog field {field} disagrees with the compiled contract")
            }
            Self::Endpoint { field } => {
                format!("OIDC metadata field {field} disagrees with the compiled pin")
            }
            Self::GrantedScope => {
                "granted OAuth scopes disagree with the compiled request".to_owned()
            }
            Self::SigningAlgorithm => {
                "signing keys no longer offer the pinned algorithm".to_owned()
            }
            Self::VersionGate => "provider rejected the compiled client version".to_owned(),
            Self::UnknownBeta => "provider rejected a compiled beta value".to_owned(),
        }
    }

    /// Formats the operator remedy without retaining provider response text.
    #[must_use]
    pub fn message(&self, route: &str) -> String {
        format!(
            "{}; this build's {route} contract is out of date; install a newer build",
            self.reason()
        )
    }
}

impl std::fmt::Display for CompatibilityDrift {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message("subscription"))
    }
}

impl std::error::Error for CompatibilityDrift {}
