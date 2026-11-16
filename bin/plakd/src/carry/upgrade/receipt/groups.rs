//! Connected-group authority shares the receipt's atomic surrender and Ready writes.
use super::{CliFailure, CliResult, Phase, Receipt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerTransfer {
    pub(crate) from: Option<plakous::runner::protocol::LineOwner>,
    pub(crate) to: plakous::runner::protocol::LineOwner,
    pub(crate) previous: Vec<plakous::runner::protocol::LineOwner>,
}

impl OwnerTransfer {
    fn valid_history(&self, line: &str) -> bool {
        self.previous.len() <= 64
            && self.previous.iter().enumerate().all(|(index, owner)| {
                owner.line == line
                    && owner.generation == self.to.generation
                    && owner.branch == self.to.branch
                    && owner.fence != 0
                    && self.to.is_newer_than(owner)
                    && !self.previous[..index].contains(owner)
            })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GroupPhase {
    Pending,
    Staged,
    Published,
    Abandoning,
    Ready,
    Abandoned,
}

/// Identities and digests only: all reconstruction data remains in the v13 capsule.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Group {
    pub(crate) id: String,
    pub(crate) operation: [u8; 32],
    pub(crate) trees: BTreeSet<String>,
    pub(crate) products: BTreeSet<String>,
    pub(crate) sessions: BTreeSet<String>,
    pub(crate) lines: BTreeSet<String>,
    pub(crate) runners: BTreeSet<String>,
    pub(crate) source_digest: Option<String>,
    pub(crate) source_retired: bool,
    pub(crate) target_digest: Option<String>,
    pub(crate) phase: GroupPhase,
    pub(crate) abandon_error: Option<String>,
    pub(crate) abandon_retry_after: Option<u64>,
    pub(crate) transfers: BTreeMap<String, OwnerTransfer>,
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid() -> CliFailure {
    CliFailure::new("host_upgrade_group_invalid: connected-group authority cannot be verified")
}

impl Receipt {
    pub(super) fn validate_groups(&self) -> CliResult<()> {
        let mut groups = BTreeSet::new();
        let mut operations = BTreeSet::new();
        let mut trees = BTreeSet::new();
        let mut products = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        let mut lines = BTreeSet::new();
        for group in &self.groups {
            let source_digest = group.source_digest_value()?;
            let target_digest = group.digest()?;
            if !group.id.strip_prefix("group-").is_some_and(digest)
                || !groups.insert(&group.id)
                || !operations.insert(group.operation)
                || group.operation == [0; 32]
                || group.trees.is_empty()
                || group.products.is_empty()
                || group.sessions.is_empty()
                || group.lines.is_empty()
                || group.runners.is_empty()
                || group.trees.len() > 4096
                || group.products.len() > 4096
                || group.sessions.len() > 1024
                || group.lines.len() > 16384
                || group.runners.len() > 256
                || group
                    .abandon_error
                    .as_ref()
                    .is_some_and(|error| error.len() > 2048)
                || group
                    .runners
                    .iter()
                    .any(|runner| runner.is_empty() || runner.len() > 256)
                || (!matches!(self.phase, Phase::Prepared | Phase::Aborted)
                    && source_digest.is_none())
                || (!matches!(
                    group.phase,
                    GroupPhase::Pending | GroupPhase::Abandoning | GroupPhase::Abandoned
                ) && target_digest.is_none())
                || (group.phase == GroupPhase::Ready && !group.source_retired)
                || (matches!(self.phase, Phase::Prepared | Phase::Aborted)
                    && (group.phase != GroupPhase::Pending || group.source_retired))
            {
                return Err(invalid());
            }
            for (line, transfer) in &group.transfers {
                if !group.lines.contains(line)
                    || transfer.to.line != *line
                    || transfer.to.generation == 0
                    || transfer.to.fence == 0
                    || transfer.to.fence == u64::MAX
                    || uuid::Uuid::parse_str(&transfer.to.branch).is_err()
                    || transfer.from.as_ref().is_some_and(|from| {
                        from.line != *line
                            || from.generation == 0
                            || from.fence == 0
                            || from.fence == u64::MAX
                            || uuid::Uuid::parse_str(&from.branch).is_err()
                            || !transfer.to.is_at_least(from)
                    })
                    || matches!(group.phase, GroupPhase::Pending | GroupPhase::Staged)
                    || !transfer.valid_history(line)
                {
                    return Err(invalid());
                }
            }
            for (members, inventory) in [
                (&group.trees, &mut trees),
                (&group.products, &mut products),
                (&group.sessions, &mut sessions),
                (&group.lines, &mut lines),
            ] {
                for member in members {
                    if uuid::Uuid::parse_str(member).is_err() || !inventory.insert(member) {
                        return Err(invalid());
                    }
                }
            }
            for product in &group.products {
                let mapping = self
                    .mappings
                    .iter()
                    .find(|mapping| &mapping.source == product)
                    .ok_or_else(invalid)?;
                if mapping.source != mapping.successor
                    || !group.trees.contains(&mapping.source_node.tree_id)
                    || mapping.ready != (group.phase == GroupPhase::Ready)
                    || mapping.abandoned != (group.phase == GroupPhase::Abandoned)
                {
                    return Err(invalid());
                }
            }
        }
        if !self.groups.is_empty()
            && (self.groups.len() > 4096 || products.len() != self.mappings.len())
        {
            return Err(invalid());
        }
        Ok(())
    }
}

impl Group {
    #[cfg(any(feature = "web", test))]
    pub(crate) fn abandon_retry_due(&self) -> bool {
        self.abandon_retry_after
            .is_none_or(|deadline| now() >= deadline)
    }

    pub(crate) fn source_digest_value(&self) -> CliResult<Option<plakous::model::Digest>> {
        decode_digest(self.source_digest.as_deref())
    }

    pub(crate) fn digest(&self) -> CliResult<Option<plakous::model::Digest>> {
        decode_digest(self.target_digest.as_deref())
    }
}

#[cfg(any(feature = "web", test))]
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn decode_digest(value: Option<&str>) -> CliResult<Option<plakous::model::Digest>> {
    value
        .map(|value| {
            let mut bytes = [0_u8; 32];
            if !digest(value) {
                return Err(invalid());
            }
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                    .map_err(|_| invalid())?;
            }
            plakous::model::Digest::from_sha256_bytes(plakous::model::DigestDomain::Blob, 1, bytes)
                .map_err(|_| invalid())
        })
        .transpose()
}

#[cfg(test)]
mod tests;
