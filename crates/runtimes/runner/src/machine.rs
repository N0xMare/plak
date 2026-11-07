//! Stable physical-machine evidence, independent of runner home and machine boot.

/// Reads the operating system's stable machine identity. Unknown identity must
/// refuse remote placement, never local filesystem admission.
///
/// # Errors
/// The platform identity is unavailable or malformed.
pub fn machine_id() -> std::io::Result<String> {
    #[cfg(target_os = "linux")]
    let value = std::fs::read_to_string("/etc/machine-id")?;
    #[cfg(target_os = "macos")]
    let value = {
        let output = std::process::Command::new("/usr/sbin/ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .find_map(|line| {
                let (key, value) = line.split_once('=')?;
                key.contains("\"IOPlatformUUID\"")
                    .then(|| value.trim().trim_matches('"').to_owned())
            })
            .ok_or_else(|| std::io::Error::other("machine identity unavailable"))?
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let value = String::new();
    let value = value.trim().to_ascii_lowercase();
    if !matches!(value.len(), 32 | 36)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(std::io::Error::other("machine identity unavailable"));
    }
    Ok(value)
}
