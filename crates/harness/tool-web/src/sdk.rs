//! The SDK-only registrations of the neutral Python SDK's optional
//! namespaces (spec 01 §9.8.3): `web_search`, `web_fetch`, `x_search`,
//! `image_gen` and `fetch_url`, each under its canonical name and a
//! presentation-independent schema, nested-eligible and present only in that
//! composition's catalog. The per-presentation packs are unchanged.
//!
//! A helper wrapper (every retrieval and `image_gen`) carries its port's
//! declared most physical requests per call, which the nested-call choke
//! point reserves (§9.8.7) and which the binding identity binds; a port that
//! declares no bound cannot back one. `fetch_url` is a local fetch, not a
//! helper request.

use std::sync::Arc;

use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    PYTHON_HARNESS_SDK_WEB_FETCH_V1, PYTHON_HARNESS_SDK_WEB_SEARCH_V1,
    PYTHON_HARNESS_SDK_X_SEARCH_V1, PythonHarnessSdkOptionalToolV1, RegisteredToolId,
    RetrievalPort, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId, ToolOwnerClass,
    ToolRegistration, ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind, object_schema,
};
use serde_json::{Value, json};

use crate::text::{
    PLAKOUS_WEB_CLAUDE_FETCH_DESCRIPTION, PLAKOUS_WEB_SEARCH_DESCRIPTION,
    PLAKOUS_WEB_X_SEARCH_DESCRIPTION,
};
use crate::{
    ISSUER, MAX_DOMAINS, MAX_PROMPT_BYTES, MAX_QUERY_BYTES, MAX_URL_BYTES, RetrievalOptions,
    RetrievalTool, SEMANTIC_VERSION, WebTool, registration_error,
};

/// The registrations one SDK node adds for its optional namespaces, in a
/// fixed order: retrieval wrappers, `image_gen`, `fetch_url`, then MCP tools.
#[derive(Default)]
pub struct PythonHarnessSdkOptionalTools {
    registrations: Vec<ToolRegistration>,
}

impl PythonHarnessSdkOptionalTools {
    /// No optional namespace.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `web_search`, `web_fetch` and `x_search` for each retrieval
    /// `port` serves, with `options` fixed on every call.
    ///
    /// # Errors
    ///
    /// Refuses a served kind the port declares no request bound for.
    pub fn retrieval(
        mut self,
        port: &Arc<dyn RetrievalPort>,
        options: &RetrievalOptions,
    ) -> Result<Self, ToolRuntimeError> {
        for tool in [WebTool::WebSearch, WebTool::WebFetch, WebTool::XSearch] {
            if port.kinds().contains(&tool.kind()) {
                self.registrations
                    .push(retrieval_registration(tool, port, options)?);
            }
        }
        Ok(self)
    }

    /// Adds `image_gen` over `port`.
    ///
    /// # Errors
    ///
    /// Refuses a port that declares no request bound.
    #[cfg(feature = "image-gen")]
    pub fn image_gen(
        mut self,
        port: Arc<dyn crate::ImageGenPort>,
    ) -> Result<Self, ToolRuntimeError> {
        self.registrations
            .push(crate::ImageGenPack::new(port).sdk_registration()?);
        Ok(self)
    }

    /// Adds `fetch_url` with `pack`'s bounds and client.
    ///
    /// # Errors
    ///
    /// Returns a registration construction error.
    #[cfg(feature = "fetch")]
    pub fn fetch_url(mut self, pack: &crate::FetchUrlPack) -> Result<Self, ToolRuntimeError> {
        self.registrations.push(pack.sdk_registration()?);
        Ok(self)
    }

    /// Adds MCP tools as `mcp.<server>.<tool>` functions: each registration
    /// must be MCP-owned, and becomes nested-eligible here only.
    ///
    /// # Errors
    ///
    /// Refuses a registration no MCP server owns.
    pub fn mcp(
        mut self,
        registrations: impl IntoIterator<Item = ToolRegistration>,
    ) -> Result<Self, ToolRuntimeError> {
        for registration in registrations {
            if !matches!(registration.owner(), ToolOwnerClass::Mcp(_)) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "an SDK MCP function must be owned by an MCP server",
                ));
            }
            self.registrations.push(registration.nested_eligible());
        }
        Ok(self)
    }

    /// The registrations, in order.
    #[must_use]
    pub fn registrations(&self) -> &[ToolRegistration] {
        &self.registrations
    }

    /// Registers every optional capability into `registry`.
    ///
    /// # Errors
    ///
    /// Returns the registry's collision or capacity error.
    pub fn register_into(self, registry: &mut ToolRegistryBuilder) -> Result<(), ToolRuntimeError> {
        self.registrations
            .into_iter()
            .try_for_each(|registration| registry.register(registration))
    }
}

/// One SDK registration: canonical name, SDK tool id, and a binding identity
/// that names the declared helper maximum when there is one.
pub(crate) fn sdk_registration(
    tool: PythonHarnessSdkOptionalToolV1,
    definition: (&'static str, Value, ToolConcurrency, ToolEffect),
    helper_requests: Option<u32>,
    executor: Arc<dyn ToolExecutor>,
) -> Result<ToolRegistration, ToolRuntimeError> {
    let (description, schema, concurrency, effect) = definition;
    let definition = ToolDefinition::new(
        ToolName::new(tool.name).map_err(registration_error("SDK tool name is invalid"))?,
        SEMANTIC_VERSION,
        description,
        ToolInputContract::structured(schema)
            .map_err(registration_error("SDK tool schema is invalid"))?,
        ToolOutputContract::portable_model_content(),
        concurrency,
        effect,
    )
    .map_err(registration_error("SDK tool definition is invalid"))?;
    let maximum = helper_requests.map_or_else(String::new, |max| max.to_string());
    let registration = ToolRegistration::new(
        RegisteredToolId::new(tool.tool_id)
            .map_err(registration_error("SDK tool id is invalid"))?,
        ToolExecutorId::new(format!("{}.executor", tool.tool_id))
            .map_err(registration_error("SDK executor id is invalid"))?,
        ToolExecutorBindingIdentity::new(
            1,
            &[ISSUER, b"sdk", tool.name.as_bytes(), maximum.as_bytes()],
        )
        .map_err(registration_error("SDK binding identity is invalid"))?,
        definition,
        executor,
    )
    .nested_eligible();
    Ok(match helper_requests {
        Some(max) => registration.helper_requests(max),
        None => registration,
    })
}

/// The declared bound, or the refusal for a port that declares none.
pub(crate) fn declared(max: Option<u32>, name: &str) -> Result<u32, ToolRuntimeError> {
    max.ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::UnsupportedCapability,
            format!("{name} has no declared request bound on this route"),
        )
    })
}

fn retrieval_registration(
    tool: WebTool,
    port: &Arc<dyn RetrievalPort>,
    options: &RetrievalOptions,
) -> Result<ToolRegistration, ToolRuntimeError> {
    let (sdk, description, schema) = match tool {
        WebTool::WebSearch => (
            PYTHON_HARNESS_SDK_WEB_SEARCH_V1,
            PLAKOUS_WEB_SEARCH_DESCRIPTION,
            object_schema(
                json!({
                    "query": {"type": "string", "minLength": 2, "maxLength": MAX_QUERY_BYTES},
                    "allowed_domains": crate::search::domains_schema(),
                    "blocked_domains": crate::search::domains_schema()
                }),
                &["query"],
            ),
        ),
        WebTool::WebFetch => (
            PYTHON_HARNESS_SDK_WEB_FETCH_V1,
            PLAKOUS_WEB_CLAUDE_FETCH_DESCRIPTION,
            object_schema(
                json!({
                    "url": {"type": "string", "minLength": 8, "maxLength": MAX_URL_BYTES},
                    "prompt": {"type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES}
                }),
                &["url", "prompt"],
            ),
        ),
        WebTool::XSearch => (
            PYTHON_HARNESS_SDK_X_SEARCH_V1,
            PLAKOUS_WEB_X_SEARCH_DESCRIPTION,
            object_schema(
                json!({
                    "query": {"type": "string", "minLength": 2, "maxLength": MAX_QUERY_BYTES},
                    "handles": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": 15},
                        "maxItems": MAX_DOMAINS
                    },
                    "from_date": {"type": "string", "minLength": 10, "maxLength": 10},
                    "to_date": {"type": "string", "minLength": 10, "maxLength": 10}
                }),
                &["query"],
            ),
        ),
    };
    let max = declared(port.max_requests(tool.kind()), sdk.name)?;
    sdk_registration(
        sdk,
        (
            description,
            schema,
            ToolConcurrency::Parallel,
            ToolEffect::ReadOnly,
        ),
        Some(max),
        Arc::new(RetrievalTool {
            tool,
            presented: sdk.name,
            port: Arc::clone(port),
            options: options.clone(),
        }),
    )
}
