//! Read-only qualification of the frozen bindings reachable from a tree.

use crate::home::paths::PlakPaths;
use plakous::agent::BindingChange;
use plakous::sessions::{SessionLocator, rehearsal};
use plakous_recursive_subagents_store::SqliteTreeStore;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub(crate) struct ChildReopenRefusal {
    affected_children: usize,
    binding_change: Option<String>,
    unavailable: Option<String>,
}

impl ChildReopenRefusal {
    pub(crate) fn record(&mut self, change: Option<&str>, unavailable: Option<&str>) {
        self.affected_children += 1;
        self.binding_change = self.binding_change.take().or_else(|| change.map(bounded));
        self.unavailable = self.unavailable.take().or_else(|| unavailable.map(bounded));
    }

    pub(crate) fn into_report(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.affected_children == 0
    }
}

impl fmt::Display for ChildReopenRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "host_upgrade_child_reopen_refused: {} affected child session(s)",
            self.affected_children
        )?;
        if let Some(change) = &self.binding_change {
            write!(out, "; binding change: {change}")?;
        }
        if let Some(unavailable) = &self.unavailable {
            write!(out, "; unavailable: {unavailable}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Fault {
    Binding { detail: String },
    EvaluatorContract { detail: String },
    Unreadable { detail: String },
    ArtifactMissing,
    ArtifactBytesMismatch,
    ArtifactUnavailable,
}

impl Fault {
    fn pre_existing_kind(&self) -> FaultKind {
        match self {
            Self::Binding { .. } | Self::EvaluatorContract { .. } => FaultKind::BindingChange,
            Self::Unreadable { .. } => FaultKind::BindingUnreadable,
            Self::ArtifactMissing => FaultKind::EvaluatorArtifactMissing,
            Self::ArtifactBytesMismatch => FaultKind::EvaluatorArtifactBytesMismatch,
            Self::ArtifactUnavailable => FaultKind::EvaluatorArtifactUnavailable,
        }
    }

    fn environmental(&self) -> bool {
        matches!(
            self,
            Self::ArtifactMissing | Self::ArtifactBytesMismatch | Self::ArtifactUnavailable
        )
    }

    fn record_refusal(&self, refused: &mut ChildReopenRefusal) {
        match self {
            Self::Binding { detail } | Self::EvaluatorContract { detail } => {
                refused.record(Some(detail), None);
            }
            Self::Unreadable { detail } => refused.record(None, Some(detail)),
            Self::ArtifactMissing => {
                refused.record(None, Some("recorded evaluator artifact is missing"));
            }
            Self::ArtifactBytesMismatch => {
                refused.record(None, Some("recorded evaluator artifact bytes do not match"));
            }
            Self::ArtifactUnavailable => {
                refused.record(None, Some("recorded evaluator artifact is unavailable"));
            }
        }
    }
}

// The private probe compares only exact child heads. The operator sees counts
// and categories, never locators or filesystem paths.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ChildReopenInspection {
    opened: BTreeSet<String>,
    faults: Vec<(Option<String>, Fault)>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum FaultKind {
    BindingChange,
    BindingUnreadable,
    EvaluatorArtifactMissing,
    EvaluatorArtifactBytesMismatch,
    EvaluatorArtifactUnavailable,
}

impl FaultKind {
    const fn label(self) -> &'static str {
        match self {
            Self::BindingChange => "binding_change",
            Self::BindingUnreadable => "binding_unreadable",
            Self::EvaluatorArtifactMissing => "evaluator_artifact_missing",
            Self::EvaluatorArtifactBytesMismatch => "evaluator_artifact_bytes_mismatch",
            Self::EvaluatorArtifactUnavailable => "evaluator_artifact_unavailable",
        }
    }
}

#[derive(Default, Serialize)]
pub(crate) struct ChildReopenQualification {
    pub(crate) regressions: ChildReopenRefusal,
    pub(crate) pre_existing: PreExistingChildFaults,
}

#[derive(Default, Serialize)]
pub(crate) struct PreExistingChildFaults {
    affected_children: usize,
    by_kind: BTreeMap<FaultKind, usize>,
}

impl PreExistingChildFaults {
    fn record(&mut self, kind: FaultKind) {
        self.affected_children += 1;
        *self.by_kind.entry(kind).or_default() += 1;
    }
}

impl fmt::Display for PreExistingChildFaults {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "host_upgrade_child_reopen_pre_existing: {} affected child session(s)",
            self.affected_children
        )?;
        for (kind, count) in &self.by_kind {
            write!(out, "; {}={count}", kind.label())?;
        }
        Ok(())
    }
}

impl ChildReopenInspection {
    pub(super) fn into_faults(mut self) -> Option<Self> {
        self.opened.clear();
        (!self.faults.is_empty()).then_some(self)
    }

    /// Without a source baseline every build-attributable fault refuses;
    /// environmental artifact faults are reported as pre-existing.
    pub(crate) fn qualify(&self) -> ChildReopenQualification {
        let mut report = ChildReopenQualification::default();
        for (head, fault) in &self.faults {
            if head.is_none() || !fault.environmental() {
                fault.record_refusal(&mut report.regressions);
            } else {
                report.pre_existing.record(fault.pre_existing_kind());
            }
        }
        report
    }

    pub(super) fn strict(&self) -> Option<ChildReopenRefusal> {
        let mut refused = ChildReopenRefusal::default();
        for (_, fault) in &self.faults {
            fault.record_refusal(&mut refused);
        }
        refused.into_report()
    }
}

pub(crate) fn inspect(paths: &PlakPaths, tree_id: &str) -> Result<ChildReopenInspection, String> {
    use rehearsal::{ChildBindingInspectionError, EvaluatorArtifactFault};

    let nodes = SqliteTreeStore::inspect_reopen_children(&paths.subagents_database(), tree_id)
        .map_err(|error| error.message().to_owned())?;
    let mut locators = Vec::with_capacity(nodes.len());
    let mut report = ChildReopenInspection::default();
    for locator in nodes {
        match locator.parse::<SessionLocator>() {
            Ok(locator) => locators.push(locator),
            Err(_) => report.faults.push((
                None,
                Fault::Unreadable {
                    detail: "child session locator is invalid".into(),
                },
            )),
        }
    }
    let executor_cut = crate::test_hooks::child_binding_cut();
    let unreadable = crate::test_hooks::child_binding_unreadable();
    rehearsal::inspect_child_bindings_sqlite(
        &paths.runtime_database(),
        &locators,
        |head, child| {
            let fault = match child {
                Ok(frozen) => match plakous_hosted::agent::check_frozen_child_binding(&frozen) {
                    Ok(None) if unreadable => Some(Fault::Unreadable {
                        detail: "child binding is unreadable in this image".into(),
                    }),
                    Ok(None) if executor_cut => Some(Fault::Binding {
                        detail: "executor binding for child tool".into(),
                    }),
                    Ok(change) => change.map(|change| Fault::Binding {
                        detail: bounded(&change.to_string()),
                    }),
                    Err(error) => Some(Fault::Unreadable {
                        detail: bounded(error.message()),
                    }),
                },
                Err(ChildBindingInspectionError::EvaluatorArtifact(fault)) => Some(match fault {
                    EvaluatorArtifactFault::Missing => Fault::ArtifactMissing,
                    EvaluatorArtifactFault::BytesMismatch => Fault::ArtifactBytesMismatch,
                    EvaluatorArtifactFault::Unavailable => Fault::ArtifactUnavailable,
                }),
                Err(ChildBindingInspectionError::EvaluatorContract(error)) => {
                    Some(Fault::EvaluatorContract {
                        detail: bounded(&format!("execution environment: {error}")),
                    })
                }
                Err(ChildBindingInspectionError::Binding(error)) => {
                    Some(match error.binding_change() {
                        Some(BindingChange::ExecutionEnvironment) => Fault::Binding {
                            detail: bounded(&format!("execution environment: {error}")),
                        },
                        Some(change) => Fault::Binding {
                            detail: bounded(&change.to_string()),
                        },
                        None => Fault::Unreadable {
                            detail: bounded(&error.to_string()),
                        },
                    })
                }
            };
            if let Some(fault) = fault {
                report
                    .faults
                    .push((head.map(|head| head.to_string()), fault));
            } else if let Some(head) = head {
                report.opened.insert(head.to_string());
            }
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(report)
}

fn bounded(message: &str) -> String {
    crate::cli_output::safe_terminal_block(&message.chars().take(512).collect::<String>())
}

#[cfg(test)]
mod tests;
