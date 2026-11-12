//! Cross-key ownership receipts contain identities, never carried session encodings.

use rustix::fs::Mode;
use std::fs::{self, File};
use std::io::Read as _;
#[cfg(any(test, feature = "web"))]
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::home::paths::PlakPaths;
use crate::store::sessions::{SessionNode, SessionRecord};
use crate::{CliFailure, CliResult};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Prepared,
    Committed,
    Acknowledged,
    Aborted,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Mapping {
    pub(crate) source: String,
    pub(crate) source_node: plakous_host_protocol::wire::NodeAddress,
    pub(crate) successor: String,
    pub(crate) digest: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub(crate) version: u32,
    pub(crate) id: uuid::Uuid,
    pub(crate) sequence: u64,
    pub(crate) source_key: String,
    pub(crate) source_instance: String,
    pub(crate) source_binary: String,
    pub(crate) source_executable: PathBuf,
    pub(crate) host_argv: Vec<String>,
    pub(crate) destination_key: String,
    pub(crate) destination_binary: String,
    pub(crate) destination: PathBuf,
    pub(crate) phase: Phase,
    pub(crate) mappings: Vec<Mapping>,
}

impl Receipt {
    pub(crate) fn validate(&self) -> CliResult<()> {
        let uuid = |value: &str| uuid::Uuid::parse_str(value).is_ok();
        let mut sources = std::collections::BTreeSet::new();
        let mut successors = std::collections::BTreeSet::new();
        if self.version != 1
            || self.sequence == 0
            || plakous_recursive_subagents::tree::HostInstance::parse(&self.source_instance)
                .is_err()
            || !super::state_key_valid(&self.source_key)
            || !super::state_key_valid(&self.destination_key)
            || self.source_key == self.destination_key
            || !super::binary_id_valid(&self.source_binary)
            || !super::binary_id_valid(&self.destination_binary)
            || !self.destination.is_absolute()
            || !self.source_executable.is_absolute()
            || self
                .host_argv
                .first()
                .is_none_or(|argument| argument != "host")
            || self.host_argv.len() > 64
            || self.mappings.len() > 4096
            || self.mappings.iter().any(|mapping| {
                !uuid(&mapping.source)
                    || !uuid(&mapping.successor)
                    || SessionNode::parse(
                        &mapping.source_node.tree_id,
                        &mapping.source_node.agent_id,
                    )
                    .is_err()
                    || mapping.digest.len() != 64
                    || !mapping
                        .digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    || !sources.insert(&mapping.source)
                    || !successors.insert(&mapping.successor)
            })
        {
            return Err(CliFailure::new(
                "host_upgrade_receipt_invalid: upgrade authority cannot be verified",
            ));
        }
        Ok(())
    }

    pub(crate) fn path(&self, paths: &PlakPaths) -> PathBuf {
        root(paths).join(self.id.to_string()).join("receipt.json")
    }

    /// A publication error is outcome-unknown: after attempting a commit the
    /// caller must keep the source fenced, never rewrite this as an abort.
    #[cfg(any(test, feature = "web"))]
    pub(crate) fn save(&self, paths: &PlakPaths) -> CliResult<()> {
        self.validate()?;
        let directory = root(paths).join(self.id.to_string());
        crate::commands::runner::private_directory(&paths.home().join("host"))?;
        crate::commands::runner::private_directory(&root(paths))?;
        crate::commands::runner::private_directory(&directory)?;
        let bytes = serde_json::to_vec(self).map_err(|error| CliFailure::new(error.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(CliFailure::new(
                "host upgrade receipt exceeds its size bound",
            ));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(failed)?;
        temporary.write_all(&bytes).map_err(failed)?;
        temporary.as_file().sync_all().map_err(failed)?;
        temporary
            .persist(self.path(paths))
            .map_err(|error| failed(error.error))?;
        File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(failed)?;
        File::open(root(paths))
            .and_then(|file| file.sync_all())
            .map_err(failed)?;
        File::open(paths.home().join("host"))
            .and_then(|file| file.sync_all())
            .map_err(failed)?;
        File::open(paths.home())
            .and_then(|file| file.sync_all())
            .map_err(failed)
    }
}

fn failed(error: std::io::Error) -> CliFailure {
    CliFailure::new(format!("host_upgrade_receipt_io: {error}"))
}

pub(crate) fn root(paths: &PlakPaths) -> PathBuf {
    paths.home().join("host/upgrades")
}

pub(crate) fn read(path: &Path) -> CliResult<Receipt> {
    use rustix::fs::{FileType, OFlags};
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| failed(error.into()))?;
    let stat = rustix::fs::fstat(&descriptor).map_err(|error| failed(error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO)
        || stat.st_size < 0
        || u64::try_from(stat.st_size).unwrap_or(u64::MAX) > MAX_BYTES
    {
        return Err(CliFailure::new(
            "host_upgrade_receipt_invalid: expected a bounded private owned regular file",
        ));
    }
    let mut bytes = Vec::new();
    File::from(descriptor)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(CliFailure::new(
            "host upgrade receipt exceeds its size bound",
        ));
    }
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|error| CliFailure::new(format!("host_upgrade_receipt_invalid: {error}")))?;
    receipt.validate()?;
    Ok(receipt)
}

pub(crate) fn all(paths: &PlakPaths) -> CliResult<Vec<Receipt>> {
    use std::os::unix::fs::MetadataExt as _;
    let directory = root(paths);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(failed(error)),
    };
    let private = |metadata: &fs::Metadata| {
        metadata.is_dir()
            && !metadata.is_symlink()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && !Mode::from_raw_mode(metadata.mode()).intersects(Mode::RWXG | Mode::RWXO)
    };
    if !private(&metadata) {
        return Err(CliFailure::new(
            "host upgrade authority directory is not private and owned",
        ));
    }
    let mut receipts = Vec::new();
    for entry in fs::read_dir(directory).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|name| uuid::Uuid::parse_str(name).is_ok())
        else {
            continue;
        };
        if !private(&fs::symlink_metadata(entry.path()).map_err(failed)?) {
            return Err(CliFailure::new(
                "host upgrade transaction directory is not private and owned",
            ));
        }
        let path = entry.path().join("receipt.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(failed(error)),
            Ok(_) => {}
        }
        let receipt = read(&path)?;
        if receipt.id.to_string() != name {
            return Err(CliFailure::new(
                "host upgrade receipt identity disagrees with its directory",
            ));
        }
        receipts.push(receipt);
    }
    receipts.sort_by_key(|receipt| receipt.sequence);
    if receipts
        .windows(2)
        .any(|pair| pair[0].sequence == pair[1].sequence)
    {
        return Err(CliFailure::new("host upgrade receipt sequences conflict"));
    }
    Ok(receipts)
}

/// Source and target opens consult the same durable commit. Prepared and
/// aborted target seeds are never execution authority.
pub(crate) fn ensure_open(paths: &PlakPaths, record: &SessionRecord) -> CliResult<()> {
    let key = crate::home::state::state_key();
    for receipt in all(paths)? {
        for mapping in &receipt.mappings {
            if key == receipt.source_key
                && mapping.source_node.tree_id == record.node.tree_id
                && matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
            {
                return Err(CliFailure::new(format!(
                    "host_upgrade_source_fenced: session {} is reserved for successor {} under {}; retry the host upgrade to resolve its receipt",
                    record.id, mapping.successor, receipt.destination_key,
                )));
            }
            if key == receipt.destination_key
                && mapping.successor == record.id
                && !matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
            {
                return Err(CliFailure::new(
                    "host_upgrade_target_pending: this dormant seed has no committed source surrender",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "web")]
pub(crate) fn successor_key(paths: &PlakPaths, source: &str) -> CliResult<String> {
    let mut key = source.to_owned();
    for receipt in all(paths)? {
        if matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
            && receipt.source_key == key
        {
            key = receipt.destination_key;
        }
    }
    Ok(key)
}

pub(crate) fn withheld_targets(paths: &PlakPaths) -> CliResult<std::collections::BTreeSet<String>> {
    let key = crate::home::state::state_key();
    Ok(all(paths)?
        .into_iter()
        .filter(|receipt| {
            receipt.destination_key == key
                && !matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
        })
        .flat_map(|receipt| {
            receipt
                .mappings
                .into_iter()
                .map(|mapping| mapping.successor)
        })
        .collect())
}

#[cfg(feature = "web")]
pub(crate) fn successor_node(
    paths: &PlakPaths,
    source: &plakous_host_protocol::wire::NodeAddress,
) -> CliResult<Option<plakous_host_protocol::wire::NodeAddress>> {
    let mut node = source.clone();
    let mut destination_key = None;
    for receipt in all(paths)? {
        if !matches!(receipt.phase, Phase::Committed | Phase::Acknowledged) {
            continue;
        }
        if let Some(mapping) = receipt.mappings.iter().find(|mapping| {
            mapping.source_node == node
                || mapping.source == node.tree_id && mapping.source == node.agent_id
        }) {
            node = plakous_host_protocol::wire::NodeAddress {
                tree_id: mapping.successor.clone(),
                agent_id: mapping.successor.clone(),
            };
            destination_key = Some(receipt.destination_key);
        }
    }
    Ok((node != *source
        && destination_key.as_deref() == Some(crate::home::state::state_key().as_str()))
    .then_some(node))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated receipt fixtures")]
mod tests;
