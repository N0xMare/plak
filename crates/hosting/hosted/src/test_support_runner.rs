//! Explicit owner-transfer and recorded-stop crash boundaries for gate builds.
//!
//! A gate build (`test-support`) names one `HostCut` in
//! `PLAK_TEST_HOST_READOPT_CUT`; a name that is no cut aborts the host rather
//! than never firing. Without `test-support` nothing reads the variable and no
//! cut fires.

/// A point where a gate build's host kills itself with `SIGKILL`, or, for
/// [`HostCut::SkipReadopt`], skips re-adopting its runner's lines at start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(feature = "test-support"),
    allow(dead_code, reason = "only a gate build selects a cut by name")
)]
pub enum HostCut {
    /// Re-adoption is skipped; not a crash.
    SkipReadopt,
    /// Re-adoption holds the line's head lease, before the owner transfer.
    Lease,
    /// The runner answered the owner transfer.
    Transfer,
    /// After the transfer, before a recorded stop is sent to the runner.
    BeforeStop,
    /// The runner answered the recorded stop.
    AfterStop,
    /// The re-adopted line's job terminals are recorded.
    Terminals,
    /// A quit reached a running root turn, before its handoff is recorded.
    BeforeRootHandoff,
    /// A reopen fenced the resident session, before its worker is joined.
    ReopenFenced,
    /// A reopen joined the released worker, before the binding repair.
    ReopenReleased,
    /// The binding repair ran, before the session is opened again.
    ReopenCarried,
    /// The reopen opened the session, before it ends the reopen.
    ReopenOpened,
}

#[cfg_attr(
    not(feature = "test-support"),
    allow(dead_code, reason = "only a gate build selects a cut by name")
)]
impl HostCut {
    /// Every cut, for tests that iterate the set. Selection does not read it.
    pub const ALL: [Self; 11] = [
        Self::SkipReadopt,
        Self::Lease,
        Self::Transfer,
        Self::BeforeStop,
        Self::AfterStop,
        Self::Terminals,
        Self::BeforeRootHandoff,
        Self::ReopenFenced,
        Self::ReopenReleased,
        Self::ReopenCarried,
        Self::ReopenOpened,
    ];

    /// The name `PLAK_TEST_HOST_READOPT_CUT` selects this cut by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SkipReadopt => "skip-readopt",
            Self::Lease => "lease",
            Self::Transfer => "transfer",
            Self::BeforeStop => "before-stop",
            Self::AfterStop => "after-stop",
            Self::Terminals => "terminals",
            Self::BeforeRootHandoff => "before-root-handoff",
            Self::ReopenFenced => "reopen:fenced",
            Self::ReopenReleased => "reopen:released",
            Self::ReopenCarried => "reopen:carried",
            Self::ReopenOpened => "reopen:opened",
        }
    }

    /// The cut `name` selects, if any: the inverse of [`HostCut::name`],
    /// which does not depend on [`HostCut::ALL`] listing the cut.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "skip-readopt" => Self::SkipReadopt,
            "lease" => Self::Lease,
            "transfer" => Self::Transfer,
            "before-stop" => Self::BeforeStop,
            "after-stop" => Self::AfterStop,
            "terminals" => Self::Terminals,
            "before-root-handoff" => Self::BeforeRootHandoff,
            "reopen:fenced" => Self::ReopenFenced,
            "reopen:released" => Self::ReopenReleased,
            "reopen:carried" => Self::ReopenCarried,
            "reopen:opened" => Self::ReopenOpened,
            _ => return None,
        })
    }
}

/// Kills this process when `at` is the selected cut.
#[cfg(feature = "test-support")]
pub(crate) fn cut(at: HostCut) {
    if selected(at) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGKILL);
    }
}

#[cfg(feature = "test-support")]
fn selection() -> Option<String> {
    std::env::var("PLAK_TEST_HOST_READOPT_CUT").ok()
}

#[cfg(feature = "test-support")]
fn hold_directory() -> Option<std::path::PathBuf> {
    selection()?
        .strip_prefix("hold:")
        .map(std::path::PathBuf::from)
}

/// Without the gate's build, no cut fires.
#[cfg(not(feature = "test-support"))]
pub(crate) const fn cut(_: HostCut) {}

/// Whether `at` is the cut this gate build's host was started with.
#[cfg(feature = "test-support")]
pub(crate) fn selected(at: HostCut) -> bool {
    selection().is_some_and(|name| !name.starts_with("hold:") && named_or_abort(&name) == at)
}

/// Without the gate's build, no cut is selected.
#[cfg(not(feature = "test-support"))]
pub(crate) const fn selected(_: HostCut) -> bool {
    false
}

#[cfg(feature = "test-support")]
fn named_or_abort(name: &str) -> HostCut {
    HostCut::named(name).unwrap_or_else(|| {
        eprintln!("PLAK_TEST_HOST_READOPT_CUT: no host cut is named {name:?}");
        std::process::abort()
    })
}

#[cfg(feature = "test-support")]
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

#[cfg(not(feature = "test-support"))]
pub(crate) async fn hold_readoption() {}

#[cfg(feature = "test-support")]
pub(crate) fn readoption_waiting() {
    if let Some(directory) = hold_directory() {
        let _ = std::fs::write(directory.join("waiting"), b"waiting");
    }
}

#[cfg(not(feature = "test-support"))]
pub(crate) const fn readoption_waiting() {}

#[cfg(test)]
mod tests {
    use super::HostCut;

    #[test]
    fn each_name_selects_exactly_its_own_cut() {
        for cut in HostCut::ALL {
            for other in HostCut::ALL {
                assert_eq!(
                    HostCut::named(cut.name()) == Some(other),
                    cut == other,
                    "{cut:?} and {other:?}"
                );
            }
        }
        for unknown in ["", "reopen", "reopen:", "fenced", "Lease", "before_stop"] {
            assert_eq!(HostCut::named(unknown), None, "{unknown:?}");
        }
    }
}
