//! Read-only qualification of the frozen bindings reachable from a tree.

use crate::home::paths::PlakPaths;
use plakous::{agent::BindingChange, sessions::SessionLocator};
use plakous_recursive_subagents_store::SqliteTreeStore;
use serde::{Deserialize, Serialize};
use std::fmt;

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
        (self.affected_children != 0).then_some(self)
    }

    fn note_unavailable(&mut self, message: &str) {
        self.record(None, Some(message));
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.affected_children += other.affected_children;
        self.binding_change = self.binding_change.take().or(other.binding_change);
        self.unavailable = self.unavailable.take().or(other.unavailable);
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

pub(super) fn inspect(
    paths: &PlakPaths,
    tree_id: &str,
) -> Result<Option<ChildReopenRefusal>, String> {
    let nodes = SqliteTreeStore::inspect_reopen_children(&paths.subagents_database(), tree_id)
        .map_err(|error| error.message().to_owned())?;
    let mut locators = Vec::with_capacity(nodes.len());
    let mut refused = ChildReopenRefusal::default();
    for locator in nodes {
        match locator.parse::<SessionLocator>() {
            Ok(locator) => locators.push(locator),
            Err(_) => refused.note_unavailable("child session locator is invalid"),
        }
    }
    let executor_cut = crate::test_hooks::child_binding_cut();
    plakous::sessions::rehearsal::inspect_child_bindings_sqlite(
        &paths.runtime_database(),
        &locators,
        |child| {
            let change = match child {
                Ok(frozen) => match plakous_hosted::agent::check_frozen_child_binding(&frozen) {
                    Ok(None) if executor_cut => Some("executor binding for child tool".into()),
                    Ok(change) => change.map(|change| change.to_string()),
                    Err(error) => {
                        refused.note_unavailable(error.message());
                        return;
                    }
                },
                Err(error) => match error.binding_change() {
                    Some(BindingChange::ExecutionEnvironment) => {
                        Some(format!("execution environment: {error}"))
                    }
                    Some(change) => Some(change.to_string()),
                    None => {
                        refused.note_unavailable(&error.to_string());
                        return;
                    }
                },
            };
            if let Some(change) = change {
                refused.record(Some(&change), None);
            }
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(refused.into_report())
}

fn bounded(message: &str) -> String {
    crate::cli_output::safe_terminal_block(&message.chars().take(512).collect::<String>())
}
