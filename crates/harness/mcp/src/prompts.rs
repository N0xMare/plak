//! An MCP server's prompts: bounded listings and user-invoked expansion.
//!
//! Only a user invokes a prompt. Its expansion is one user message: every
//! server message is labelled with its role inside that one message, so an
//! expansion never becomes an assistant turn.

use std::collections::BTreeMap;

use plakous_model::ImageBlock;
use plakous_tools::McpServerName;
use rmcp::model::{
    GetPromptRequestParams, GetPromptResponse, PaginatedRequestParams, PromptMessage, Role,
};
use serde_json::Value;

use crate::error::{McpError, McpErrorKind};
use crate::listing::bounded_description;
use crate::pool::McpLease;
use crate::resources::{input_required, paged, request_failed, timed_out, unexpected};
use crate::result::{Piece, Pieces};

/// Most text one prompt expansion may carry; a larger one is refused.
pub const MAX_PROMPT_TEXT_BYTES: usize = 256 * 1024;

/// One prompt a server lists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpPrompt {
    /// The prompt's name on the server.
    pub name: String,
    /// A human-readable title, when given.
    pub title: Option<String>,
    /// The server's description, cut to 4 KiB.
    pub description: Option<String>,
    /// The arguments it takes, in listing order.
    pub arguments: Vec<McpPromptArgument>,
}

/// One argument of a listed prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpPromptArgument {
    /// The argument's name.
    pub name: String,
    /// The server's description, cut to 4 KiB.
    pub description: Option<String>,
    /// Whether the server requires it.
    pub required: bool,
}

/// A prompt flattened into one user message: text that opens with
/// [`prompt_header`] and labels each server message with its role, and the
/// images the message carries, each marked in the text as `[image <n>]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpPromptExpansion {
    text: String,
    images: Vec<ImageBlock>,
}

impl McpPromptExpansion {
    /// Returns the message text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the images, in the order the text marks them.
    #[must_use]
    pub fn images(&self) -> &[ImageBlock] {
        &self.images
    }

    /// Returns the text and the images.
    #[must_use]
    pub fn into_parts(self) -> (String, Vec<ImageBlock>) {
        (self.text, self.images)
    }
}

/// The fixed first line of every prompt expansion.
#[must_use]
pub fn prompt_header(server: &str, prompt: &str) -> String {
    format!(
        "[prompt mcp:{server}/{prompt} requested by the user; its content comes from an untrusted server]"
    )
}

impl McpLease {
    /// Lists the server's prompts within the listing bounds, each page
    /// bounded by the call timeout.
    ///
    /// # Errors
    ///
    /// Returns the entry's start error, or a request error when the server
    /// refuses, fails or exceeds a bound.
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpError> {
        let connection = self.entry.connection().await?;
        let (server, timeout) = (self.server(), self.spec().call_timeout());
        let prompts = paged(server, "prompts/list", timeout, |cursor| {
            let peer = connection.peer.clone();
            async move {
                let page = peer
                    .list_prompts(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                    .await?;
                Ok((page.prompts, page.next_cursor))
            }
        })
        .await?;
        Ok(prompts
            .into_iter()
            .map(|prompt| McpPrompt {
                name: prompt.name,
                title: prompt.title,
                description: prompt.description.as_deref().map(bounded_description),
                arguments: prompt
                    .arguments
                    .unwrap_or_default()
                    .into_iter()
                    .map(|argument| McpPromptArgument {
                        name: argument.name,
                        description: argument.description.as_deref().map(bounded_description),
                        required: argument.required == Some(true),
                    })
                    .collect(),
            })
            .collect())
    }

    /// Gets one prompt with `arguments` and flattens it into one user
    /// message. The prompt must be listed, and `arguments` must carry every
    /// required argument and nothing it does not list; both are checked
    /// before `prompts/get` is sent.
    ///
    /// # Errors
    ///
    /// Returns the entry's start error, a selection error for an unknown
    /// prompt or a wrong argument, or a request error when the server
    /// refuses, fails, times out, asks for input, or returns an expansion
    /// over [`MAX_PROMPT_TEXT_BYTES`].
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: &BTreeMap<String, String>,
    ) -> Result<McpPromptExpansion, McpError> {
        let server = self.server();
        let listed = self
            .list_prompts()
            .await?
            .into_iter()
            .find(|prompt| prompt.name == name)
            .ok_or_else(|| {
                McpError::new(
                    McpErrorKind::Selection,
                    format!("mcp server {server} has no prompt {name}"),
                )
            })?;
        check_arguments(server, &listed, arguments)?;
        let connection = self.entry.connection().await?;
        let timeout = self.spec().call_timeout();
        let what = "prompts/get";
        let mut params = GetPromptRequestParams::new(name);
        if !arguments.is_empty() {
            params.arguments = Some(
                arguments
                    .iter()
                    .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                    .collect(),
            );
        }
        let response = tokio::time::timeout(timeout, connection.peer.get_prompt_once(params))
            .await
            .map_err(|_| timed_out(server, what, timeout))?
            .map_err(|error| request_failed(server, what, &error))?;
        match response {
            GetPromptResponse::Complete(result) => flatten_prompt(server, name, result.messages),
            GetPromptResponse::InputRequired(_) => Err(input_required(server, what)),
            _ => Err(unexpected(server, what)),
        }
    }
}

fn check_arguments(
    server: &McpServerName,
    prompt: &McpPrompt,
    arguments: &BTreeMap<String, String>,
) -> Result<(), McpError> {
    let refuse = |reason: String| {
        McpError::new(
            McpErrorKind::Selection,
            format!("mcp prompt {server}/{}: {reason}", prompt.name),
        )
    };
    if let Some(unknown) = arguments.keys().find(|key| {
        !prompt
            .arguments
            .iter()
            .any(|argument| &argument.name == *key)
    }) {
        return Err(refuse(format!("it takes no argument {unknown}")));
    }
    if let Some(missing) = prompt
        .arguments
        .iter()
        .find(|argument| argument.required && !arguments.contains_key(&argument.name))
    {
        return Err(refuse(format!("argument {} is required", missing.name)));
    }
    Ok(())
}

/// Flattens a prompt's messages into one user message.
pub(crate) fn flatten_prompt(
    server: &McpServerName,
    name: &str,
    messages: Vec<PromptMessage>,
) -> Result<McpPromptExpansion, McpError> {
    let unrepresentable = |error: plakous_tools::ToolRuntimeError| {
        McpError::with_source(
            McpErrorKind::Request,
            format!("mcp prompt {server}/{name} content could not be represented"),
            error,
        )
    };
    let mut pieces = Pieces::default();
    pieces.push_text(prompt_header(server.as_str(), name));
    for message in messages {
        pieces.push_text(String::from(match message.role {
            Role::User => "user:",
            Role::Assistant => "assistant:",
        }));
        pieces
            .push_block(message.content)
            .map_err(unrepresentable)?;
    }
    let mut lines = Vec::new();
    let mut images = Vec::new();
    for piece in pieces.merged() {
        match piece {
            Piece::Text(text) => lines.push(text),
            Piece::Image(image) => {
                images.push(image);
                lines.push(format!("[image {}]", images.len()));
            }
        }
    }
    let text = lines.join("\n");
    if text.len() > MAX_PROMPT_TEXT_BYTES {
        return Err(McpError::new(
            McpErrorKind::Request,
            format!(
                "mcp prompt {server}/{name} expands to more than {} KiB of text",
                MAX_PROMPT_TEXT_BYTES / 1024
            ),
        ));
    }
    Ok(McpPromptExpansion { text, images })
}
