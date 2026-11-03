//! The root's optional tools, frozen into its session record when its
//! runtime is first created.
//!
//! Editing the `[root] agent` template, a `[tools.*]` table or an MCP server
//! later never changes a session that already exists: reopen, clear and
//! rebind read only this record, and a rebind onto another tuple keeps it, so
//! canonical tool authority never changes across a rebind. A new session
//! takes the configuration as it is then.

use std::collections::BTreeMap;

use plakous::mcp::McpToolRecord;
use serde::{Deserialize, Serialize};

use crate::binding::RootTemplateFact;
use crate::tool_plan::{NodeToolSelection, TemplateToolSelection};

/// The `[root] agent` template as configured when the session was created on
/// its tuple, and the MCP tools its selection reached then. A session created
/// off the template's tuple froze no template and has no optional tools.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FrozenRootTools {
    template: Option<RootTemplateFact>,
    /// `Some` once the selection's MCP servers were listed at creation.
    mcp_tools: Option<Vec<FrozenMcpTool>>,
}

/// Where an MCP server's process runs: on the host beside the agent, or on
/// the node's runner. Frozen per tool record; every server runs on the host
/// today.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpPlacement {
    /// The host that runs the agent.
    #[default]
    Host,
    /// The runner that hosts the node's workspace.
    Runner,
}

impl McpPlacement {
    /// The spelling config and the frozen record use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Runner => "runner",
        }
    }

    /// Parses [`Self::as_str`]'s spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "host" => Some(Self::Host),
            "runner" => Some(Self::Runner),
            _ => None,
        }
    }
}

/// One MCP tool record frozen with the root's tools, and where its server
/// ran when it was listed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenMcpTool {
    record: McpToolRecord,
    placement: McpPlacement,
}

impl FrozenRootTools {
    /// Freezes `template` as the root's tools; its MCP tools are added by
    /// [`Self::with_mcp_tools`] once listed.
    #[must_use]
    pub fn new(template: Option<RootTemplateFact>) -> Self {
        Self {
            template,
            mcp_tools: None,
        }
    }

    /// The freeze a session created on (`account`, `model`) takes: the
    /// configured template when that is exactly its tuple, and none
    /// otherwise (spec 03 §3).
    #[must_use]
    pub fn at_creation(
        configured: Option<RootTemplateFact>,
        account: Option<&str>,
        model: Option<&str>,
    ) -> Self {
        Self::new(configured.filter(|fact| {
            account == Some(fact.account.as_str()) && model == Some(fact.model.as_str())
        }))
    }

    /// The same freeze with the MCP tools listed at creation, each frozen
    /// with its server's placement: the host, where every server runs today.
    #[must_use]
    pub fn with_mcp_tools(mut self, records: Vec<McpToolRecord>) -> Self {
        self.mcp_tools = Some(
            records
                .into_iter()
                .map(|record| FrozenMcpTool {
                    record,
                    placement: McpPlacement::Host,
                })
                .collect(),
        );
        self
    }

    /// The placement each frozen MCP server ran under when it was listed.
    #[must_use]
    pub fn mcp_placements(&self) -> BTreeMap<&str, McpPlacement> {
        self.mcp_tools
            .iter()
            .flatten()
            .map(|tool| (tool.record.server().as_str(), tool.placement))
            .collect()
    }

    /// The root's tools, whatever tuple the session runs now: a rebind
    /// never changes them (spec 03 §3).
    #[must_use]
    pub fn selection(&self) -> NodeToolSelection {
        let Some(template) = self.template.as_ref() else {
            return NodeToolSelection::default();
        };
        let mut tools = RootTemplateFact::root_tool_selection(
            Some(template),
            Some(&template.account),
            Some(&template.model),
        );
        tools.mcp_tools = self
            .mcp_tools
            .as_ref()
            .map(|frozen| frozen.iter().map(|tool| tool.record.clone()).collect());
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
        let mut placements = BTreeMap::new();
        for tool in wire.mcp_tools.iter().flatten() {
            if *placements
                .entry(tool.record.server().as_str())
                .or_insert(tool.placement)
                != tool.placement
            {
                return Err(
                    "frozen root tools are invalid: one MCP server has two placements".to_owned(),
                );
            }
        }
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
    mcp_tools: Option<Vec<FrozenMcpTool>>,
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
    fn a_freeze_round_trips_and_applies_whatever_tuple_runs() -> TestResult {
        let frozen = FrozenRootTools::at_creation(
            Some(fact(&["fetch_url", "mcp:github"])?),
            Some("codex"),
            Some("m"),
        )
        .with_mcp_tools(Vec::new());
        let decoded = FrozenRootTools::decode(&frozen.encode()?)?;
        assert_eq!(decoded, frozen);
        let tools = decoded.selection();
        assert_eq!(
            tools.selection,
            TemplateToolSelection::parse(["fetch_url", "mcp:github"])?
        );
        assert_eq!(
            tools
                .tool_sections
                .get("tools.fetch_url")
                .map(String::as_str),
            Some("[tools.fetch_url]\nmax_bytes = 4096\n")
        );
        assert_eq!(tools.mcp_tools, Some(Vec::new()));
        let off_tuple =
            FrozenRootTools::at_creation(Some(fact(&["fetch_url"])?), Some("codex"), Some("other"));
        assert_eq!(off_tuple, FrozenRootTools::new(None));
        assert_eq!(off_tuple.selection(), NodeToolSelection::default());
        let unconfigured = FrozenRootTools::decode(&FrozenRootTools::new(None).encode()?)?;
        assert_eq!(unconfigured.selection(), NodeToolSelection::default());
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
