//! Receipt publication is a host operation; admission reads remain feature-independent.

use super::{CliFailure, CliResult, File, MAX_BYTES, PlakPaths, Receipt, failed, root};
use std::io::Write as _;

impl Receipt {
    /// A publication error is outcome-unknown: after attempting a commit the
    /// caller must keep the source fenced, never rewrite this as an abort.
    pub(crate) fn save(&self, paths: &PlakPaths) -> CliResult<()> {
        self.validate()?;
        let directory = root(paths).join(self.id.to_string());
        crate::commands::runner::private_directory(&paths.home().join("host"))?;
        crate::commands::runner::private_directory(&root(paths))?;
        crate::commands::runner::private_directory(&directory)?;
        let bytes = serde_json::to_vec(self).map_err(|error| CliFailure::new(error.to_string()))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(CliFailure::new(
                "host upgrade receipt exceeds its size bound",
            ));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(failed)?;
        temporary.write_all(&bytes).map_err(failed)?;
        temporary.as_file().sync_all().map_err(failed)?;
        temporary
            .persist(self.path(paths))
            .map_err(|error| failed(error.error))?;
        File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(failed)?;
        File::open(root(paths))
            .and_then(|file| file.sync_all())
            .map_err(failed)?;
        File::open(paths.home().join("host"))
            .and_then(|file| file.sync_all())
            .map_err(failed)?;
        File::open(paths.home())
            .and_then(|file| file.sync_all())
            .map_err(failed)
    }
}
