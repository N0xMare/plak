//! Product build capabilities; persisted route and composition vocabularies stay complete.

#[cfg(feature = "clipboard")]
mod clipboard;
#[cfg(not(feature = "clipboard"))]
mod clipboard {
    pub(super) const COMPILED: bool = false;
}
#[cfg(feature = "images")]
mod images;
#[cfg(not(feature = "images"))]
mod images {
    pub(super) const COMPILED: bool = false;
}
#[cfg(feature = "mcp")]
mod mcp;
#[cfg(not(feature = "mcp"))]
mod mcp {
    pub(super) const COMPILED: bool = false;
}
#[cfg(feature = "python")]
mod python;
#[cfg(not(feature = "python"))]
mod python {
    pub(super) const COMPILED: bool = false;
}
#[cfg(feature = "tui")]
mod tui;
#[cfg(not(feature = "tui"))]
mod tui {
    pub(super) const COMPILED: bool = false;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityKind {
    Route,
    Evaluator,
    Surface,
    ToolPack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityUnavailable {
    pub id: &'static str,
    pub kind: CapabilityKind,
    pub feature: &'static str,
}

impl std::fmt::Display for CapabilityUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "`{}` is not included in this build (compiled without feature `{}`)",
            self.id, self.feature
        )
    }
}
impl std::error::Error for CapabilityUnavailable {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildCapability {
    pub id: &'static str,
    pub kind: CapabilityKind,
    pub feature: &'static str,
    pub compiled: bool,
}

pub const BUILD_CAPABILITIES: &[BuildCapability] = &[
    BuildCapability {
        id: "clipboard",
        kind: CapabilityKind::Surface,
        feature: "clipboard",
        compiled: clipboard::COMPILED,
    },
    BuildCapability {
        id: "images",
        kind: CapabilityKind::Surface,
        feature: "images",
        compiled: images::COMPILED,
    },
    BuildCapability {
        id: "mcp",
        kind: CapabilityKind::ToolPack,
        feature: "mcp",
        compiled: mcp::COMPILED,
    },
    BuildCapability {
        id: "py-ptc",
        kind: CapabilityKind::Evaluator,
        feature: "python",
        compiled: python::COMPILED,
    },
    BuildCapability {
        id: "tui",
        kind: CapabilityKind::Surface,
        feature: "tui",
        compiled: tui::COMPILED,
    },
];

pub fn require_feature(feature: &str) -> Result<(), CapabilityUnavailable> {
    if let Some(capability) = BUILD_CAPABILITIES
        .iter()
        .find(|entry| entry.feature == feature && !entry.compiled)
    {
        return Err(CapabilityUnavailable {
            id: capability.id,
            kind: capability.kind,
            feature: capability.feature,
        });
    }
    Ok(())
}

/// Admits a composition's worker with the selected label in a refusal.
pub fn require_composition(
    composition: crate::binding::CompositionSelection,
) -> Result<(), CapabilityUnavailable> {
    if composition.requires_python_worker() {
        require_feature("python").map_err(|mut error| {
            error.id = composition.as_str();
            error
        })?;
    }
    Ok(())
}

pub fn omitted_features() -> Vec<&'static str> {
    BUILD_CAPABILITIES
        .iter()
        .filter(|entry| !entry.compiled)
        .map(|entry| entry.feature)
        .collect()
}

pub fn compiled_features() -> Vec<&'static str> {
    let mut features: Vec<_> = super::ROUTES
        .iter()
        .map(|route| route.account)
        .chain(
            BUILD_CAPABILITIES
                .iter()
                .filter(|entry| entry.compiled)
                .map(|entry| entry.feature),
        )
        .collect();
    features.sort_unstable();
    features
}
