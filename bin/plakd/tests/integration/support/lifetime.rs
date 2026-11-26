//! A lock held by the test, not by any of its daemon processes.

#![allow(clippy::expect_used, reason = "private real-process test harness")]

use std::fs::File;
use std::path::Path;

pub(super) const OWNER_LOCK: &str = "PLAK_TEST_OWNER_LOCK";

pub(super) struct Lifetime {
    _owner: File,
}

impl Lifetime {
    pub(super) fn new(root: &Path) -> Self {
        let file = File::create_new(Self::environment(root).1).expect("test owner file");
        file.lock().expect("test owner lock");
        Self { _owner: file }
    }

    pub(super) fn environment(root: &Path) -> (&'static str, std::path::PathBuf) {
        (OWNER_LOCK, root.join("test-owner.lock"))
    }
}
