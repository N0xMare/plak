//! Ephemeral pre-bind lookup progress; never session or admission authority.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

static STATUS: Mutex<Option<String>> = Mutex::new(None);
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::Relaxed);
}

pub(crate) fn retry(attempt: u8, cause: &str) {
    let message = format!("retrying model lookup ({attempt}/3): {cause}");
    *STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
    if !TUI_ACTIVE.load(Ordering::Relaxed) {
        write_cli_status(&message);
    }
}

pub(crate) fn finished() {
    *STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

pub(crate) fn current() -> Option<String> {
    STATUS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn write_cli_status(message: &str) {
    // CLI dispatch holds Rust's stderr lock while awaiting binding on another
    // thread. Use the descriptor directly, as terminal restoration does.
    let line = format!("{message}\n");
    let mut bytes = line.as_bytes();
    while !bytes.is_empty() {
        match rustix::io::write(std::io::stderr(), bytes) {
            Ok(0) | Err(_) => break,
            Ok(count) => bytes = &bytes[count.min(bytes.len())..],
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lookup_progress_does_not_wait_for_cli_stderr_lock() {
        let locked = std::io::stderr().lock();
        let (sent, received) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            super::write_cli_status("retrying model lookup (2/3): HTTP 503");
            let _ = sent.send(());
        });
        let completed = received
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok();
        drop(locked);
        assert!(worker.join().is_ok());
        assert!(completed, "lookup status deadlocked on the CLI stderr lock");
    }
}
