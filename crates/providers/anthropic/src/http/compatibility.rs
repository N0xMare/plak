//! Documented API rejection shape; subscription applicability is inferred.

use plakous_auth::CompatibilityDrift;

pub(crate) fn unknown_beta(status: u16, body: &[u8], sent: &str) -> Option<CompatibilityDrift> {
    if status != 400 || body.len() > crate::error::MAX_PROVIDER_ERROR_BODY_BYTES {
        return None;
    }
    let document: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error = document.get("error")?;
    if error.get("type")?.as_str()? != "invalid_request_error" {
        return None;
    }
    let message = error.get("message")?.as_str()?;
    let rejected = message
        .strip_prefix("Unexpected value(s) ")?
        .split_once(" for the `anthropic-beta` header")?
        .0;
    rejected
        .split('`')
        .enumerate()
        .any(|(index, value)| index % 2 == 1 && sent.split(',').any(|beta| beta == value))
        .then_some(CompatibilityDrift::UnknownBeta)
}

#[cfg(test)]
mod tests;
