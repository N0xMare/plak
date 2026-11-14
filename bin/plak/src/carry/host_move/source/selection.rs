//! Membership inspection is valid while actors run; it is never an export cut.
use super::{Inventory, Tree};
use crate::CliResult;
use crate::carry::host_move::Custody;
use crate::shell::services::Services;
use crate::store::{ProductStore, sessions::SessionRecord};
use plakous::sessions::host_move::MoveJobCut;
use plakous::sessions::{SessionBackend, SessionLocator};
use plakous_hosted::sessions::host_move::{MoveGroup, MoveGroups};
use plakous_recursive_subagents::tree::host_move::DormantTree;
use plakous_recursive_subagents::tree::{CustodyRecord, PersistedTree};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Selection {
    pub(crate) groups: Vec<MoveGroup>,
    operation: [u8; 32],
    records: Vec<SessionRecord>,
    trees: BTreeMap<String, (PersistedTree, CustodyRecord)>,
}

impl Selection {
    pub(crate) fn read(services: &Services, operation: [u8; 32]) -> CliResult<Option<Self>> {
        let paths = services.paths()?;
        let records = ProductStore::inspect_upgrade_roots(paths).map_err(super::failure)?;
        if records.is_empty() {
            return Ok(None);
        }
        if records.len() > 4096 {
            return Err(super::failure("too many carried roots"));
        }
        let mut trees = BTreeMap::new();
        let mut members = BTreeMap::<_, BTreeSet<_>>::new();
        for record in &records {
            crate::carry::upgrade::receipt::ensure_open(paths, record)?;
            let source = record
                .plakous_locator
                .ok_or_else(|| super::failure("a carried root has no native binding"))?;
            members
                .entry(record.node.tree_id.clone())
                .or_default()
                .insert(source.session_id());
            if let Some((rows, custody)) =
                plakous_recursive_subagents_store::SqliteTreeStore::inspect_move_tree(
                    &paths.subagents_database(),
                    &record.node.tree_id,
                )
                .map_err(super::failure)?
            {
                for node in rows.rows().nodes().values() {
                    if let Some(locator) = &node.session_locator {
                        let locator: SessionLocator = locator.parse().map_err(super::failure)?;
                        members
                            .entry(record.node.tree_id.clone())
                            .or_default()
                            .insert(locator.session_id());
                    }
                }
                trees.insert(record.node.tree_id.clone(), (rows, custody));
            }
        }
        let ids = members.values().flatten().copied().collect::<BTreeSet<_>>();
        let dependencies = SessionBackend::inspect_move_dependencies_sqlite(
            paths.runtime_database(),
            &ids.into_iter().collect::<Vec<_>>(),
        )
        .map_err(super::failure)?;
        let groups = MoveGroups::plan(
            &plakous::model::hex_lower(&operation),
            &members,
            &dependencies,
        )
        .map_err(super::failure)?;
        Ok(Some(Self {
            operation,
            records,
            trees,
            groups: groups.groups().to_vec(),
        }))
    }

    pub(crate) fn sessions(&self) -> BTreeSet<String> {
        self.groups
            .iter()
            .flat_map(MoveGroup::sessions)
            .map(ToString::to_string)
            .collect()
    }

    pub(crate) fn runners(&self) -> Vec<String> {
        self.records
            .iter()
            .filter_map(|record| {
                record
                    .runner
                    .as_ref()
                    .map(|runner| runner.runner_id.clone())
            })
            .chain(self.trees.values().flat_map(|(tree, _)| {
                tree.rows()
                    .nodes()
                    .values()
                    .filter_map(|node| node.placement.runner_id.clone())
            }))
            .filter(|runner| runner != "local")
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(super) fn joined(self, job_cut: MoveJobCut) -> CliResult<Inventory> {
        let trees = self
            .trees
            .into_iter()
            .map(|(id, (rows, custody))| {
                let cut = DormantTree::capture(rows.rows()).map_err(super::failure)?;
                Ok((
                    id,
                    Tree {
                        cut,
                        custody: Custody {
                            fence: custody.fence,
                            host: custody.holding.host.host_id,
                            instance: custody.holding.host.instance.to_string(),
                            binary: custody.holding.host.binary,
                            machine: custody.holding.host.host_name,
                            claimed_ms: custody.holding.claimed_at_ms,
                            renewed_ms: custody.holding.renewed_at_ms,
                        },
                    },
                ))
            })
            .collect::<CliResult<_>>()?;
        Ok(Inventory {
            groups: self.groups,
            operation: self.operation,
            records: self.records,
            trees,
            job_cut,
        })
    }
}
