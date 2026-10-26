//! Fetch bounds remain readable without the HTTP fetch implementation.

/// Default response-body cap, in bytes.
pub const DEFAULT_FETCH_MAX_BYTES: u32 = 2_097_152;
/// Largest configurable response-body cap, in bytes.
pub const MAX_FETCH_MAX_BYTES: u32 = 8_388_608;
/// Default whole-fetch deadline, in seconds.
pub const DEFAULT_FETCH_TIMEOUT_SECONDS: u16 = 30;
/// Longest configurable whole-fetch deadline, in seconds.
pub const MAX_FETCH_TIMEOUT_SECONDS: u16 = 120;
/// Most redirects one call follows.
pub const MAX_FETCH_REDIRECTS: usize = 5;
/// Most result text returned inline; the rest is spilled to a file.
pub const MAX_INLINE_FETCH_TEXT_BYTES: usize = 256 * 1024;

/// The configured bounds of every `fetch_url` call in one session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchUrlLimits {
    max_bytes: u32,
    timeout_seconds: u16,
}

impl Default for FetchUrlLimits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_FETCH_MAX_BYTES,
            timeout_seconds: DEFAULT_FETCH_TIMEOUT_SECONDS,
        }
    }
}

impl FetchUrlLimits {
    /// A response-body cap in `1..=MAX_FETCH_MAX_BYTES` and a whole-fetch
    /// deadline in `1..=MAX_FETCH_TIMEOUT_SECONDS` seconds; `None` outside
    /// either range.
    #[must_use]
    pub const fn new(max_bytes: u32, timeout_seconds: u16) -> Option<Self> {
        if max_bytes == 0
            || max_bytes > MAX_FETCH_MAX_BYTES
            || timeout_seconds == 0
            || timeout_seconds > MAX_FETCH_TIMEOUT_SECONDS
        {
            return None;
        }
        Some(Self {
            max_bytes,
            timeout_seconds,
        })
    }

    /// The response-body cap before conversion, in bytes.
    #[must_use]
    pub const fn max_bytes(self) -> u32 {
        self.max_bytes
    }

    /// The whole-fetch deadline, redirects and conversion included.
    #[must_use]
    pub const fn timeout_seconds(self) -> u16 {
        self.timeout_seconds
    }
}
