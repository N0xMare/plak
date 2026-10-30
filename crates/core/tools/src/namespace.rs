//! One resolved tool namespace.
//!
//! A request shows the model two namespaces at most: the outer one (the
//! provider-visible tools) and, under a program runtime, the nested one (the
//! tools a cell calls). Pack presentations, a runtime's composition markers and
//! orchestration tools all declare themselves here as [`NamespacePresentation`]s:
//! the slots (canonical capabilities) they cover and an optional alternate
//! spelling. Placement is the composition's, not the presentation's: a Direct
//! request places the whole surface outer; a program runtime places its markers
//! outer and every other presentation nested, and its markers also hold their
//! names in the nested namespace. Compiling a surface ([`crate::compile_over`])
//! is the only place a name is decided, and it never shadows: two presentations
//! of different slots under one name move the one that declares an alternate,
//! and anything else fails with a [`NamespaceClash`] naming both providers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use plakous_model::ToolName;

use crate::{ToolRuntimeError, ToolRuntimeErrorKind};

/// Who declared a presentation; a clash names both.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationProvider {
    /// A tool pack, by its pack id (coding packs and the orchestration pack).
    Pack(&'static str),
    /// A program runtime's composition markers, by runtime name.
    Runtime(&'static str),
}

impl fmt::Display for PresentationProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pack(id) => write!(formatter, "pack `{id}`"),
            Self::Runtime(id) => write!(formatter, "runtime `{id}`"),
        }
    }
}

/// One declared presentation: its name, the slots it covers and an optional
/// alternate spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespacePresentation {
    provider: PresentationProvider,
    name: ToolName,
    slots: BTreeSet<ToolName>,
    alternate: Option<ToolName>,
}

impl NamespacePresentation {
    /// Declares one presentation of `slots` (canonical capabilities) under `name`.
    #[must_use]
    pub fn new(
        provider: PresentationProvider,
        name: ToolName,
        slots: impl IntoIterator<Item = ToolName>,
    ) -> Self {
        Self {
            provider,
            name,
            slots: slots.into_iter().collect(),
            alternate: None,
        }
    }

    /// Declares the spelling this presentation takes when another slot holds its name.
    #[must_use]
    pub fn with_alternate(mut self, alternate: ToolName) -> Self {
        self.alternate = Some(alternate);
        self
    }

    /// Returns the declaring provider.
    #[must_use]
    pub const fn provider(&self) -> PresentationProvider {
        self.provider
    }

    /// Returns the declared (primary) name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the declared alternate spelling, if any.
    #[must_use]
    pub const fn alternate(&self) -> Option<&ToolName> {
        self.alternate.as_ref()
    }
}

/// Why two presentations could not share a namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceClashReason {
    /// Different slots, and neither (or, with several, more than one) side
    /// keeps the name without an alternate spelling.
    NoAlternate,
    /// Both sides declare an alternate spelling, so neither owns the name.
    AmbiguousAlternate,
    /// The presentations cover a common slot: one capability presented twice.
    SameSlot,
    /// A declared alternate spelling is itself taken.
    AlternateTaken,
}

/// Construction failed: one namespace holds two presentations under one name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceClash {
    name: ToolName,
    first: PresentationProvider,
    second: PresentationProvider,
    reason: NamespaceClashReason,
}

impl NamespaceClash {
    /// Returns the contested name.
    #[must_use]
    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    /// Returns the provider declared first.
    #[must_use]
    pub const fn first(&self) -> PresentationProvider {
        self.first
    }

    /// Returns the provider declared second.
    #[must_use]
    pub const fn second(&self) -> PresentationProvider {
        self.second
    }

    /// Returns why the clash could not be resolved.
    #[must_use]
    pub const fn reason(&self) -> NamespaceClashReason {
        self.reason
    }

    /// One capability presented twice under `name`: the second registration
    /// was not an explicit claim of the first.
    #[must_use]
    pub const fn same_slot(
        name: ToolName,
        first: PresentationProvider,
        second: PresentationProvider,
    ) -> Self {
        Self {
            name,
            first,
            second,
            reason: NamespaceClashReason::SameSlot,
        }
    }
}

impl fmt::Display for NamespaceClash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            NamespaceClashReason::NoAlternate => "neither declares an alternate spelling",
            NamespaceClashReason::AmbiguousAlternate => {
                "both declare an alternate spelling, so neither owns the name"
            }
            NamespaceClashReason::SameSlot => "they present the same capability twice",
            NamespaceClashReason::AlternateTaken => "the declared alternate spelling is taken",
        };
        write!(
            formatter,
            "tool name `{}` is presented by {} and {} in one namespace; {reason}",
            self.name, self.first, self.second
        )
    }
}

impl std::error::Error for NamespaceClash {}

impl From<NamespaceClash> for ToolRuntimeError {
    fn from(clash: NamespaceClash) -> Self {
        Self::new(ToolRuntimeErrorKind::Registration, clash.to_string()).with_namespace_clash(clash)
    }
}

/// Resolves one namespace holding every given presentation and returns each
/// presentation's name in declaration order.
///
/// A name held by presentations of disjoint slots stays with the one that
/// declares no alternate; every other takes its declared alternate. Alternates
/// do not chain: an alternate that is itself taken fails.
///
/// # Errors
///
/// Returns the first [`NamespaceClash`] in declaration order: a contested name
/// is judged where its second holder is declared, then alternates are judged
/// in the order their holders were declared.
pub fn resolve_namespace(
    presentations: &[NamespacePresentation],
) -> Result<Vec<ToolName>, NamespaceClash> {
    let mut by_name = BTreeMap::<&ToolName, Vec<usize>>::new();
    let mut contested_order = Vec::new();
    for (index, presentation) in presentations.iter().enumerate() {
        let members = by_name.entry(&presentation.name).or_default();
        if members.len() == 1 {
            contested_order.push(&presentation.name);
        }
        members.push(index);
    }
    let mut resolved = presentations
        .iter()
        .map(|presentation| presentation.name.clone())
        .collect::<Vec<_>>();
    for name in contested_order {
        let members = &by_name[name];
        if members.len() < 2 {
            continue;
        }
        let clash = |left: usize, right: usize, reason| NamespaceClash {
            name: name.clone(),
            first: presentations[left].provider,
            second: presentations[right].provider,
            reason,
        };
        for (position, &left) in members.iter().enumerate() {
            for &right in &members[position + 1..] {
                if !presentations[left]
                    .slots
                    .is_disjoint(&presentations[right].slots)
                {
                    return Err(clash(left, right, NamespaceClashReason::SameSlot));
                }
            }
        }
        let keepers = members
            .iter()
            .copied()
            .filter(|&index| presentations[index].alternate.is_none())
            .collect::<Vec<_>>();
        match keepers.as_slice() {
            [] => {
                return Err(clash(
                    members[0],
                    members[1],
                    NamespaceClashReason::AmbiguousAlternate,
                ));
            }
            [_] => {}
            [left, right, ..] => {
                return Err(clash(*left, *right, NamespaceClashReason::NoAlternate));
            }
        }
        for &index in members {
            if let Some(alternate) = &presentations[index].alternate {
                resolved[index] = alternate.clone();
            }
        }
    }
    let mut taken = BTreeMap::<&ToolName, usize>::new();
    for (index, name) in resolved.iter().enumerate() {
        if let Some(&holder) = taken.get(name) {
            return Err(NamespaceClash {
                name: name.clone(),
                first: presentations[holder].provider,
                second: presentations[index].provider,
                reason: NamespaceClashReason::AlternateTaken,
            });
        }
        taken.insert(name, index);
    }
    Ok(resolved)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests;
