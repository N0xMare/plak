//! Refuse unresolved old-format ownership records without recovering them.

use std::fs::{self, File};
use std::io::{ErrorKind, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use serde::Deserialize;

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliFailureKind, CliResult};

const MAX_RECEIPT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct ReceiptState {
    phase: Phase,
    mappings: Vec<Mapping>,
}

#[derive(Deserialize)]
struct Mapping {
    abandoned: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Committed,
    Aborted,
    Acknowledged,
}

pub(super) fn refuse_unresolved(paths: &PlakPaths) -> CliResult<()> {
    let host = paths.home().join("host");
    let upgrades = host.join("upgrades");
    if !private_directory_if_present(&host)? || !private_directory_if_present(&upgrades)? {
        return Ok(());
    }
    for entry in fs::read_dir(upgrades).map_err(|_| invalid_receipt())? {
        let entry = entry.map_err(|_| invalid_receipt())?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| uuid::Uuid::parse_str(name).ok())
        else {
            continue;
        };
        if private_directory_if_present(&entry.path())?
            && read_state(&entry.path().join("receipt.json"))?.is_some_and(|receipt| {
                matches!(receipt.phase, Phase::Prepared | Phase::Committed)
                    || matches!(receipt.phase, Phase::Acknowledged)
                        && receipt.mappings.iter().any(|mapping| mapping.abandoned)
            })
        {
            return Err(CliFailure::classified(
                CliFailureKind::RecoveryRequired,
                format!(
                    "host_upgrade_legacy_receipt_pending: inspect legacy upgrade {id} with the original builds before starting this host; do not delete the receipt"
                ),
            ));
        }
    }
    Ok(())
}

fn private_directory_if_present(path: &Path) -> CliResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && private_owner(&metadata) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        _ => Err(invalid_receipt()),
    }
}

fn private_owner(metadata: &fs::Metadata) -> bool {
    if metadata.mode() & 0o077 != 0 {
        return false;
    }
    metadata.uid() == rustix::process::geteuid().as_raw()
}

fn read_state(path: &Path) -> CliResult<Option<ReceiptState>> {
    let file = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(invalid_receipt()),
    };
    let metadata = file.metadata().map_err(|_| invalid_receipt())?;
    if !metadata.is_file() || !private_owner(&metadata) || metadata.len() > MAX_RECEIPT_BYTES {
        return Err(invalid_receipt());
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_receipt())?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(invalid_receipt());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| invalid_receipt())
}

fn invalid_receipt() -> CliFailure {
    CliFailure::new(
        "host_upgrade_legacy_receipt_invalid: cannot verify old upgrade state; inspect it with the original builds before starting this host",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    #[test]
    fn startup_guard_leaves_legacy_receipts_untouched() {
        use std::io::Write as _;
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().to_owned());
        assert!(refuse_unresolved(&paths).is_ok());
        let op = paths
            .home()
            .join("host/upgrades")
            .join(uuid::Uuid::new_v4().to_string());
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&op)
            .unwrap();
        assert!(refuse_unresolved(&paths).is_ok());
        let receipt = op.join("receipt.json");
        for (phase, abandoned, pending) in [
            ("acknowledged", false, false),
            ("acknowledged", true, true),
            ("aborted", false, false),
            ("prepared", false, true),
            ("committed", false, true),
        ] {
            let body = format!(
                r#"{{"phase":"{phase}","mappings":[{{"abandoned":false}},{{"abandoned":{abandoned}}}]}}"#
            );
            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&receipt)
                .unwrap()
                .write_all(body.as_bytes())
                .unwrap();
            let checked = refuse_unresolved(&paths);
            if pending {
                let refusal = checked.unwrap_err();
                assert!(refusal.is_recovery_required());
                assert!(
                    refusal
                        .message()
                        .starts_with("host_upgrade_legacy_receipt_pending:")
                );
            } else {
                checked.unwrap();
            }
            assert_eq!(fs::read_to_string(&receipt).unwrap(), body);
        }
    }
}
