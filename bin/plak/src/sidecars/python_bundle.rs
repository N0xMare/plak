//! Locate the `monty` worker for product py-ptc.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const EXPLICIT_ENV: &str = "PLAK_MONTY_WORKER";
#[cfg(windows)]
const WORKER_FILE_NAME: &str = "monty.exe";
#[cfg(not(windows))]
const WORKER_FILE_NAME: &str = "monty";

/// Find a runnable `monty` worker. No PATH search.
pub fn locate() -> Result<PathBuf, String> {
    if let Some(explicit) = env::var_os(EXPLICIT_ENV) {
        let path = PathBuf::from(explicit);
        return require_file(&path);
    }
    if let Some(exe) = crate::build_identity::running_executable() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(WORKER_FILE_NAME);
            if sibling.is_file() {
                return require_file(&sibling);
            }
        }
    }
    Err(format!(
        "{WORKER_FILE_NAME} not found; set {EXPLICIT_ENV} or install the worker beside plak"
    ))
}

/// JSONL `hello` evidence: whether this process can locate a monty worker.
#[must_use]
pub fn python_host_hello_status() -> (String, &'static str) {
    match locate() {
        Ok(_) => ("discovered".to_owned(), "monty-pool-1.1.0"),
        Err(_) => ("unavailable".to_owned(), "monty-pool-1.1.0"),
    }
}

fn require_file(path: &Path) -> Result<PathBuf, String> {
    let meta = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_explicit_worker_fails_closed() {
        let error = require_file(Path::new("/nonexistent/plak-monty-worker-admission-test"))
            .expect_err("missing worker must fail closed");
        assert!(error.contains("nonexistent"));
    }
}
