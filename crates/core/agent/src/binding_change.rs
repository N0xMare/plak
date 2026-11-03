//! The frozen binding input an exact reopen found different in this build.

use std::fmt;

/// One frozen binding input that an exact reopen reconstructs differently.
///
/// A session freezes its model, route, adapter, tool and environment facts at
/// creation. When the build that reopens it derives a different value for one
/// of them, the reopen refuses and names which one; identifiers are build and
/// catalog constants, never credentials or conversation content.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BindingChange {
    /// The model protocol differs.
    Protocol {
        /// The recorded protocol.
        recorded: String,
        /// The protocol this build derives.
        current: String,
    },
    /// The provider route differs.
    Route {
        /// The recorded route.
        recorded: String,
        /// The route this build derives.
        current: String,
    },
    /// The requested model differs.
    Model {
        /// The recorded model.
        recorded: String,
        /// The model this build derives.
        current: String,
    },
    /// The model adapter profile differs.
    AdapterProfile {
        /// The recorded adapter profile.
        recorded: String,
        /// The adapter profile this build derives.
        current: String,
    },
    /// The model's capability binding (adapter revision, declared features or
    /// context contract) differs.
    ModelCapabilities,
    /// The selected or effective provider configuration differs.
    ProviderConfiguration,
    /// One tool's executor binding identity differs.
    ToolBinding {
        /// The canonical tool executor identity.
        tool: String,
    },
    /// The tool presentation (descriptions, exposure or projection) differs.
    ToolPresentation,
    /// The frozen tool registry differs.
    ToolRegistry,
    /// The stable execution-environment facts differ.
    ExecutionEnvironment,
    /// The runner the session's tools run on differs. Every rebind of a line
    /// keeps its runner.
    Runner {
        /// The recorded runner.
        recorded: String,
        /// The runner this build derives.
        current: String,
    },
}

fn pair(
    formatter: &mut fmt::Formatter<'_>,
    what: &str,
    recorded: &str,
    current: &str,
) -> fmt::Result {
    write!(
        formatter,
        "{what} (recorded `{recorded}`, this build `{current}`)"
    )
}

impl fmt::Display for BindingChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol { recorded, current } => {
                pair(formatter, "model protocol", recorded, current)
            }
            Self::Route { recorded, current } => pair(formatter, "route", recorded, current),
            Self::Model { recorded, current } => pair(formatter, "model", recorded, current),
            Self::AdapterProfile { recorded, current } => {
                pair(formatter, "model adapter profile", recorded, current)
            }
            Self::ModelCapabilities => formatter.write_str(
                "model capability binding (adapter revision, declared features or context contract)",
            ),
            Self::ProviderConfiguration => formatter.write_str("provider configuration"),
            Self::ToolBinding { tool } => write!(formatter, "executor binding for tool `{tool}`"),
            Self::ToolPresentation => formatter.write_str("tool presentation"),
            Self::ToolRegistry => formatter.write_str("tool registry"),
            Self::ExecutionEnvironment => formatter.write_str("execution environment"),
            Self::Runner { recorded, current } => pair(formatter, "runner", recorded, current),
        }
    }
}
