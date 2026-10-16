//! `fetch_url`: one Plak-run HTTPS GET of a URL the model chose.
//!
//! Plak itself fetches; no provider is involved. Every hop, redirects
//! included, goes through one client whose [`AddressPolicy::public_only`]
//! is checked on the exact address then connected, so a model-chosen URL
//! never reaches loopback, private, link-local or metadata addresses.

mod content;

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_http::{
    AddressPolicy, Bytes, Client, ErrorKind, ExchangeControl, Failure, HeaderMap, HeaderName,
    HeaderValue, Limits, Method, Origin, Request, Response, StatusCode,
};
use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    CodingPresentation, MenuEntry, PassthroughPack, PromptPin, RegisteredToolId, ToolContext,
    ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation,
    ToolOutcome, ToolPack, ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind, object_schema,
};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::text::PLAKOUS_WEB_FETCH_URL_DESCRIPTION;
use crate::{
    ISSUER, MAX_URL_BYTES, SEMANTIC_VERSION, WEB_PROMPT_PINS, invalid, registration_error,
    require_text,
};

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

const TOOL_NAME: &str = "fetch_url";
const ACCEPT: &str =
    "text/markdown, text/html;q=0.9, text/plain;q=0.8, application/json;q=0.5, */*;q=0.1";
/// Conversion walks the HTML tree recursively; it runs on its own thread
/// with this stack, and the tree depth is bounded before the walk.
const RENDER_STACK_BYTES: usize = 32 * 1024 * 1024;

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

/// The `fetch_url` tool: one registration, appended after the core menu in
/// every vocabulary under the same name.
#[derive(Clone)]
pub struct FetchUrlPack {
    fetcher: Arc<Fetcher>,
}

impl FetchUrlPack {
    /// Binds `fetch_url` under `limits`, sending `user_agent` on every
    /// request, over a public-only HTTPS client.
    ///
    /// # Errors
    ///
    /// Refuses a user agent that is not a valid header value, and a client
    /// that cannot be built.
    pub fn new(limits: FetchUrlLimits, user_agent: &str) -> Result<Self, ToolRuntimeError> {
        let client = Client::new(Limits::default())
            .map_err(registration_error("fetch_url client could not be built"))?
            .with_address_policy(Some(AddressPolicy::public_only()));
        Self::with_client(client, limits, user_agent)
    }

    fn with_client(
        client: Client,
        limits: FetchUrlLimits,
        user_agent: &str,
    ) -> Result<Self, ToolRuntimeError> {
        let user_agent = HeaderValue::from_str(user_agent)
            .map_err(registration_error("fetch_url user agent is invalid"))?;
        Ok(Self {
            fetcher: Arc::new(Fetcher {
                client,
                limits,
                user_agent,
            }),
        })
    }

    /// The configured bounds.
    #[must_use]
    pub fn limits(&self) -> FetchUrlLimits {
        self.fetcher.limits
    }

    /// The one registration. Its identity names no limit: the bounds change
    /// what a call returns, not what the model is offered.
    fn registration(&self) -> Result<ToolRegistration, ToolRuntimeError> {
        let definition = ToolDefinition::new(
            ToolName::new(TOOL_NAME).map_err(registration_error("fetch_url name is invalid"))?,
            SEMANTIC_VERSION,
            PLAKOUS_WEB_FETCH_URL_DESCRIPTION,
            ToolInputContract::structured(schema())
                .map_err(registration_error("fetch_url schema is invalid"))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        )
        .map_err(registration_error("fetch_url definition is invalid"))?;
        Ok(ToolRegistration::new(
            RegisteredToolId::new("plakous.web.fetch_url.v1")
                .map_err(registration_error("fetch_url id is invalid"))?,
            ToolExecutorId::new("plakous.web.fetch_url.executor.v1")
                .map_err(registration_error("fetch_url executor id is invalid"))?,
            ToolExecutorBindingIdentity::new(1, &[ISSUER, TOOL_NAME.as_bytes()])
                .map_err(registration_error("fetch_url binding identity is invalid"))?,
            definition,
            Arc::new(FetchUrlTool {
                fetcher: Arc::clone(&self.fetcher),
            }),
        )
        .nested_eligible())
    }
}

impl ToolPack for FetchUrlPack {
    fn id(&self) -> &'static str {
        "plakous.web.fetch_url.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        Ok(vec![self.registration()?])
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        let name =
            ToolName::new(TOOL_NAME).map_err(registration_error("fetch_url name is invalid"))?;
        PassthroughPack::appended_menu(&[name])
    }

    fn pins(&self) -> &'static [PromptPin] {
        WEB_PROMPT_PINS
    }
}

fn schema() -> Value {
    object_schema(
        json!({
            "url": {"type": "string", "minLength": 8, "maxLength": MAX_URL_BYTES},
            "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_FETCH_MAX_BYTES}
        }),
        &["url"],
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    url: String,
    max_bytes: Option<u64>,
}

/// One validated call: the URL to request first and its body cap.
#[derive(Debug)]
struct Call {
    url: Url,
    max_bytes: usize,
}

impl Call {
    fn parse(arguments: Value, limits: FetchUrlLimits) -> Result<Self, ToolRuntimeError> {
        let arguments: Arguments = serde_json::from_value(arguments)
            .map_err(|_| invalid("fetch_url arguments are invalid"))?;
        let text = require_text(&arguments.url, MAX_URL_BYTES, "url")?;
        let mut url =
            Url::parse(&text).map_err(|_| invalid("url must be an absolute https URL"))?;
        if url.scheme() == "http" {
            url.set_scheme("https")
                .map_err(|()| invalid("url must be an absolute https URL"))?;
        }
        let url = checked(url).map_err(|refusal| invalid(refusal.message()))?;
        let cap = limits.max_bytes as usize;
        let max_bytes = match arguments.max_bytes {
            None => cap,
            Some(0) => return Err(invalid("max_bytes must be at least 1")),
            Some(requested) => {
                usize::try_from(requested).map_or(cap, |requested| requested.min(cap))
            }
        };
        Ok(Self { url, max_bytes })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UrlRefusal {
    Scheme,
    Credentials,
    Host,
}

impl UrlRefusal {
    const fn message(self) -> &'static str {
        match self {
            Self::Scheme => "url must be an absolute https URL",
            Self::Credentials => "url must not carry a user name or password",
            Self::Host => "url must name a host",
        }
    }
}

/// An `https` URL with a host and no credentials, without its fragment.
fn checked(mut url: Url) -> Result<Url, UrlRefusal> {
    if url.scheme() != "https" {
        return Err(UrlRefusal::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UrlRefusal::Credentials);
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(UrlRefusal::Host);
    }
    url.set_fragment(None);
    Ok(url)
}

fn host(url: &Url) -> &str {
    url.host_str().unwrap_or_default()
}

/// The shared state of every call in one session.
struct Fetcher {
    client: Client,
    limits: FetchUrlLimits,
    user_agent: HeaderValue,
}

/// A response the call settled on: the last hop, with at most its cap of
/// body bytes.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub(crate) requested: Url,
    pub(crate) final_url: Url,
    pub(crate) status: StatusCode,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
    /// The body continued past the cap.
    pub(crate) truncated: bool,
    pub(crate) max_bytes: usize,
}

/// Why a call produced no response to render; every variant is a tool
/// error the model reads.
#[derive(Debug)]
pub(crate) enum FetchError {
    /// The address policy refused the destination.
    Refused {
        host: String,
        local_name: bool,
    },
    /// A redirect pointed from https to http.
    Downgrade {
        location: String,
    },
    /// A redirect pointed at no usable https URL.
    BadRedirect {
        refusal: String,
    },
    TooManyRedirects,
    Timeout {
        seconds: u16,
    },
    Transport {
        host: String,
        failure: Failure,
    },
    /// The URL could not be expressed as one request.
    Unrequestable,
}

impl FetchError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Refused {
                host,
                local_name: true,
            } => format!(
                "fetch_url refused {host}: the name is reserved for the local machine or a \
                 cloud metadata service"
            ),
            Self::Refused {
                host,
                local_name: false,
            } => format!(
                "fetch_url refused {host}: it resolves to no public internet address \
                 (loopback, private, link-local, shared, metadata, documentation and reserved \
                 ranges are refused)"
            ),
            Self::Downgrade { location } => {
                format!("fetch_url refused a redirect from https to http: {location}")
            }
            Self::BadRedirect { refusal } => format!("fetch_url refused a redirect: {refusal}"),
            Self::TooManyRedirects => {
                format!("fetch_url stopped after {MAX_FETCH_REDIRECTS} redirects")
            }
            Self::Timeout { seconds } => format!("fetch_url timed out after {seconds} s"),
            Self::Transport { host, failure } => match failure.kind() {
                ErrorKind::Resolution => format!("fetch_url could not resolve {host}"),
                ErrorKind::Tls => format!("fetch_url could not establish TLS with {host}"),
                _ => format!("fetch_url could not fetch from {host}: {failure}"),
            },
            Self::Unrequestable => String::from("fetch_url cannot request this URL"),
        }
    }
}

impl Fetcher {
    /// Follows at most [`MAX_FETCH_REDIRECTS`] redirects, each re-checked,
    /// and reads the final body up to its cap.
    async fn fetch(&self, call: &Call, deadline: Instant) -> Result<Fetched, FetchError> {
        let mut current = call.url.clone();
        let mut redirects = 0;
        loop {
            let response = self.exchange(&current, deadline).await?;
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = header_text(&response, "location")
            {
                if redirects == MAX_FETCH_REDIRECTS {
                    return Err(FetchError::TooManyRedirects);
                }
                redirects += 1;
                current = redirect_target(&current, &location)?;
                continue;
            }
            let content_type = header_text(&response, "content-type");
            let cap = if status.as_u16() >= 400 {
                content::ERROR_BODY_BYTES
            } else {
                call.max_bytes
            };
            let (body, truncated) = self.read_body(response, cap, &current).await?;
            return Ok(Fetched {
                requested: call.url.clone(),
                final_url: current,
                status,
                content_type,
                body,
                truncated,
                max_bytes: call.max_bytes,
            });
        }
    }

    async fn exchange(&self, url: &Url, deadline: Instant) -> Result<Response, FetchError> {
        let host_text = host(url);
        let origin = match url.port() {
            Some(port) => format!("https://{host_text}:{port}/"),
            None => format!("https://{host_text}/"),
        };
        let origin = Origin::parse(&origin).map_err(|_| FetchError::Unrequestable)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static(ACCEPT),
        );
        headers.insert(
            HeaderName::from_static("accept-encoding"),
            HeaderValue::from_static("identity"),
        );
        headers.insert(
            HeaderName::from_static("user-agent"),
            self.user_agent.clone(),
        );
        let request = Request::new(
            origin,
            Method::GET,
            &url[url::Position::BeforePath..url::Position::AfterQuery],
            headers,
            Bytes::new(),
            self.client.limits(),
        )
        .map_err(|_| FetchError::Unrequestable)?;
        self.client
            .execute(request, ExchangeControl::new(deadline))
            .await
            .map_err(|failure| self.failure(url, failure))
    }

    async fn read_body(
        &self,
        response: Response,
        cap: usize,
        url: &Url,
    ) -> Result<(Vec<u8>, bool), FetchError> {
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk.map_err(|failure| self.failure(url, failure))?;
            let room = cap - bytes.len();
            if chunk.len() > room {
                bytes.extend_from_slice(&chunk[..room]);
                return Ok((bytes, true));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((bytes, false))
    }

    fn failure(&self, url: &Url, failure: Failure) -> FetchError {
        let host = host(url).to_owned();
        match failure.kind() {
            ErrorKind::AddressRefused => FetchError::Refused {
                local_name: AddressPolicy::public_only().refuses_host_name(&host),
                host,
            },
            ErrorKind::DeadlineExceeded => FetchError::Timeout {
                seconds: self.limits.timeout_seconds,
            },
            _ => FetchError::Transport { host, failure },
        }
    }
}

fn header_text(response: &Response, name: &'static str) -> Option<String> {
    response
        .headers()
        .get(&HeaderName::from_static(name))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The next hop of a redirect from `current`: re-parsed and re-checked like
/// the first URL, except that http is a downgrade, never an upgrade.
fn redirect_target(current: &Url, location: &str) -> Result<Url, FetchError> {
    let next = current
        .join(location)
        .map_err(|_| FetchError::BadRedirect {
            refusal: String::from("the location is not a URL"),
        })?;
    if next.scheme() == "http" {
        let mut shown = next;
        shown.set_fragment(None);
        return Err(FetchError::Downgrade {
            location: shown.to_string(),
        });
    }
    checked(next).map_err(|refusal| FetchError::BadRedirect {
        refusal: refusal.message().replacen("url", "the location", 1),
    })
}

struct FetchUrlTool {
    fetcher: Arc<Fetcher>,
}

impl ToolExecutor for FetchUrlTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let call = Call::parse(
                invocation.arguments().value().as_ref().clone(),
                self.fetcher.limits,
            )?;
            let subject = match call.url.port() {
                Some(port) => format!("{}:{port}", host(&call.url)),
                None => host(&call.url).to_owned(),
            };
            let _ = context.progress().report(&subject, None, None);
            let deadline = Instant::now()
                + Duration::from_secs(u64::from(self.fetcher.limits.timeout_seconds));
            let fetched = tokio::select! {
                biased;
                () = context.cancellation().cancelled() => return Err(cancelled()),
                fetched = self.fetcher.fetch(&call, deadline) => fetched,
            };
            let fetched = match fetched {
                Ok(fetched) => fetched,
                Err(error) => {
                    return Ok(ToolOutcome::failure_text(error.message())?
                        .with_presentation_subject(subject));
                }
            };
            let sessions = context.environment().existing_joined_process_sessions();
            let rendered = tokio::select! {
                biased;
                () = context.cancellation().cancelled() => return Err(cancelled()),
                rendered = render_off_thread(fetched, sessions, deadline, self.fetcher.limits) => rendered,
            }?;
            Ok(rendered.with_presentation_subject(subject))
        })
    }
}

fn cancelled() -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::Cancelled, "fetch_url was cancelled")
}

/// Converts and renders on a dedicated thread, bounded by the call's
/// deadline. Spilling walks the jobs root, so it runs there too.
async fn render_off_thread(
    fetched: Fetched,
    sessions: Option<plakous_tools::JoinedProcessSessions>,
    deadline: Instant,
    limits: FetchUrlLimits,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(String::from("fetch_url-render"))
        .stack_size(RENDER_STACK_BYTES)
        .spawn(move || {
            let mut spill = |bytes: &[u8]| {
                sessions
                    .as_ref()
                    .and_then(|sessions| sessions.spill_output(bytes))
                    .map(|file| file.path)
            };
            let _ = sender.send(content::render(&fetched, &mut spill));
        })
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "fetch_url could not start rendering",
                &error,
            )
        })?;
    match tokio::time::timeout_at(deadline.into(), receiver).await {
        Ok(Ok(rendered)) => rendered,
        Ok(Err(_)) => Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "fetch_url rendering did not finish",
        )),
        Err(_) => ToolOutcome::failure_text(
            FetchError::Timeout {
                seconds: limits.timeout_seconds,
            }
            .message(),
        ),
    }
}

#[cfg(test)]
mod tests;
