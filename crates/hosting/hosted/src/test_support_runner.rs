//! Explicit owner-transfer and recorded-stop crash boundaries for gate builds.

pub(crate) fn cut(phase: &str) {
    if selected(phase) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL);
    }
}

pub(crate) fn selected(phase: &str) -> bool {
    selection().as_deref() == Some(phase)
}

fn selection() -> Option<String> {
    std::env::var("PLAK_TEST_HOST_READOPT_CUT").ok()
}

fn hold_directory() -> Option<std::path::PathBuf> {
    selection()?
        .strip_prefix("hold:")
        .map(std::path::PathBuf::from)
}

pub(crate) fn reopen_cut(phase: &str) {
    if selected(&format!("reopen:{phase}")) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL);
    }
}

pub(crate) async fn hold_readoption() {
    let Some(directory) = hold_directory() else {
        return;
    };
    if std::fs::write(directory.join("entered"), b"held").is_err() {
        return;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while !directory.join("release").is_file() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

pub(crate) fn readoption_waiting() {
    if let Some(directory) = hold_directory() {
        let _ = std::fs::write(directory.join("waiting"), b"waiting");
    }
}
