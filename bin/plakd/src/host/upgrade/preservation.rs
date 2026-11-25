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

use self::facts::{Budget, Key};

#[derive(Deserialize, Serialize)]
pub(super) struct Snapshot {
    product: product::Product,
    runtime: Option<runtime::Runtime>,
    trees: trees::Trees,
}

impl Snapshot {
    pub(super) fn capture(
        paths: &PlakPaths,
        before: Option<&Self>,
    ) -> CliResult<(Self, BTreeSet<Key>)> {
        let config =
            crate::product_config::ProductConfig::load_home_file(paths).map_err(CliFailure::new)?;
        check(
            !config.retire_finished_children,
            "cannot verify intentional retirement while retire_finished_children is enabled",
        )?;
        let mut budget = Budget::default();
        // Read the runtime after the queue: a visible removal must have
        // prior durable admission, not merely a delivery view.
        let (product, candidates) =
            product::capture(paths, before.map(|before| &before.product), &mut budget)?;
        let (runtime, accepted) = runtime::capture(paths, candidates, &mut budget)?;
        let trees = trees::capture(paths, &mut budget)?;
        Ok((
            Self {
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
        self.product.compare(&after.product, accepted)?;
        if let Some(before) = &self.runtime {
            before.compare(
                after
                    .runtime
                    .as_ref()
                    .ok_or_else(|| unreadable("runtime store"))?,
            )?;
        }
        self.trees.compare(&after.trees, paths)
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
