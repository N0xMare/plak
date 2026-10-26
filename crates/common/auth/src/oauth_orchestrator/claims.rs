//! Unvalidated, zeroizing identity claim shapes shared by OAuth dialects.

use serde::Deserialize;
use zeroize::Zeroize as _;

/// An unvalidated JWT audience, accepting both registered JSON encodings.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum AudienceClaim {
    /// A single audience string.
    One(String),
    /// An audience array.
    Many(Vec<String>),
}

impl AudienceClaim {
    /// Tests an exact single audience, not merely membership in an array.
    #[must_use]
    pub fn is_exact(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.len() == 1 && values[0] == expected,
        }
    }
}

impl Drop for AudienceClaim {
    fn drop(&mut self) {
        match self {
            Self::One(value) => value.zeroize(),
            Self::Many(values) => values.zeroize(),
        }
    }
}

/// An unvalidated subject response; the dialect checks and binds the subject.
#[derive(Deserialize)]
pub struct SubjectClaim {
    /// The unvalidated subject, wiped when this response is dropped.
    pub sub: String,
}

impl Drop for SubjectClaim {
    fn drop(&mut self) {
        self.sub.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_must_be_exactly_one() -> Result<(), serde_json::Error> {
        for encoded in [r#""client""#, r#"["client"]"#] {
            let audience: AudienceClaim = serde_json::from_str(encoded)?;
            assert!(audience.is_exact("client"));
            assert!(!audience.is_exact("other"));
        }
        for encoded in [r"[]", r#"["client","other"]"#, r#"["client","client"]"#] {
            let audience: AudienceClaim = serde_json::from_str(encoded)?;
            assert!(!audience.is_exact("client"));
        }
        Ok(())
    }
}
