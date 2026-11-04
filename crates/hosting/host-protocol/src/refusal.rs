//! Transient refusal facts whose command grammar belongs to the frontend.

use crate::notice_remedy::ProductSessionId;

/// The typed credential gate that refused a child template.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildCredentialState {
    /// No signed-in Ready login was available.
    Unavailable,
    /// Credential readiness could not be checked.
    CheckUnavailable,
}

/// Typed action-refusal data; never persisted as a notice remedy.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RefusalAdvice {
    /// Inspect operator storage health after a preventive admission refusal.
    CheckStorage,
    /// A child template's account could not pass the credential gate.
    ChildCredential {
        /// Validated template name.
        template: String,
        /// Original native account selected by the template.
        account: crate::notice_remedy::AccountTarget,
        /// Typed credential-gate result.
        state: ChildCredentialState,
    },
    /// A model is absent from the selected account's catalog.
    UnknownModel {
        /// Exact model requested.
        model: String,
        /// Account whose models should be inspected.
        account: String,
        /// Catalog that refused the model.
        catalog: String,
    },
    /// A model requires one of the offered root reasoning efforts.
    ExplicitEffort {
        /// Exact selected account.
        account: String,
        /// Exact selected model.
        model: String,
        /// Offered effort identifiers in provider order.
        offered: Vec<String>,
    },
    /// Explicitly requested reasoning effort was downgraded at compilation.
    EffortDowngraded,
    /// The named MCP server requires authentication, possibly with additional scopes.
    McpLogin {
        /// Validated server identifier.
        server: String,
        /// Validated OAuth scope identifiers; empty means no scope option.
        scopes: Vec<String>,
    },
    /// A configured secret is absent.
    SecretMissing {
        /// Server whose configuration names the secret.
        server: String,
        /// Secret name only, never its value.
        name: String,
    },
    /// An incompatible build must export this exact session.
    ExportSession {
        /// Exact source session identity.
        session_id: ProductSessionId,
    },
}

/// Structured tool-result refusals whose help is rendered by the frontend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRefusal {
    /// A topology action's closed error code and optional repair facts.
    Topology {
        /// Exact refusal code.
        code: crate::topology::TopologyErrorCode,
        /// Neutral failure facts.
        message: String,
        /// Additional target facts, when the code is too broad.
        advice: Option<RefusalAdvice>,
        /// Semantic repair retained through composed runtime failures.
        remedy: Option<crate::notice_remedy::NoticeRemedy>,
    },
    /// An MCP authorization refusal, before any tool effect.
    McpLogin {
        /// Neutral failure facts.
        message: String,
        /// Original server identifier.
        server: String,
        /// Validated canonical scope list; empty means no step-up scope.
        scope: String,
    },
    /// A named MCP secret was missing when the call was prepared.
    McpSecret {
        /// Neutral failure facts.
        message: String,
        /// Server whose configuration names the secret.
        server: String,
        /// Secret name only.
        name: String,
    },
}

impl ToolRefusal {
    /// Decode only closed structured error payloads, never rendered prose.
    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Topology {
            code: crate::topology::TopologyErrorCode,
            message: String,
            #[serde(default)]
            advice: Option<RefusalAdvice>,
            #[serde(default)]
            remedy: Option<crate::notice_remedy::NoticeRemedy>,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Authorization {
            message: String,
            server: String,
            scope: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Mcp {
            mcp_authorization: Authorization,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SecretBody {
            message: String,
            server: String,
            name: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Secret {
            mcp_secret: SecretBody,
        }
        if let Ok(Topology {
            code,
            message,
            advice,
            remedy,
        }) = serde_json::from_str(raw)
        {
            return Some(Self::Topology {
                code,
                message,
                advice,
                remedy,
            });
        }
        if let Ok(Mcp {
            mcp_authorization:
                Authorization {
                    message,
                    server,
                    scope,
                },
        }) = serde_json::from_str::<Mcp>(raw)
        {
            return Some(Self::McpLogin {
                message,
                server,
                scope,
            });
        }
        let Secret {
            mcp_secret:
                SecretBody {
                    message,
                    server,
                    name,
                },
        } = serde_json::from_str(raw).ok()?;
        Some(Self::McpSecret {
            message,
            server,
            name,
        })
    }

    /// Whether a readable preview must retain its structured repair data.
    #[must_use]
    pub const fn has_help(&self) -> bool {
        matches!(
            self,
            Self::McpLogin { .. }
                | Self::McpSecret { .. }
                | Self::Topology {
                    advice: Some(_),
                    ..
                }
                | Self::Topology {
                    remedy: Some(_),
                    ..
                }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ToolRefusal;

    #[test]
    fn an_mcp_secret_document_decodes_without_a_secret_value() {
        let raw = concat!(
            r#"{"mcp_secret":{"message":"secret token is not set; the call was not sent","#,
            r#""server":"fake","name":"token"}}"#
        );
        let Some(ToolRefusal::McpSecret {
            message,
            server,
            name,
        }) = ToolRefusal::decode(raw)
        else {
            panic!("not an mcp secret refusal");
        };
        assert_eq!(server, "fake");
        assert_eq!(name, "token");
        assert!(!message.contains("s3cret-value"));
        assert!(ToolRefusal::decode(r#"{"mcp_secret":{"message":"x","server":"fake"}}"#).is_none());
    }
}
