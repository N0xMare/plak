//! Where a hosted session writes its event log: the one directory it needs
//! from the product's home. Everything else the product's layout knows —
//! config, secrets, databases — stays with the product, which builds one of
//! these with `PlakPaths::session_home`.

use std::fs;
use std::path::{Path, PathBuf};

/// The session-log directory of one product home.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionHome {
    sessions_dir: PathBuf,
}

impl SessionHome {
    /// A home rooted at an explicit sessions directory.
    #[must_use]
    pub const fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// The directory session event logs live in.
    #[must_use]
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// The append-only event log of one session.
    #[must_use]
    pub fn session_events_file(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.events.jsonl"))
    }

    /// Create the sessions directory when it is missing.
    ///
    /// # Errors
    /// Returns a message when the path exists but is not a directory or
    /// cannot be created.
    pub fn ensure(&self) -> Result<(), String> {
        match fs::symlink_metadata(&self.sessions_dir) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
            Ok(_) => Err("plak sessions path is not a directory".to_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.sessions_dir)
                    .map_err(|_| "could not create plak sessions directory".to_owned())
            }
            Err(_) => Err("could not inspect plak sessions directory".to_owned()),
        }
    }
}
