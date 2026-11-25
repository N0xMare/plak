//! Read-only preservation across bounded, individually pinned SQLite snapshots.

mod facts;
mod product;
mod runtime;
#[cfg(test)]
mod tests;
mod trees;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

pub(super) use self::facts::CAPTURE_TIME;
use self::facts::{Budget, Key};

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::host) enum Evidence {
    Held,
    BaselinePredatesDrain,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Snapshot {
    pub(super) evidence: Evidence,
    product: product::Product,
    runtime: Option<runtime::Runtime>,
    trees: trees::Trees,
}

impl Snapshot {
    pub(super) fn capture(
        paths: &PlakPaths,
        before: Option<&Self>,
    ) -> CliResult<(Self, BTreeSet<Key>)> {
        Self::capture_at(paths, before, before.map_or(Evidence::Held, |s| s.evidence))
    }

    pub(super) fn capture_at(
        paths: &PlakPaths,
        before: Option<&Self>,
        evidence: Evidence,
    ) -> CliResult<(Self, BTreeSet<Key>)> {
        let config =
            crate::product_config::ProductConfig::load_home_file(paths).map_err(CliFailure::new)?;
        check(
            evidence == Evidence::BaselinePredatesDrain || !config.retire_finished_children,
            "cannot verify intentional retirement while retire_finished_children is enabled",
        )?;
        let mut budget = Budget::default();
        // Read the runtime after the queue: a visible removal must have
        // prior durable admission, not merely a delivery view.
        let (product, candidates) = product::capture(
            paths,
            before.map(|before| &before.product),
            evidence,
            &mut budget,
        )?;
        let (runtime, accepted) = runtime::capture(paths, candidates, evidence, &mut budget)?;
        let trees = trees::capture(paths, evidence, &mut budget)?;
        Ok((
            Self {
                evidence,
                product,
                runtime,
                trees,
            },
            accepted,
        ))
    }

    pub(super) fn compare(
        &self,
        after: &Self,
        accepted: &BTreeSet<Key>,
        paths: &PlakPaths,
    ) -> CliResult<Option<String>> {
        let recorded_deletes = after.runtime.as_ref().map(runtime::Runtime::deleted);
        self.product
            .compare(&after.product, accepted, self.evidence, |id| {
                recorded_deletes.is_some_and(|deleted| deleted.contains_key(id))
            })?;
        if let Some(before) = &self.runtime {
            before.compare(
                after
                    .runtime
                    .as_ref()
                    .ok_or_else(|| unreadable("runtime store"))?,
                self.evidence,
            )?;
        }
        self.trees.compare(
            &after.trees,
            paths,
            self.evidence,
            &after.product.deleted_trees,
        )
    }
}

fn check(condition: bool, message: &str) -> CliResult<()> {
    if condition {
        Ok(())
    } else {
        Err(fail(message))
    }
}

fn fail(message: &str) -> CliFailure {
    CliFailure::new(format!("host_upgrade_preservation_refused: {message}"))
}

fn unreadable(scope: &str) -> CliFailure {
    fail(&format!("unreadable or corrupt {scope} preservation facts"))
}
