//! Product duration grammar; the runtime gate has no timers or CLI vocabulary.

use std::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum HostDrain {
    Bounded { milliseconds: u64 },
    Idle,
}

impl Default for HostDrain {
    fn default() -> Self {
        Self::Bounded {
            milliseconds: 60_000,
        }
    }
}

#[cfg(feature = "web")]
mod host {
    use super::HostDrain;

    impl HostDrain {
        pub(crate) fn valid(self) -> bool {
            !matches!(self, Self::Bounded { milliseconds } if milliseconds > 3_600_000)
        }

        pub(crate) fn duration(self) -> Option<std::time::Duration> {
            match self {
                Self::Bounded { milliseconds } => {
                    Some(std::time::Duration::from_millis(milliseconds))
                }
                Self::Idle => None,
            }
        }
    }
}

impl FromStr for HostDrain {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "idle" {
            return Ok(Self::Idle);
        }
        let (digits, factor) = [("ms", 1), ("s", 1_000), ("m", 60_000), ("h", 3_600_000)]
            .into_iter()
            .find_map(|(suffix, factor)| value.strip_suffix(suffix).map(|digits| (digits, factor)))
            .unwrap_or((value, 1_000));
        let milliseconds = (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| {
                digits
                    .parse::<u64>()
                    .ok()
                    .and_then(|count| count.checked_mul(factor))
            })
            .flatten();
        match milliseconds {
            Some(milliseconds) if milliseconds <= 3_600_000 => Ok(Self::Bounded { milliseconds }),
            _ => Err("expected idle or an integer duration (ms, s, m, h; bare numbers are seconds), at most 1h".to_owned()),
        }
    }
}
