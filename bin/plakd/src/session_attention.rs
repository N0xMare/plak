use std::io::{self, Write};

use plakous_hosted::runtime::ResumeAfterLoss;
use plakous_hosted::runtime::operator_attention::PendingTreeWork;
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection};

use crate::home::paths::PlakPaths;
use crate::product_config::{ProductConfig, ResumeAfterLoss as ConfiguredPolicy};
use crate::store::ProductStore;

pub(crate) fn policy(config: &ProductConfig) -> ResumeAfterLoss {
    if config.resume_after_loss == Some(ConfiguredPolicy::Continue) {
        ResumeAfterLoss::Continue
    } else {
        ResumeAfterLoss::Hold
    }
}

pub(super) fn write_status(paths: &PlakPaths, out: &mut dyn Write) -> io::Result<()> {
    let inspection = ProductConfig::load_home_file(paths)
        .map_err(|_| ())
        .and_then(|config| {
            let policy = policy(&config);
            ProductStore::inspect_attention_roots(paths, policy)
                .map(|roots| (policy, roots))
                .map_err(|_| ())
        });
    let Ok((policy, roots)) = inspection else {
        return writeln!(
            out,
            "Session attention unavailable (could not read local session state)\n"
        );
    };
    let mut printed = false;
    let mut unreadable = 0;
    for root in roots {
        let work = match SqliteTreeStore::inspect(&paths.subagents_database(), &root.tree) {
            Ok(TreeInspection::Tree(tree)) => PendingTreeWork::read(tree.rows(), policy),
            Ok(TreeInspection::StoreAbsent | TreeInspection::TreeAbsent) => {
                PendingTreeWork::default()
            }
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if let Some(attention) = work.attention(root.reason, 0, root.prepared_wakes) {
            if !printed {
                writeln!(out, "Sessions needing operator attention")?;
                printed = true;
            }
            writeln!(
                out,
                "  {} · {}; attach to this session first",
                root.session,
                attention.detail()
            )?;
        }
    }
    if unreadable > 0 {
        writeln!(
            out,
            "Session attention incomplete ({unreadable} trees could not be read)"
        )?;
    }
    if printed || unreadable > 0 {
        writeln!(out)?;
    }
    Ok(())
}
