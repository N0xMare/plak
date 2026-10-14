//! Locate the `plakous-js-host` sidecar for Code Mode and js-ptc.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const EXPLICIT_ENV: &str = "PLAK_JS_HOST";
#[cfg(windows)]
const HOST_FILE_NAME: &str = "plakous-js-host.exe";
#[cfg(not(windows))]
const HOST_FILE_NAME: &str = "plakous-js-host";

/// JSONL `hello` evidence: whether this process can locate `plakous-js-host`.
#[must_use]
pub fn hello_status() -> (String, &'static str) {
    match locate() {
        Ok(_) => ("discovered".to_owned(), "rquickjs-host"),
        Err(_) => ("unavailable".to_owned(), "rquickjs-host"),
    }
}

/// Find a runnable `plakous-js-host`.
pub fn locate() -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os(EXPLICIT_ENV) {
        let path = PathBuf::from(explicit);
        return require_file(&path);
    }
    if let Some(exe) = crate::build_identity::running_executable() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(HOST_FILE_NAME);
            if sibling.is_file() {
                return require_file(&sibling);
            }
        }
    }
    Err(format!(
        "{HOST_FILE_NAME} not found; set {EXPLICIT_ENV} or install the sidecar beside plak"
    ))
}

fn require_file(path: &Path) -> Result<PathBuf, String> {
    let meta = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))
}
