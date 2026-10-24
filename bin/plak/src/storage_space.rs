//! Capacity checks on the volume containing the product home, never the workspace.

use std::path::{Path, PathBuf};

use plakous_hosted::session_store::StorageAdmission;

const MIB: u64 = 1024 * 1024;
const WARN_BELOW: u64 = 5 * 1024 * MIB;
const REFUSE_BELOW: u64 = 512 * MIB;

pub(crate) fn healthy_notice() -> String {
    format!(
        "product home volume has at least {} MiB available",
        WARN_BELOW / MIB
    )
}

pub(crate) fn admission(home: &Path) -> StorageAdmission {
    #[cfg(feature = "test-hooks")]
    if let Some(admission) = crate::test_hooks::storage_capacity_override() {
        return admission;
    }
    admission_from(available(home))
}

fn admission_from(result: std::io::Result<(PathBuf, u64)>) -> StorageAdmission {
    match result {
        Ok((volume_path, bytes)) => classify(&volume_path, bytes),
        Err(error) => StorageAdmission::Warning(format!(
            "product home volume free space is unavailable: {error}; run `plak doctor`"
        )),
    }
}

fn available(home: &Path) -> std::io::Result<(PathBuf, u64)> {
    let mut existing = home;
    while !existing.try_exists()? {
        existing = existing.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "product home has no existing ancestor",
            )
        })?;
    }
    let path = existing.canonicalize()?;
    let stat = rustix::fs::statvfs(&path)?;
    Ok((path, stat.f_bavail.saturating_mul(stat.f_frsize)))
}

fn classify(volume_path: &Path, bytes: u64) -> StorageAdmission {
    if bytes >= WARN_BELOW {
        return StorageAdmission::Available;
    }
    let notice = format!(
        "product home volume has {} MiB available (containing {}); warning below {} MiB, \
         new turns and spawns refused below {} MiB. The operator can free space and retry; existing work is not cancelled",
        bytes / MIB,
        volume_path.display(),
        WARN_BELOW / MIB,
        REFUSE_BELOW / MIB
    );
    if bytes < REFUSE_BELOW {
        StorageAdmission::Refused(notice)
    } else {
        StorageAdmission::Warning(notice)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit tests")]

    use super::*;

    #[test]
    fn capacity_boundaries_are_inclusive_and_recover_on_the_next_probe() {
        let path = Path::new("/product-volume");
        assert!(matches!(
            classify(path, REFUSE_BELOW - 1),
            StorageAdmission::Refused(_)
        ));
        assert!(matches!(
            classify(path, REFUSE_BELOW),
            StorageAdmission::Warning(_)
        ));
        assert!(matches!(
            classify(path, WARN_BELOW - 1),
            StorageAdmission::Warning(_)
        ));
        assert_eq!(classify(path, WARN_BELOW), StorageAdmission::Available);
        let StorageAdmission::Refused(notice) = classify(path, 0) else {
            panic!("empty volume must refuse admission");
        };
        assert!(notice.contains("/product-volume") && notice.contains("0 MiB"));
    }

    #[test]
    fn a_new_home_probes_its_existing_ancestor_without_creating_anything() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("missing/home");
        let (path, _) = available(&home).unwrap();
        assert_eq!(path, root.path().canonicalize().unwrap());
        assert!(!home.exists());
    }

    #[test]
    fn an_unreadable_probe_warns_instead_of_inventing_zero_free_space() {
        let StorageAdmission::Warning(notice) = admission_from(Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))) else {
            panic!("unknown capacity must remain a warning");
        };
        assert!(notice.contains("unavailable"));
        assert!(!notice.contains("0 MiB"));
    }
}
