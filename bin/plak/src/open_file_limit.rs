//! The soft open-file limit, raised once at start.

use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The soft limit `plak` asks for, never past the hard limit: macOS's
/// `OPEN_MAX`, which `setrlimit` accepts there even when the hard limit is
/// unlimited, and far above what the joined-session limit and its pipes need.
const TARGET: u64 = 10_240;

/// Raises the soft `RLIMIT_NOFILE` toward the hard limit, capped at
/// [`TARGET`], before anything opens descriptors. It is never lowered, and a
/// refused raise leaves it as it was. Child processes inherit it.
pub(crate) fn raise() {
    let limit = getrlimit(Resource::Nofile);
    let target = limit.maximum.map_or(TARGET, |hard| hard.min(TARGET));
    if limit.current.is_none_or(|soft| soft >= target) {
        return;
    }
    let _ = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(target),
            maximum: limit.maximum,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{Resource, Rlimit, TARGET, getrlimit, raise, setrlimit};

    /// Each nextest test runs in its own process, so lowering the limit here
    /// touches no other test.
    #[test]
    fn a_low_soft_limit_is_raised_toward_the_hard_limit_and_never_lowered() {
        let original = getrlimit(Resource::Nofile);
        let low = Rlimit {
            current: Some(256),
            maximum: original.maximum,
        };
        assert!(setrlimit(Resource::Nofile, low).is_ok());
        raise();
        let raised = getrlimit(Resource::Nofile);
        let expected = original.maximum.map_or(TARGET, |hard| hard.min(TARGET));
        assert_eq!(raised.current, Some(expected));
        assert_eq!(raised.maximum, original.maximum);
        raise();
        assert_eq!(getrlimit(Resource::Nofile).current, Some(expected));
        let _ = setrlimit(Resource::Nofile, original);
    }
}
