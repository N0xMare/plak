//! Staging for product files published by rename or exclusive link.

use std::path::Path;

pub(crate) fn stage(directory: &Path, prefix: &str) -> std::io::Result<tempfile::NamedTempFile> {
    const RANDOM_CHARS: usize = 6;
    plakous::auth::private_temps::sweep(directory, |name| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.len() == RANDOM_CHARS && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    });
    tempfile::Builder::new()
        .prefix(prefix)
        .rand_bytes(RANDOM_CHARS)
        .tempfile_in(directory)
}
