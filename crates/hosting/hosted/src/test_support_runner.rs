//! Explicit owner-transfer and recorded-stop crash boundaries for gate builds.

pub(crate) fn cut(phase: &str) {
    if std::env::var("PLAK_TEST_HOST_READOPT_CUT").as_deref() == Ok(phase) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL);
    }
}
