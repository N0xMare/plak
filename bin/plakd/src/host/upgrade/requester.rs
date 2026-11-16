//! Requester loss withdraws preparation; a Ready transfer no longer belongs to its socket.

use super::{CliResult, UnixStream, io_failure};
use std::io::Read as _;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(super) struct Caller {
    pub(super) cancelled: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    reader: UnixStream,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Caller {
    pub(super) fn watch(stream: &UnixStream, host_cancelled: Arc<AtomicBool>) -> CliResult<Self> {
        let mut reader = stream.try_clone().map_err(io_failure)?;
        reader
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(io_failure)?;
        let closer = reader.try_clone().map_err(io_failure)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (lost, finished) = (Arc::clone(&cancelled), Arc::clone(&done));
        let worker = std::thread::spawn(move || {
            let mut byte = [0];
            while !finished.load(Ordering::Acquire) {
                if host_cancelled.load(Ordering::Acquire) {
                    lost.store(true, Ordering::Release);
                    break;
                }
                match reader.read(&mut byte) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    _ => {
                        if !finished.load(Ordering::Acquire) {
                            lost.store(true, Ordering::Release);
                        }
                        break;
                    }
                }
            }
        });
        Ok(Self {
            cancelled,
            done,
            reader: closer,
            worker: Some(worker),
        })
    }

    pub(super) fn ready(&self) {
        self.done.store(true, Ordering::Release);
        let _ = self.reader.shutdown(std::net::Shutdown::Read);
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        self.ready();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
