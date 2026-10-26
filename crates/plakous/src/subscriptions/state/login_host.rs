//! Login provenance for host-side credential consumers such as Harbor.

use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

use super::{NativeProvider, NativeSubscriptionError, SubscriptionState, write_private_atomic};

impl SubscriptionState {
    pub(in crate::subscriptions) fn record_login_host(
        &self,
        provider: NativeProvider,
    ) -> Result<(), NativeSubscriptionError> {
        let identity = host_identity()?;
        let digest = Sha256::digest(format!("plak-login-host-v1\0{identity}"));
        let mut marker = String::from("plak-login-host-v1\n");
        for byte in digest {
            let _ = write!(marker, "{byte:02x}");
        }
        marker.push('\n');
        let path = self
            .paths
            .layout_root()
            .join(provider.as_str())
            .join("login-host");
        write_private_atomic(&path, marker.as_bytes())
    }
}

#[cfg(target_os = "linux")]
fn host_identity() -> Result<String, NativeSubscriptionError> {
    let id = std::fs::read_to_string("/etc/machine-id")
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let id = id.trim();
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(format!("linux:{}", id.to_ascii_lowercase()))
}

#[cfg(target_os = "macos")]
fn host_identity() -> Result<String, NativeSubscriptionError> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    if !output.status.success() {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let output =
        String::from_utf8(output.stdout).map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let id = output
        .lines()
        .find_map(|line| {
            line.split_once("\"IOPlatformUUID\" = \"")
                .and_then(|(_, value)| value.strip_suffix('"'))
        })
        .ok_or(NativeSubscriptionError::UnsafeStorage)?;
    let id = uuid::Uuid::parse_str(id).map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    Ok(format!("macos:{id}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn host_identity() -> Result<String, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}
