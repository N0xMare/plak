use super::{PlakPaths, resolve_api_key};

/// Full internal observation identity, never the shortened display fingerprint.
pub(crate) fn revision(paths: &PlakPaths, account: &str) -> Result<Option<[u8; 32]>, String> {
    use sha2::{Digest as _, Sha256};
    Ok(resolve_api_key(paths, account)?.map(|key| {
        let key = zeroize::Zeroizing::new(key);
        Sha256::digest(key.as_bytes()).into()
    }))
}
