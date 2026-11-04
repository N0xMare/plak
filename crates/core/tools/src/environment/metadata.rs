//! The Unix file identity accessors workspace and process identity read.
//!
//! wasm32-unknown-unknown has no host filesystem: its `std::fs` refuses every
//! call, so no `Metadata` value exists there and these accessors are never
//! reached; they panic rather than return an identity two files could share.

#[cfg(unix)]
pub(super) use std::os::unix::fs::MetadataExt;

#[cfg(not(unix))]
pub(super) trait MetadataExt {
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
    fn mode(&self) -> u32;
}

#[cfg(not(unix))]
impl MetadataExt for std::fs::Metadata {
    fn dev(&self) -> u64 {
        unreachable!("no host filesystem on this target")
    }

    fn ino(&self) -> u64 {
        unreachable!("no host filesystem on this target")
    }

    fn mode(&self) -> u32 {
        unreachable!("no host filesystem on this target")
    }
}
