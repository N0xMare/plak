//! An MCP server's resources: bounded listings and typed reads.

use std::future::Future;
use std::time::Duration;

use plakous_model::ImageBlock;
use plakous_tools::McpServerName;
use rmcp::ServiceError;
use rmcp::model::{
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ResourceContents,
};
use serde::Serialize;

use crate::error::{McpError, McpErrorKind};
use crate::http::{self, HttpRefusal};
use crate::listing::{MAX_LISTED_TOOLS, MAX_LISTING_BYTES};
use crate::pool::McpLease;
use crate::result::inline_image;

/// The MCP-side name of the resources tool a definition with resources
/// enabled gets, exposed as `mcp__<server>__read_resource`.
pub(crate) const READ_RESOURCE: &str = "read_resource";

/// One resource a server lists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpResource {
    /// The resource's URI, as the server names it.
    pub uri: String,
    /// The server's name for it.
    pub name: String,
    /// A human-readable title, when given.
    pub title: Option<String>,
    /// The server's description, when given.
    pub description: Option<String>,
    /// The MIME type, when given.
    pub mime_type: Option<String>,
    /// The size in bytes, when given.
    pub size: Option<u64>,
}

/// One resource template a server lists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpResourceTemplate {
    /// The RFC 6570 URI template.
    pub uri_template: String,
    /// The server's name for it.
    pub name: String,
    /// A human-readable title, when given.
    pub title: Option<String>,
    /// The server's description, when given.
    pub description: Option<String>,
    /// The MIME type of the resources it names, when given.
    pub mime_type: Option<String>,
}

/// One content of a resource read: untrusted data from the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpResourceContents {
    /// Text content.
    Text {
        /// The URI the server reports for this content.
        uri: String,
        /// The MIME type, when given.
        mime_type: Option<String>,
        /// The text.
        text: String,
    },
    /// Binary content.
    Blob {
        /// The URI the server reports for this content.
        uri: String,
        /// The MIME type, when given.
        mime_type: Option<String>,
        /// The content, base64 as the server sent it.
        data: String,
    },
}

impl McpResourceContents {
    /// Returns the URI the server reports for this content.
    #[must_use]
    pub fn uri(&self) -> &str {
        match self {
            Self::Text { uri, .. } | Self::Blob { uri, .. } => uri,
        }
    }

    /// Returns the MIME type, when given.
    #[must_use]
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            Self::Text { mime_type, .. } | Self::Blob { mime_type, .. } => mime_type.as_deref(),
        }
    }

    /// The content as an image a result may carry inline: a PNG, JPEG, WebP
    /// or GIF blob of valid base64 within the standard image bound.
    #[must_use]
    pub fn inline_image(&self) -> Option<ImageBlock> {
        match self {
            Self::Blob {
                mime_type: Some(mime_type),
                data,
                ..
            } => inline_image(mime_type, data, 0).ok().flatten(),
            _ => None,
        }
    }

    pub(crate) fn from_wire(contents: ResourceContents) -> Option<Self> {
        match contents {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                ..
            } => Some(Self::Text {
                uri,
                mime_type,
                text,
            }),
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => Some(Self::Blob {
                uri,
                mime_type,
                data: blob,
            }),
            _ => None,
        }
    }
}

impl McpLease {
    /// Lists the server's resources within [`MAX_LISTED_TOOLS`] entries and
    /// [`MAX_LISTING_BYTES`], each page bounded by the call timeout.
    ///
    /// # Errors
    ///
    /// Returns the entry's start error, or a request error when the server
    /// refuses, fails or exceeds a bound.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let connection = self.entry.connection().await?;
        let (server, timeout) = (self.server(), self.spec().call_timeout());
        paged(server, "resources/list", timeout, |cursor| {
            let peer = connection.peer.clone();
            async move {
                let page = peer
                    .list_resources(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                    .await?;
                Ok((page.resources, page.next_cursor))
            }
        })
        .await
        .map(|resources| {
            resources
                .into_iter()
                .map(|resource| McpResource {
                    uri: resource.uri,
                    name: resource.name,
                    title: resource.title,
                    description: resource.description,
                    mime_type: resource.mime_type,
                    size: resource.size,
                })
                .collect()
        })
    }

    /// Lists the server's resource templates within the same bounds.
    ///
    /// # Errors
    ///
    /// As [`Self::list_resources`].
    pub async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, McpError> {
        let connection = self.entry.connection().await?;
        let (server, timeout) = (self.server(), self.spec().call_timeout());
        paged(server, "resources/templates/list", timeout, |cursor| {
            let peer = connection.peer.clone();
            async move {
                let page = peer
                    .list_resource_templates(Some(
                        PaginatedRequestParams::default().with_cursor(cursor),
                    ))
                    .await?;
                Ok((page.resource_templates, page.next_cursor))
            }
        })
        .await
        .map(|templates| {
            templates
                .into_iter()
                .map(|template| McpResourceTemplate {
                    uri_template: template.uri_template,
                    name: template.name,
                    title: template.title,
                    description: template.description,
                    mime_type: template.mime_type,
                })
                .collect()
        })
    }

    /// Reads one resource, bounded by the call timeout.
    ///
    /// # Errors
    ///
    /// Returns the entry's start error, or a request error when the server
    /// refuses, fails, times out or asks for interactive input.
    pub async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContents>, McpError> {
        let connection = self.entry.connection().await?;
        let (server, timeout) = (self.server(), self.spec().call_timeout());
        let what = "resources/read";
        let response = tokio::time::timeout(
            timeout,
            connection
                .peer
                .read_resource_once(ReadResourceRequestParams::new(uri)),
        )
        .await
        .map_err(|_| timed_out(server, what, timeout))?
        .map_err(|error| request_failed(server, what, &error))?;
        match response {
            ReadResourceResponse::Complete(result) => result
                .contents
                .into_iter()
                .map(|contents| {
                    McpResourceContents::from_wire(contents).ok_or_else(|| {
                        McpError::new(
                            McpErrorKind::Request,
                            format!("mcp server {server} returned unsupported resource contents"),
                        )
                    })
                })
                .collect(),
            ReadResourceResponse::InputRequired(_) => Err(input_required(server, what)),
            _ => Err(unexpected(server, what)),
        }
    }
}

/// Pages through one listing within [`MAX_LISTED_TOOLS`] entries and
/// [`MAX_LISTING_BYTES`] encoded bytes.
pub(crate) async fn paged<T, F, Fut>(
    server: &McpServerName,
    what: &str,
    timeout: Duration,
    mut page: F,
) -> Result<Vec<T>, McpError>
where
    T: Serialize,
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<String>), ServiceError>>,
{
    let mut items = Vec::new();
    let mut bytes = 0_usize;
    let mut cursor = None;
    loop {
        let (found, next) = tokio::time::timeout(timeout, page(cursor))
            .await
            .map_err(|_| timed_out(server, what, timeout))?
            .map_err(|error| request_failed(server, what, &error))?;
        for item in found {
            bytes = bytes.saturating_add(
                serde_json::to_vec(&item).map_or(usize::MAX, |encoded| encoded.len()),
            );
            items.push(item);
            if items.len() > MAX_LISTED_TOOLS || bytes > MAX_LISTING_BYTES {
                return Err(McpError::new(
                    McpErrorKind::Request,
                    format!(
                        "mcp server {server} {what} returned more than {MAX_LISTED_TOOLS} entries or {} MiB",
                        MAX_LISTING_BYTES / (1024 * 1024)
                    ),
                ));
            }
        }
        match next {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => return Ok(items),
        }
    }
}

pub(crate) fn request_failed(server: &McpServerName, what: &str, error: &ServiceError) -> McpError {
    let message = match error {
        ServiceError::McpError(error) => format!(
            "mcp server {server} refused {what}: {} ({})",
            error.message, error.code.0
        ),
        _ => match http::classify(error) {
            Some(HttpRefusal::Credentials(status)) => http::credentials_text(server, status),
            Some(HttpRefusal::Authorization { text, .. }) => text,
            _ => format!("mcp server {server} {what} failed: {error}"),
        },
    };
    McpError::new(McpErrorKind::Request, message)
}

pub(crate) fn timed_out(server: &McpServerName, what: &str, timeout: Duration) -> McpError {
    McpError::new(
        McpErrorKind::Request,
        format!(
            "mcp server {server} did not answer {what} within {} s",
            timeout.as_secs()
        ),
    )
}

pub(crate) fn input_required(server: &McpServerName, what: &str) -> McpError {
    McpError::new(
        McpErrorKind::Request,
        format!(
            "mcp server {server} asked for interactive input on {what}; elicitation is not supported"
        ),
    )
}

pub(crate) fn unexpected(server: &McpServerName, what: &str) -> McpError {
    McpError::new(
        McpErrorKind::Request,
        format!("mcp server {server} answered {what} with an unexpected result"),
    )
}
