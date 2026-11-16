//! Best-effort local-login provenance; only Harbor consumes this marker.

use std::path::Path;

pub(super) fn record(directory: &Path) -> bool {
    record_with_identity(directory, host_identity)
}

fn record_with_identity(directory: &Path, identity: impl FnOnce() -> Option<String>) -> bool {
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let marker = directory.join("login-host");
    let result = (|| -> Option<()> {
        let metadata = std::fs::symlink_metadata(directory).ok()?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o7777 != 0o700
        {
            return None;
        }
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let identity = identity()?;
        let digest =
            plakous::model::sha256_hex(&[format!("plak-login-host-v1\0{identity}").as_bytes()]);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
            .ok()?;
        file.write_all(format!("plak-login-host-v1\n{digest}\n").as_bytes())
            .ok()?;
        file.sync_all().ok()?;
        Some(())
    })();
    if result.is_none() {
        let _ = std::fs::remove_file(&marker);
    }
    result.is_some()
}

#[cfg(target_os = "linux")]
fn host_identity() -> Option<String> {
    let id = std::fs::read_to_string("/etc/machine-id").ok()?;
    let id = id.trim();
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("linux:{}", id.to_ascii_lowercase()))
}

#[cfg(target_os = "macos")]
fn host_identity() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8(output.stdout).ok()?;
    let id = output.lines().find_map(|line| {
        line.split_once("\"IOPlatformUUID\" = \"")
            .and_then(|(_, value)| value.strip_suffix('"'))
    })?;
    Some(format!("macos:{}", uuid::Uuid::parse_str(id).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_identity_removes_stale_provenance_without_touching_credential()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let credential = root.path().join("credential.json");
        std::fs::write(&credential, br#"{"status":"ready","generation":7}"#)?;
        std::fs::write(root.path().join("login-host"), "stale marker")?;
        assert!(!record_with_identity(root.path(), || None));
        assert!(!root.path().join("login-host").exists());
        assert_eq!(
            std::fs::read(&credential)?,
            br#"{"status":"ready","generation":7}"#
        );
        Ok(())
    }
}
