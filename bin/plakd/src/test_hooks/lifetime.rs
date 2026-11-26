//! Stop fixture processes when the test that owns their lifetime lock exits.

use std::fs::{File, TryLockError};
use std::io::{self, Write as _};

pub(crate) const OWNER_LOCK: &str = "PLAK_TEST_OWNER_LOCK";

pub(crate) fn enforce() -> io::Result<()> {
    let Some(path) = super::hook_environment(OWNER_LOCK) else {
        return Ok(());
    };
    let owner = File::open(path)?;
    match owner.try_lock_shared() {
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(error)) => return Err(error),
        Ok(()) => return Err(io::Error::other("the test owner has already exited")),
    }
    std::thread::Builder::new()
        .name("plakd-test-owner".to_owned())
        .spawn(move || {
            // The kernel releases the owner's lock even on SIGKILL. A
            // blocking shared lock needs no polling or PID-reuse checks.
            loop {
                match owner.lock_shared() {
                    Ok(()) => {
                        let _ = writeln!(io::stderr(), "test owner exited");
                        std::process::exit(1);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let _ = writeln!(io::stderr(), "test owner lock watch failed: {error}");
                        break;
                    }
                }
            }
        })?;
    Ok(())
}
