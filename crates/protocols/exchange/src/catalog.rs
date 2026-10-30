//! Bounded catalog transport and ordered reviewed-table projection.

use std::fmt;

use plakous_http::{Client, HeaderMap, Method, Origin, Request, SensitiveBytes, StatusCode};
use plakous_model::ReasoningEffort;

use crate::lookup::{LookupBudget, LookupError};

/// A catalog GET failed before its provider-specific parser ran.
#[derive(Debug)]
pub enum CatalogFetchError {
    /// Request construction failed without dispatch.
    Request,
    /// Bounded lookup or body consumption failed.
    Lookup(LookupError),
    /// A settled response was not HTTP 200.
    Status(u16),
}

impl fmt::Display for CatalogFetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request => f.write_str("request failed"),
            Self::Lookup(LookupError::BodyLimit) => f.write_str("exceeds the body bound"),
            Self::Lookup(error) => error.fmt(f),
            Self::Status(status) => write!(f, "answered HTTP {status}"),
        }
    }
}

impl std::error::Error for CatalogFetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lookup(error) => Some(error),
            Self::Request | Self::Status(_) => None,
        }
    }
}

/// Fetches one exact-origin page with caller-owned authentication headers.
///
/// # Errors
/// Returns bounded lookup, construction or status errors without parsing rows.
pub async fn get_page(
    client: &Client,
    origin: Origin,
    path: &str,
    headers: HeaderMap,
    budget: LookupBudget,
    max_body_bytes: usize,
) -> Result<Vec<u8>, CatalogFetchError> {
    let request = Request::new(
        origin,
        Method::GET,
        path,
        headers,
        SensitiveBytes::new(Vec::new()).into_bytes(),
        client.limits(),
    )
    .map_err(|_| CatalogFetchError::Request)?;
    let reply = crate::lookup::get(client, request, budget, max_body_bytes)
        .await
        .map_err(CatalogFetchError::Lookup)?;
    if reply.status != StatusCode::OK {
        return Err(CatalogFetchError::Status(reply.status.as_u16()));
    }
    Ok(reply.body.to_vec())
}

/// Checks a bounded ASCII identifier under the integration's punctuation set.
#[must_use]
pub fn safe_id(id: &str, max_bytes: usize, punctuation: &[u8]) -> bool {
    !id.is_empty()
        && id.len() <= max_bytes
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || punctuation.contains(&byte))
}

/// Keeps safe unique rows in input order, checking the bound before filtering.
///
/// # Errors
/// Returns the integration's catalog diagnostic suffix for a bound or duplicate.
pub fn accept_listed<T: Clone>(
    models: &[T],
    max_models: usize,
    id: impl Fn(&T) -> &str,
    accept_id: impl Fn(&str) -> Result<bool, &'static str>,
    validate: impl Fn(&T) -> Result<(), &'static str>,
) -> Result<Vec<T>, &'static str> {
    if models.len() > max_models {
        return Err("exceeds the row bound");
    }
    let mut accepted = Vec::with_capacity(models.len());
    for model in models {
        if !accept_id(id(model))? {
            continue;
        }
        if accepted.iter().any(|seen| id(seen) == id(model)) {
            return Err("repeats a model id");
        }
        validate(model)?;
        accepted.push(model.clone());
    }
    Ok(accepted)
}

/// Intersects published efforts with a reviewed ladder in ladder order.
#[must_use]
pub fn efforts_from(names: &[String], ladder: &[(&str, ReasoningEffort)]) -> Vec<ReasoningEffort> {
    ladder
        .iter()
        .filter(|(name, _)| names.iter().any(|listed| listed == name))
        .map(|(_, effort)| *effort)
        .collect()
}

/// Resolves one effort without inventing an alias or admitting an unknown name.
#[must_use]
pub fn effort_from(name: &str, ladder: &[(&str, ReasoningEffort)]) -> Option<ReasoningEffort> {
    ladder
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, effort)| *effort)
}

/// Projects listed rows against exact reviewed ids, preserving listing order.
///
/// # Errors
/// Returns the integration's first classification failure.
pub fn classify_listed<L, R, M, E>(
    listed: &[L],
    reviewed: &[R],
    listed_id: impl Fn(&L) -> &str,
    reviewed_id: impl Fn(&R) -> &str,
    classify: impl Fn(&L, Option<&R>) -> Result<M, E>,
) -> Result<Vec<M>, E> {
    listed
        .iter()
        .map(|row| {
            classify(
                row,
                reviewed
                    .iter()
                    .find(|candidate| reviewed_id(candidate) == listed_id(row)),
            )
        })
        .collect()
}
