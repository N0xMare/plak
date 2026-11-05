//! Ephemeral pre-bind lookup progress; never session or admission authority.
//!
//! An interactive launch publishes it to the face on screen (its supervisor
//! installs a publisher for the launch); without one it is a CLI status line.

use std::sync::Mutex;

/// Where progress goes while an interactive launch runs.
type Publisher = Box<dyn Fn(Option<String>) + Send + Sync>;

static PUBLISHER: Mutex<Option<Publisher>> = Mutex::new(None);

/// Routes progress to `publisher` until it is cleared with `None`.
#[allow(
    dead_code,
    reason = "the interactive launch is the only publisher; a build without one has none"
)]
pub(crate) fn publish_to(publisher: Option<Publisher>) {
    *PUBLISHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = publisher;
}

pub(crate) fn retry(attempt: u8, cause: &str) {
    let message = format!("retrying model lookup ({attempt}/3): {cause}");
    let publisher = PUBLISHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match publisher.as_ref() {
        Some(publish) => publish(Some(message)),
        None => write_cli_status(&message),
    }
}

pub(crate) fn finished() {
    if let Some(publish) = PUBLISHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        publish(None);
    }
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
