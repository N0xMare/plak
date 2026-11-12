//! Owner encoding and canonical operation digests shared by both runner call paths.

use plakous_runner_protocol::{LineOwner, Operation};
use sha2::{Digest as _, Sha256};

pub(super) fn wire_owner(owner: &plakous_tools::LineOwner) -> LineOwner {
    LineOwner {
        line: owner.line().to_string(),
        generation: owner.generation(),
        branch: owner.branch().to_string(),
        fence: owner.fence(),
    }
}

/// Lowercase hex SHA-256 of the operation's canonical (RFC 8785) JSON.
pub(crate) fn digest(operation: &Operation) -> String {
    use std::fmt::Write as _;
    let bytes = serde_json_canonicalizer::to_vec(operation).unwrap_or_default();
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(&bytes) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}
