//! The root's optional tools, frozen into its session record when its
//! runtime is first created.
//!
//! Editing the `[root] agent` template, a `[tools.*]` table or an MCP server
//! later never changes a session that already exists: reopen, clear and
//! rebind read only this record. A new session takes the configuration as it
//! is then.

use std::collections::BTreeMap;

use plakous::mcp::McpToolRecord;
use serde::{Deserialize, Serialize};

use crate::binding::RootTemplateFact;
use crate::tool_plan::{NodeToolSelection, TemplateToolSelection};

/// The `[root] agent` template as configured when the session was created,
/// and the MCP tools its selection reached then.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FrozenRootTools {
    template: Option<RootTemplateFact>,
    /// `Some` once the selection's MCP servers were listed at creation;
    /// `None` when the session's tuple did not run the template's tools then.
    mcp_tools: Option<Vec<McpToolRecord>>,
}

impl FrozenRootTools {
    /// Freezes the configured `[root] agent` template; its MCP tools are
    /// added by [`Self::with_mcp_tools`] once listed.
    #[must_use]
    pub fn new(template: Option<RootTemplateFact>) -> Self {
        Self {
            template,
            mcp_tools: None,
        }
    }

    /// The same freeze with the MCP tools listed at creation.
    #[must_use]
    pub fn with_mcp_tools(mut self, records: Vec<McpToolRecord>) -> Self {
        self.mcp_tools = Some(records);
        self
    }

    /// The root's tools for the tuple it runs: the frozen template's list
    /// when the tuple is exactly the template's, and none otherwise
    /// (spec 03 §3).
    #[must_use]
    pub fn selection(&self, account: Option<&str>, model: Option<&str>) -> NodeToolSelection {
        let mut tools =
            RootTemplateFact::root_tool_selection(self.template.as_ref(), account, model);
        if !tools.selection.is_empty() {
            tools.mcp_tools.clone_from(&self.mcp_tools);
        }
        tools
    }

    /// The JSON the product store keeps in the session row.
    ///
    /// # Errors
    ///
    /// Returns a one-line reason when the freeze cannot be encoded.
    pub fn encode(&self) -> Result<String, String> {
        let wire = FrozenRootToolsWire {
            template: self.template.as_ref().map(|fact| TemplateWire {
                name: fact.name.clone(),
                account: fact.account.clone(),
                model: fact.model.clone(),
                tools: fact
                    .tools
                    .entries()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                tool_sections: fact.tool_sections.clone(),
            }),
            mcp_tools: self.mcp_tools.clone(),
        };
        serde_json::to_string(&wire).map_err(|_| "frozen root tools are not encodable".to_owned())
    }

    /// Decodes [`Self::encode`]'s JSON, revalidating the template's list.
    ///
    /// # Errors
    ///
    /// Returns a one-line reason for malformed JSON, an invalid tool list or
    /// an invalid MCP tool record.
    pub fn decode(encoded: &str) -> Result<Self, String> {
        let wire: FrozenRootToolsWire = serde_json::from_str(encoded)
            .map_err(|error| format!("frozen root tools are invalid: {error}"))?;
        let template = wire
            .template
            .map(|template| {
                let tools = TemplateToolSelection::parse(&template.tools)
                    .map_err(|error| format!("frozen root tools are invalid: {error}"))?;
                Ok::<_, String>(RootTemplateFact {
                    name: template.name,
                    account: template.account,
                    model: template.model,
                    tools,
                    tool_sections: template.tool_sections,
                })
            })
            .transpose()?;
        Ok(Self {
            template,
            mcp_tools: wire.mcp_tools,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenRootToolsWire {
    template: Option<TemplateWire>,
    mcp_tools: Option<Vec<McpToolRecord>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TemplateWire {
    name: String,
    account: String,
    model: String,
    tools: Vec<String>,
    tool_sections: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fact(tools: &[&str]) -> Result<RootTemplateFact, Box<dyn std::error::Error>> {
        Ok(RootTemplateFact {
            name: "lead".to_owned(),
            account: "codex".to_owned(),
            model: "m".to_owned(),
            tools: TemplateToolSelection::parse(tools)?,
            tool_sections: BTreeMap::from([(
                "tools.fetch_url".to_owned(),
                "[tools.fetch_url]\nmax_bytes = 4096\n".to_owned(),
            )]),
        })
    }

    #[test]
    fn a_freeze_round_trips_and_applies_only_on_the_templates_tuple() -> TestResult {
        let frozen = FrozenRootTools::new(Some(fact(&["fetch_url", "mcp:github"])?))
            .with_mcp_tools(Vec::new());
        let decoded = FrozenRootTools::decode(&frozen.encode()?)?;
        assert_eq!(decoded, frozen);
        let on_template = decoded.selection(Some("codex"), Some("m"));
        assert_eq!(
            on_template.selection,
            TemplateToolSelection::parse(["fetch_url", "mcp:github"])?
        );
        assert_eq!(
            on_template
                .tool_sections
                .get("tools.fetch_url")
                .map(String::as_str),
            Some("[tools.fetch_url]\nmax_bytes = 4096\n")
        );
        assert_eq!(on_template.mcp_tools, Some(Vec::new()));
        assert_eq!(
            decoded.selection(Some("codex"), Some("other")),
            NodeToolSelection::default()
        );
        let unconfigured = FrozenRootTools::decode(&FrozenRootTools::new(None).encode()?)?;
        assert_eq!(
            unconfigured.selection(Some("codex"), Some("m")),
            NodeToolSelection::default()
        );
        Ok(())
    }

    #[test]
    fn a_freeze_with_an_invalid_list_or_an_unknown_field_does_not_decode() -> TestResult {
        let encoded = FrozenRootTools::new(Some(fact(&["web_search"])?)).encode()?;
        let invalid = encoded.replace("web_search", "web_surf");
        assert!(FrozenRootTools::decode(&invalid).is_err());
        let extra = encoded.replacen('{', "{\"extra\":1,", 1);
        assert!(FrozenRootTools::decode(&extra).is_err());
        Ok(())
    }
}
