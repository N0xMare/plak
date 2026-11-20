//! A target-local cancellation fence prevents a delayed request crossing a withdrawal.

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;

use rustix::fs::{Mode, OFlags};

use crate::CliResult;
use crate::shipping_artifact::{check, fail, io};
use crate::snapshot_install;

use super::inventory::Node;

pub(super) struct Fence {
    file: File,
    dir: PathBuf,
}

impl Fence {
    pub(super) fn lock(node: &Node, namespace: &str, ticket: uuid::Uuid) -> CliResult<Self> {
        let dir = node.home.join(".local/lib/plak/deploy-fences");
        snapshot_install::ensure(&dir)?;
        snapshot_install::require_owned(&dir)?;
        let file = File::from(
            rustix::fs::open(
                dir.join(format!("{namespace}-{ticket}")),
                OFlags::CREATE
                    | OFlags::RDWR
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)
            .map_err(io)?,
        );
        super::evidence::private(&file)?;
        file.try_lock().map_err(|_| {
            fail("target operation still holds the deploy fence; inspect and retry")
        })?;
        Ok(Self { file, dir })
    }

    pub(super) fn active(&self) -> CliResult<()> {
        check(
            self.file.metadata().map_err(io)?.len() == 0,
            "deploy operation was withdrawn",
        )
    }

    pub(super) fn untouched(&self) -> CliResult<bool> {
        Ok(self.file.metadata().map_err(io)?.len() == 0)
    }

    pub(super) fn requester_input(&self) -> CliResult<File> {
        // The requester does not read stdin. Keep the same locked file
        // description in the child even if its target wrapper dies.
        self.file.try_clone().map_err(io)
    }

    pub(super) fn cancel(&mut self) -> CliResult<()> {
        self.seal(b"withdrawn\n")
    }

    pub(super) fn claim_once(&mut self) -> CliResult<()> {
        self.active()?;
        self.seal(b"started\n")
    }

    fn seal(&mut self, record: &[u8]) -> CliResult<()> {
        self.file.write_all(record).map_err(io)?;
        self.file.sync_all().map_err(io)?;
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(io)
    }
}
